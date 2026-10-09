//! Steam 凭据备份：本地持久队列、SFTP 合并、读回确认与可恢复重试。
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use russh::client;
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use russh_sftp::client::{error::Error as SftpError, RawSftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Emitter;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use uuid::Uuid;

const MAX_REMOTE_BYTES: usize = 8 * 1024 * 1024;
const CHUNK: usize = 16 * 1024;
const REQUEST_TIMEOUT: u64 = 10;
static STORE_LOCK: Mutex<()> = Mutex::new(());
static SYNC_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
static TESTED_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static CLIENT_ID: LazyLock<String> = LazyLock::new(|| {
    let machine: String = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Microsoft\\Cryptography")
        .and_then(|k| k.get_value("MachineGuid")).unwrap_or_default();
    let identity = format!("{}|{}|{}|{}", machine,
        std::env::var("COMPUTERNAME").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default(), crate::data_dir().display());
    Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
});

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub remote_path: String,
    pub fingerprint: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: false, host: String::new(), port: 2022,
            username: String::new(), password: String::new(),
            remote_path: String::new(), fingerprint: String::new() }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    account: String,
    password: String,
    source: String,
    ts_ms: i64,
    #[serde(default)]
    manual: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    config: Config,
    verified: bool,
    credentials_seeded: bool,
    confirmed_credentials: BTreeMap<String, String>,
    synced_credentials: BTreeMap<String, String>,
    revision: u64,
    state_version: u64,
    client_id: String,
    queue: Vec<Pending>,
    status: String,
    message: String,
    last_success_ms: Option<i64>,
    failures: u32,
    blocked: bool,
    next_retry_ms: i64,
}

impl Default for Store {
    fn default() -> Self {
        Self { config: Config::default(), verified: false, credentials_seeded: false, confirmed_credentials: BTreeMap::new(), synced_credentials: BTreeMap::new(), revision: 0, state_version: 0, client_id: Uuid::new_v4().to_string(),
            queue: Vec::new(), status: "disabled".into(), message: "自动备份未开启".into(),
            last_success_ms: None, failures: 0, blocked: false, next_retry_ms: 0 }
    }
}

#[derive(Clone, Serialize)]
pub struct QueueItem {
    pub account: String,
    pub source: String,
    pub ts_ms: i64,
    pub login_confirmed: bool,
}

#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub state_version: u64,
    pub config: Config,
    pub config_verified: bool,
    pub missing_password_accounts: Vec<String>,
    pub unconfirmed_password_accounts: Vec<String>,
    pub inventory_error: Option<String>,
    pub queue: Vec<QueueItem>,
    pub status: String,
    pub message: String,
    pub last_success_ms: Option<i64>,
    pub next_retry_ms: i64,
}

impl Store {
    fn snapshot(&self) -> Snapshot {
        self.snapshot_from_inventory(inventory(&self.confirmed_credentials))
    }

    fn snapshot_from_inventory(&self, inventory: Result<Inventory>) -> Snapshot {
        let mut queue: Vec<_> = self.queue.iter().filter(|p| upload_allowed(self, p)).map(|p| QueueItem {
            account: p.account.clone(), source: p.source.clone(), ts_ms: p.ts_ms,
            login_confirmed: credential_confirmed(&self.confirmed_credentials, &p.account, &p.password),
        }).collect();
        let (missing_password_accounts, unconfirmed_password_accounts, inventory_error) = match inventory {
            Ok(inventory) => {
                let mut unconfirmed = Vec::new();
                for (account, password) in &inventory.available {
                    if credential_confirmed(&self.synced_credentials, account, password)
                        || queue.iter().any(|p| p.account.eq_ignore_ascii_case(account)) { continue; }
                    if credential_confirmed(&self.confirmed_credentials, account, password) {
                        queue.push(QueueItem { account: account.clone(), source: "已确认登录".into(), ts_ms: 0, login_confirmed: true });
                    } else { unconfirmed.push(account.clone()); }
                }
                let missing = inventory.missing.into_iter()
                    .filter(|a| !queue.iter().any(|p| p.account.eq_ignore_ascii_case(a))).collect();
                (missing, unconfirmed, None)
            }
            Err(e) => (Vec::new(), Vec::new(), Some(e.message)),
        };
        Snapshot { state_version: self.state_version, config: self.config.clone(), config_verified: self.verified && validate_config(&self.config).is_ok(),
            missing_password_accounts, unconfirmed_password_accounts, inventory_error,
            queue, status: self.status.clone(), message: self.message.clone(),
            last_success_ms: self.last_success_ms, next_retry_ms: self.next_retry_ms }
    }

    fn enqueue(&mut self, account: &str, password: &str, source: &str) {
        self.queue.retain(|p| !p.account.eq_ignore_ascii_case(account));
        self.queue.push(Pending { id: Uuid::new_v4().to_string(), account: account.into(),
            password: password.into(), source: source.into(), ts_ms: now(), manual: false });
    }

    fn acknowledge(&mut self, sent: &[Pending]) {
        // 上传期间收到的同账号新密码具有新 id，不能被旧批次清掉。
        self.queue.retain(|p| !sent.iter().any(|s| s.id == p.id));
        for p in sent {
            self.synced_credentials.insert(p.account.trim().to_ascii_lowercase(), credential_digest(&p.account, &p.password));
        }
    }

    fn confirm_success(&mut self, account: &str, password: &str, source: &str) -> Result<()> {
        if !valid_credential(account, password) {
            return Err(error("format_error", "账号或密码为空或含换行，未加入备份队列", false));
        }
        self.confirmed_credentials.insert(account.trim().to_ascii_lowercase(), credential_digest(account, password));
        if self.config.enabled && !credential_confirmed(&self.synced_credentials, account, password) {
            self.enqueue(account.trim(), password, source);
            if !self.blocked && self.status != "syncing" {
                self.status = "pending".into();
                self.message = "登录已确认，账号已进入本地待同步队列".into();
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    retryable: bool,
}

type Result<T> = std::result::Result<T, Error>;

fn error(code: &str, message: &str, retryable: bool) -> Error {
    Error { code: code.into(), message: message.into(), retryable }
}

fn now() -> i64 { chrono::Utc::now().timestamp_millis() }

fn local_error(context: &str, e: std::io::Error) -> Error {
    error("local_error", &format!("{context}：{e}；待同步记录不会被清空"), false)
}

fn read_store(path: &Path) -> Result<Store> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| error("local_error",
            "Data\\backup.json 格式损坏，请修复或还原该文件；原文件已保留", false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(local_error("读取备份配置失败", e)),
    }
}

fn write_store(path: &Path, store: &Store) -> Result<()> {
    let text = serde_json::to_vec_pretty(store)
        .map_err(|_| error("local_error", "备份数据序列化失败", false))?;
    crate::atomic_write(path, &text, true).map_err(|e| local_error("保存备份数据失败", e))
}

fn edit<T>(f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = crate::data_dir().join("backup.json");
    let mut store = read_store(&path)?;
    store.client_id = CLIENT_ID.clone();
    store.state_version = store.state_version.saturating_add(1);
    let value = f(&mut store)?;
    write_store(&path, &store)?;
    publish(&store);
    Ok(value)
}

fn load() -> Result<Store> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = read_store(&crate::data_dir().join("backup.json"))?;
    // 同一 Data 配置分发到不同电脑时，仍使用各设备独立的远程锁身份。
    store.client_id = CLIENT_ID.clone();
    Ok(store)
}

fn publish(store: &Store) {
    if let Some(app) = APP.get() {
        let mut snapshot = store.snapshot();
        snapshot.config.password.clear();
        let _ = app.emit("backup-changed", snapshot);
    }
}

pub fn get() -> Result<Snapshot> { Ok(load()?.snapshot()) }

pub fn credentials_changed() -> Result<()> { edit(|_| Ok(())) }

fn same_connection(a: &Config, b: &Config) -> bool {
    a.host == b.host && a.port == b.port && a.username == b.username
        && a.password == b.password && a.remote_path == b.remote_path
}

fn validate_config(config: &Config) -> Result<()> {
    if config.host.is_empty() || config.host.contains(['/', '\\', '@']) || config.host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(error("config_error", "请输入服务器域名或 IP，不要包含协议或路径", false));
    }
    if config.port == 0 || config.username.is_empty() || config.username.chars().any(char::is_control) || config.password.is_empty() {
        return Err(error("config_error", "请填写有效端口、NAS 用户名和密码", false));
    }
    if !config.remote_path.starts_with('/') || config.remote_path.ends_with('/')
        || config.remote_path.contains(['\r', '\n', '\0', '\\'])
        || config.remote_path.split('/').any(|p| p == "..") {
        return Err(error("config_error", "远程文件请填写 SFTP 绝对路径，例如 /GameReady/accounts.txt", false));
    }
    Ok(())
}

pub fn save(mut config: Config) -> Result<Snapshot> {
    config.host = config.host.trim().to_string();
    config.username = config.username.trim().to_string();
    config.remote_path = config.remote_path.trim().to_string();
    if config.enabled { validate_config(&config)?; }
    let tested = TESTED_CONFIG.lock().unwrap_or_else(|p| p.into_inner()).take();
    let snapshot = edit(|s| {
        // 同一服务器已经记录的密钥不能被空值或旧的前端快照清掉。
        if config.host == s.config.host && config.port == s.config.port {
            if !s.config.fingerprint.is_empty() { config.fingerprint = s.config.fingerprint.clone(); }
        } else {
            config.fingerprint = tested.as_ref()
                .filter(|c| c.host == config.host && c.port == config.port)
                .map(|c| c.fingerprint.clone()).unwrap_or_default();
        }
        s.verified = (s.verified && same_connection(&s.config, &config))
            || tested.as_ref().is_some_and(|c| same_connection(c, &config));
        if !same_connection(&s.config, &config) {
            s.synced_credentials.clear();
            s.credentials_seeded = false;
        }
        s.config = config;
        prune_unconfirmed(s);
        let inventory = if s.config.enabled { Some(inventory(&s.confirmed_credentials)?) } else { None };
        if let Some(inventory) = &inventory {
            seed_inventory(s, inventory, "历史账号补传");
        }
        s.revision = s.revision.saturating_add(1);
        s.failures = 0;
        s.blocked = false;
        s.next_retry_ms = 0;
        s.status = if !s.config.enabled { "disabled" } else if s.queue.is_empty() { "idle" } else { "pending" }.into();
        s.message = if s.config.enabled {
            format!("配置已保存，{} 个账号待同步；{} 个账号待验证登录；{} 个账号需补输密码", s.queue.len(), inventory.as_ref().map_or(0, |i| i.unconfirmed.len()), inventory.as_ref().map_or(0, |i| i.missing.len()))
        } else { "自动备份已关闭，已有队列保留".into() };
        Ok(s.snapshot())
    })?;
    WAKE.notify_one();
    Ok(snapshot)
}

fn valid_credential(account: &str, password: &str) -> bool {
    !account.is_empty() && !password.is_empty() && !account.contains("----")
        && !account.contains(['\r', '\n', '\0']) && !password.contains(['\r', '\n', '\0'])
}

struct Inventory {
    available: Vec<(String, String)>,
    credentials: Vec<(String, String)>,
    missing: Vec<String>,
    unconfirmed: Vec<String>,
}

fn credential_digest(account: &str, password: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(account.trim().to_ascii_lowercase().as_bytes());
    digest.update([0]);
    digest.update(password.as_bytes());
    format!("{:x}", digest.finalize())
}

fn credential_confirmed(confirmed: &BTreeMap<String, String>, account: &str, password: &str) -> bool {
    confirmed.get(&account.trim().to_ascii_lowercase()) == Some(&credential_digest(account, password))
}

fn prune_unconfirmed(store: &mut Store) {
    let confirmed = &store.confirmed_credentials;
    store.queue.retain(|p| valid_credential(&p.account, &p.password)
        && (p.manual || credential_confirmed(confirmed, &p.account, &p.password)));
}

fn upload_allowed(store: &Store, pending: &Pending) -> bool {
    valid_credential(&pending.account, &pending.password)
        && (pending.manual || credential_confirmed(&store.confirmed_credentials, &pending.account, &pending.password))
}

pub fn is_login_confirmed(account: &str, password: &str) -> bool {
    load().map(|s| credential_confirmed(&s.confirmed_credentials, account, password)).unwrap_or(false)
}

pub fn login_attempt_started(account: &str) -> Result<()> {
    edit(|s| {
        forget_confirmation(s, account);
        Ok(())
    })
}

fn forget_confirmation(store: &mut Store, account: &str) {
    store.confirmed_credentials.remove(&account.trim().to_ascii_lowercase());
    store.queue.retain(|p| !p.account.eq_ignore_ascii_case(account.trim()));
}

fn classify_inventory(creds: &BTreeMap<String, String>, history: &[String], confirmed: &BTreeMap<String, String>) -> Inventory {
    let mut grouped = BTreeMap::<String, Option<String>>::new();
    for (account, password) in creds {
        let account = account.trim().to_ascii_lowercase();
        match grouped.entry(account.clone()) {
            std::collections::btree_map::Entry::Occupied(mut entry) => { entry.insert(None); }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(valid_credential(&account, password).then(|| password.clone()));
            }
        }
    }
    for account in history {
        grouped.entry(account.trim().to_ascii_lowercase()).or_default();
    }
    let mut inventory = Inventory { available: Vec::new(), credentials: Vec::new(), missing: Vec::new(), unconfirmed: Vec::new() };
    for (account, password) in grouped {
        if account.is_empty() { continue; }
        match password {
            Some(password) => {
                inventory.available.push((account.clone(), password.clone()));
                if credential_confirmed(confirmed, &account, &password) { inventory.credentials.push((account, password)); }
                else { inventory.unconfirmed.push(account); }
            }
            None => inventory.missing.push(account),
        }
    }
    inventory
}

fn inventory(confirmed: &BTreeMap<String, String>) -> Result<Inventory> {
    let creds = crate::read_creds(&crate::creds_path())
        .map_err(|message| error("local_error", &message, false))?;
    let history: Vec<_> = crate::accounts_list_v().into_iter().map(|a| a.account_name).collect();
    Ok(classify_inventory(&creds, &history, confirmed))
}

fn seed_inventory(store: &mut Store, inventory: &Inventory, source: &str) {
    for (account, password) in &inventory.credentials {
        // 已有在途队列可能比历史文件更新，补传不能覆盖它。
        if credential_confirmed(&store.confirmed_credentials, account, password)
            && !credential_confirmed(&store.synced_credentials, account, password)
            && !store.queue.iter().any(|p| p.account.eq_ignore_ascii_case(account)) {
            store.enqueue(account, password, source);
        }
    }
    store.credentials_seeded = true;
}

// 手动上传明确选择的本地密码，不创建 Steam 登录成功证明。
fn prepare_manual(store: &mut Store, inventory: &Inventory, target: Option<&str>) -> Result<()> {
    if let Some(account) = target {
        if !inventory.available.iter().any(|(a, _)| a.eq_ignore_ascii_case(account))
            && !store.queue.iter().any(|p| p.account.eq_ignore_ascii_case(account)) {
            return Err(error("missing_password", "该账号没有可用密码，请先补充密码", false));
        }
    }
    for (account, password) in &inventory.available {
        if target.is_some_and(|a| !a.eq_ignore_ascii_case(account))
            || credential_confirmed(&store.synced_credentials, account, password) { continue; }
        if !store.queue.iter().any(|p| p.account.eq_ignore_ascii_case(account)) {
            store.enqueue(account, password, "手动同步");
        }
    }
    for pending in &mut store.queue {
        if target.is_none_or(|a| a.eq_ignore_ascii_case(&pending.account)) { pending.manual = true; }
    }
    Ok(())
}

pub fn save_password(account: &str, password: &str) -> Result<Snapshot> {
    let account = account.trim();
    if !valid_credential(account, password) {
        return Err(error("format_error", "请输入有效账号和密码，不能含换行或 NUL", false));
    }
    // 不等待正在进行的登录，也不启动、关闭或切换 Steam。
    let _steam_op = crate::STEAM_OP.try_lock()
        .map_err(|_| error("busy", "Steam 账号操作进行中，请完成后补充密码", false))?;
    crate::save_cred_by_name(account, password).map_err(|e| error("local_error", &e, false))?;
    let snapshot = edit(|s| {
        s.message = format!("{account}：密码已补充，可点击同步");
        Ok(s.snapshot())
    })?;
    if let Some(app) = APP.get() { let _ = app.emit("steam-accounts-changed", ()); }
    Ok(snapshot)
}

pub fn confirmed_login(account: &str, password: &str, source: &str) {
    let account = account.trim();
    let result = (|| -> Result<()> {
        edit(|s| s.confirm_success(account, password, source))?;
        WAKE.notify_one();
        Ok(())
    })();
    if let Err(e) = result { crate::log("acct", "Steam 备份排队失败", &e.message, false); }
    if let Some(app) = APP.get() { let _ = app.emit("steam-accounts-changed", ()); }
}

pub fn missing_password() {
    if load().map(|s| s.config.enabled).unwrap_or(false) {
        let _ = edit(|s| {
            if s.config.enabled { s.message = "该账号缺少本地密码，未加入上传队列；请在待同步页补输密码".into(); }
            Ok(())
        });
        crate::log("acct", "Steam 账号未备份", "本机没有保存该账号密码，请在待同步页补充密码", false);
    }
}

// 远程原有重复行取最后一行，随后用当前批次覆盖；任何坏行都停止写入。
fn merge_remote(bytes: &[u8], pending: &[Pending]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).map_err(|_| error("format_error",
        "NAS 账号文件不是 UTF-8 文本，原文件未修改", false))?;
    let mut accounts = BTreeMap::<String, (String, String)>::new();
    for (line_no, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.is_empty() { continue; }
        let (account, password) = line.split_once("----").ok_or_else(|| error("format_error",
            &format!("NAS 账号文件第 {} 行格式错误，原文件未修改", line_no + 1), false))?;
        let account = account.trim();
        if !valid_credential(account, password) {
            return Err(error("format_error", &format!("NAS 账号文件第 {} 行不完整，原文件未修改", line_no + 1), false));
        }
        accounts.insert(account.to_ascii_lowercase(), (account.into(), password.into()));
    }
    for p in pending {
        if !valid_credential(&p.account, &p.password) {
            return Err(error("format_error", "本地队列中有无效凭据，队列和远程文件均已保留", false));
        }
        accounts.insert(p.account.to_ascii_lowercase(), (p.account.clone(), p.password.clone()));
    }
    let mut output = String::new();
    for (_, (account, password)) in accounts { output.push_str(&format!("{account}----{password}\n")); }
    if output.len() > MAX_REMOTE_BYTES { return Err(error("format_error", "合并后的账号文件超过 8 MiB，未写入", false)); }
    Ok(output.into_bytes())
}

fn sftp_error(stage: &str, e: SftpError) -> Error {
    match e {
        SftpError::Status(s) if s.status_code == StatusCode::PermissionDenied =>
            error("permission_error", &format!("{stage}：NAS 账号没有读写权限，请使用个人空间中的可写目录"), false),
        SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile =>
            error("path_error", &format!("{stage}：远程目录不存在，请先在 NAS 创建目标目录"), false),
        SftpError::Status(s) if s.status_code == StatusCode::OpUnsupported =>
            error("unsupported", &format!("{stage}：NAS 不支持所需 SFTP 操作，原账号文件未清空"), false),
        SftpError::Status(s) if s.status_code == StatusCode::Failure =>
            error("remote_error", &format!("{stage}：NAS 拒绝该操作，请测试目标目录的写入与文件替换权限"), false),
        _ => error("network_error", &format!("{stage}：连接中断或响应超时，记录保留，稍后自动重试"), true),
    }
}

fn is_status(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == code)
}

struct HostCheck {
    expected: String,
    seen: Arc<Mutex<String>>,
    mismatch: Arc<std::sync::atomic::AtomicBool>,
}

impl client::Handler for HostCheck {
    type Error = russh::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = fingerprint.clone();
        let accepted = self.expected.is_empty() || self.expected == fingerprint;
        self.mismatch.store(!accepted, std::sync::atomic::Ordering::Relaxed);
        Ok(accepted)
    }
}

struct Remote {
    ssh: client::Handle<HostCheck>,
    sftp: RawSftpSession,
    posix_rename: bool,
    fsync: bool,
    fingerprint: String,
}

async fn timed<T>(future: impl std::future::Future<Output = std::result::Result<T, russh::Error>>, stage: &str) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(15), future).await
        .map_err(|_| error("network_error", &format!("{stage}超时，稍后自动重试"), true))?
        .map_err(|_| error("network_error", &format!("{stage}失败，请检查域名、公网端口和 SFTP 服务"), true))
}

impl Remote {
    async fn connect(config: &Config) -> Result<Self> {
        validate_config(config)?;
        let seen = Arc::new(Mutex::new(String::new()));
        let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = HostCheck { expected: config.fingerprint.clone(), seen: seen.clone(), mismatch: mismatch.clone() };
        let ssh_config = client::Config { inactivity_timeout: Some(Duration::from_secs(35)), ..Default::default() };
        let connected = timed(client::connect(Arc::new(ssh_config), (config.host.as_str(), config.port), handler), "SSH 连接").await;
        if mismatch.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(error("host_key_error", "NAS 主机密钥与已保存记录不一致，连接已停止；请核对服务器，确认更换后关闭程序并更新 Data\\backup.json 中的 fingerprint", false));
        }
        let mut ssh = connected?;
        if !timed(ssh.authenticate_password(&config.username, &config.password), "NAS 登录").await?.success() {
            return Err(error("auth_error", "NAS 用户名或密码错误，自动重试已暂停；修正并保存配置后重试", false));
        }
        let channel = timed(ssh.channel_open_session(), "打开 SSH 通道").await?;
        timed(channel.request_subsystem(true, "sftp"), "启动 SFTP").await?;
        let sftp = RawSftpSession::new(channel.into_stream());
        sftp.set_timeout(REQUEST_TIMEOUT);
        let version = sftp.init().await.map_err(|e| sftp_error("初始化 SFTP", e))?;
        let fingerprint = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Ok(Self { ssh, sftp, fingerprint,
            posix_rename: version.extensions.get("posix-rename@openssh.com").is_some_and(|v| v == "1"),
            fsync: version.extensions.get("fsync@openssh.com").is_some_and(|v| v == "1") })
    }

    async fn close(&self) {
        let _ = self.sftp.close_session();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.ssh.disconnect(russh::Disconnect::ByApplication, "", "")).await;
    }

    async fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let handle = match self.sftp.open(path, OpenFlags::READ, FileAttributes::default()).await {
            Ok(h) => h.handle,
            Err(e) if is_status(&e, StatusCode::NoSuchFile) => return Ok(None),
            Err(e) => return Err(sftp_error("读取远程文件", e)),
        };
        let result = async {
            let expected_size = self.sftp.fstat(&handle).await.ok().and_then(|a| a.attrs.size);
            if expected_size.is_some_and(|size| size > MAX_REMOTE_BYTES as u64) {
                return Err(error("format_error", "远程文件超过 8 MiB，原文件未修改", false));
            }
            let mut bytes = Vec::new();
            loop {
                match self.sftp.read(&handle, bytes.len() as u64, CHUNK as u32).await {
                    Ok(data) if data.data.is_empty() => {
                        if expected_size == Some(bytes.len() as u64) { break; }
                        return Err(error("verify_error", "远程文件读取不完整，原文件未修改，稍后重试", true));
                    },
                    Ok(data) => {
                        bytes.extend_from_slice(&data.data);
                        if bytes.len() > MAX_REMOTE_BYTES { return Err(error("format_error", "远程文件超过 8 MiB，原文件未修改", false)); }
                    }
                    Err(e) if is_status(&e, StatusCode::Eof) => break,
                    Err(e) => return Err(sftp_error("读取远程文件", e)),
                }
            }
            if expected_size.is_some_and(|size| size != bytes.len() as u64) {
                return Err(error("verify_error", "远程文件读取长度不一致，原文件未修改，稍后重试", true));
            }
            Ok(Some(bytes))
        }.await;
        let closed = self.sftp.close(handle).await.map_err(|e| sftp_error("关闭远程读取", e));
        let bytes = result?;
        closed?;
        Ok(bytes)
    }

    async fn write(&self, path: &str, bytes: &[u8], exclusive: bool) -> Result<()> {
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | if exclusive { OpenFlags::EXCLUDE } else { OpenFlags::TRUNCATE };
        let handle = self.sftp.open(path, flags, FileAttributes::default()).await
            .map_err(|e| sftp_error("创建远程临时文件", e))?.handle;
        let result = async {
            for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
                self.sftp.write(&handle, (i * CHUNK) as u64, chunk.to_vec()).await
                    .map_err(|e| sftp_error("写入远程临时文件", e))?;
            }
            if self.fsync { self.sftp.fsync(&handle).await.map_err(|e| sftp_error("刷新远程文件", e))?; }
            Ok(())
        }.await;
        let closed = self.sftp.close(handle).await.map_err(|e| sftp_error("确认远程写入", e));
        result?;
        closed?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        match self.sftp.remove(path).await {
            Ok(_) => Ok(()),
            Err(e) if is_status(&e, StatusCode::NoSuchFile) => Ok(()),
            Err(e) => Err(sftp_error("清理远程临时文件", e)),
        }
    }

    async fn replace(&self, source: &str, target: &str) -> Result<()> {
        if self.posix_rename {
            let mut data = Vec::new();
            for path in [source, target] {
                data.extend_from_slice(&(path.len() as u32).to_be_bytes());
                data.extend_from_slice(path.as_bytes());
            }
            match self.sftp.extended("posix-rename@openssh.com", data).await {
                Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => return Ok(()),
                Ok(Packet::Status(s)) => return Err(sftp_error("替换远程账号文件", SftpError::Status(s))),
                Ok(_) => return Err(error("remote_error", "NAS 文件替换返回异常响应，待同步记录已保留", false)),
                Err(e) => return Err(sftp_error("替换远程账号文件", e)),
            }
        }
        // 不使用“删除原文件后重命名”的回退，失败时保留完整原文件。
        self.sftp.rename(source, target).await.map_err(|e| sftp_error("替换远程账号文件", e))?;
        Ok(())
    }

    async fn lock_files(&self, lock: &str) -> Result<Vec<russh_sftp::protocol::File>> {
        let handle = self.sftp.opendir(lock).await.map_err(|e| sftp_error("检查远程写锁", e))?.handle;
        let result = async {
            let mut files = Vec::new();
            loop {
                match self.sftp.readdir(&handle).await {
                    Ok(names) => {
                        if names.files.is_empty() { break; }
                        files.extend(names.files.into_iter().filter(|f| f.filename != "." && f.filename != ".."));
                        if files.len() > 16 { return Err(error("remote_error", "远程锁目录内容异常，请检查 NAS", false)); }
                    },
                    Err(e) if is_status(&e, StatusCode::Eof) => break,
                    Err(e) => return Err(sftp_error("检查远程写锁", e)),
                }
            }
            Ok(files)
        }.await;
        let _ = self.sftp.close(handle).await;
        result
    }

    async fn acquire(&self, lock: &str, client_id: &str) -> Result<String> {
        if let Err(original) = self.sftp.mkdir(lock, FileAttributes::default()).await {
            let attrs = self.sftp.stat(lock).await.map_err(|_| sftp_error("取得远程写锁", original))?;
            let files = self.lock_files(lock).await?;
            let known = files.iter().all(|f| !f.filename.contains(['/', '\\']) &&
                (f.filename.starts_with("owner-") || f.filename.starts_with("next-") || f.filename == "next.txt"));
            let own = !files.is_empty() && files.iter().filter(|f| f.filename.starts_with("owner-"))
                .any(|f| f.filename.starts_with(&format!("owner-{client_id}")));
            // 单次提交最多 60 秒；跨设备异常退出的锁等待 10 分钟再恢复。
            let stale = attrs.attrs.mtime.is_some_and(|t| now() / 1000 - i64::from(t) > 600);
            if !known || (!own && !stale) {
                return Err(error("remote_busy", "NAS 文件正被其他设备同步，记录保留，稍后自动重试", true));
            }
            // 先原子移走整个目录。旧批次的唯一临时路径失效，无法覆盖新批次。
            let abandoned = format!("{lock}.abandoned-{}", Uuid::new_v4());
            self.sftp.rename(lock, &abandoned).await.map_err(|_| error("remote_busy", "远程锁已变化，稍后重试", true))?;
            for f in files { self.remove(&format!("{abandoned}/{}", f.filename)).await?; }
            self.sftp.rmdir(abandoned).await.map_err(|e| sftp_error("清理中断的写锁", e))?;
            self.sftp.mkdir(lock, FileAttributes::default()).await.map_err(|_| error("remote_busy", "其他设备已取得远程写锁，稍后重试", true))?;
        }
        let owner = format!("{client_id}-{}", Uuid::new_v4());
        self.write(&format!("{lock}/owner-{owner}"), b"", true).await?;
        Ok(owner)
    }

    async fn release(&self, lock: &str, owner: &str) -> Result<()> {
        self.remove(&format!("{lock}/next-{owner}.txt")).await?;
        self.remove(&format!("{lock}/owner-{owner}")).await?;
        self.sftp.rmdir(lock).await.map_err(|e| sftp_error("释放远程写锁", e))?;
        Ok(())
    }

    async fn commit(&self, path: &str, client_id: &str, batch: &[Pending]) -> Result<()> {
        let lock = format!("{path}.gameready-lock");
        let owner = self.acquire(&lock, client_id).await?;
        let result = async {
            let original = self.read(path).await?;
            let merged = merge_remote(original.as_deref().unwrap_or_default(), batch)?;
            if original.as_deref() != Some(merged.as_slice()) {
                let tmp = format!("{lock}/next-{owner}.txt");
                self.write(&tmp, &merged, false).await?;
                if self.read(&tmp).await?.as_deref() != Some(merged.as_slice()) {
                    return Err(error("verify_error", "远程临时文件校验失败，原账号文件未修改", true));
                }
                self.sftp.stat(format!("{lock}/owner-{owner}")).await
                    .map_err(|_| error("remote_busy", "远程写锁已失效，记录保留，稍后重试", true))?;
                self.replace(&tmp, path).await?;
            }
            if self.read(path).await?.as_deref() != Some(merged.as_slice()) {
                return Err(error("verify_error", "NAS 文件写后校验失败，记录保留，稍后重试", true));
            }
            Ok(())
        }.await;
        // 清理失败时保留 owner，下次连接同一安装可以恢复；已确认的写入不重复排队。
        let _ = self.release(&lock, &owner).await;
        result
    }

    async fn probe(&self, path: &str) -> Result<()> {
        let (parent, _) = path.rsplit_once('/').unwrap_or(("", ""));
        let stem = format!("{parent}/.gameready-probe-{}", Uuid::new_v4());
        let a = format!("{stem}.a");
        let b = format!("{stem}.b");
        let result = async {
            self.write(&a, b"probe-before", true).await?;
            self.write(&b, b"probe-after", true).await?;
            self.replace(&b, &a).await?;
            if self.read(&a).await?.as_deref() != Some(b"probe-after".as_slice()) {
                return Err(error("verify_error", "NAS 文件读写测试未通过", false));
            }
            Ok(())
        }.await;
        let cleanup_a = self.remove(&a).await;
        let cleanup_b = self.remove(&b).await;
        result?;
        cleanup_a?;
        cleanup_b?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct TestReport { pub message: String, pub fingerprint: String }

pub async fn test(mut config: Config) -> Result<TestReport> {
    config.host = config.host.trim().into();
    config.username = config.username.trim().into();
    config.remote_path = config.remote_path.trim().into();
    let saved = load()?;
    config.fingerprint = if config.host == saved.config.host && config.port == saved.config.port {
        saved.config.fingerprint.clone()
    } else { String::new() };
    let result = async {
        let remote = Remote::connect(&config).await?;
        let result = remote.probe(&config.remote_path).await;
        let fingerprint = remote.fingerprint.clone();
        remote.close().await;
        result?;
        Ok::<_, Error>(fingerprint)
    }.await;
    match result {
        Ok(fingerprint) => {
            config.fingerprint = fingerprint.clone();
            *TESTED_CONFIG.lock().unwrap_or_else(|p| p.into_inner()) = Some(config.clone());
            if same_connection(&saved.config, &config) {
                edit(|s| {
                    if same_connection(&s.config, &config) {
                        s.verified = true;
                        if s.config.fingerprint.is_empty() { s.config.fingerprint = fingerprint.clone(); }
                    }
                    Ok(())
                })?;
            }
            Ok(TestReport { message: "SFTP 登录、文件写入、替换及读回验证通过".into(), fingerprint })
        }
        Err(e) => {
            {
                let mut tested = TESTED_CONFIG.lock().unwrap_or_else(|p| p.into_inner());
                if tested.as_ref().is_some_and(|c| same_connection(c, &config)) { *tested = None; }
            }
            if same_connection(&saved.config, &config) {
                edit(|s| {
                    if same_connection(&s.config, &config) { s.verified = false; }
                    Ok(())
                })?;
            }
            Err(e)
        }
    }
}

pub async fn sync(force: bool, account: Option<&str>) -> Result<Snapshot> {
    let _guard = SYNC_LOCK.lock().await;
    let mut store = load()?;
    if !force && !store.config.enabled { return Ok(store.snapshot()); }
    // 旧队列仍需登录证明；只有用户明确手动同步的记录可绕过此自动备份前提。
    if store.queue.iter().any(|p| !upload_allowed(&store, p)) {
        edit(|s| { prune_unconfirmed(s); Ok(()) })?;
        store = load()?;
    }
    let mut missing_count = 0;
    if force || !store.credentials_seeded {
        edit(|s| {
            let inventory = inventory(&s.confirmed_credentials)?;
            missing_count = inventory.missing.len();
            if force {
                validate_config(&s.config)?;
                prepare_manual(s, &inventory, account)?;
            } else if s.config.enabled {
                seed_inventory(s, &inventory, "历史账号补传");
            }
            Ok(())
        })?;
        store = load()?;
        if !force && !store.config.enabled { return Ok(store.snapshot()); }
    }
    if force {
        store.queue.retain(|p| account.is_none_or(|a| p.account.eq_ignore_ascii_case(a)));
        if store.queue.is_empty() {
            return edit(|s| {
                s.message = if account.is_some() { "该账号已同步，无需重复上传".into() }
                    else if missing_count > 0 { format!("暂无可同步账号，{missing_count} 个账号待补充密码") }
                    else { "所有账号均已同步".into() };
                Ok(s.snapshot())
            });
        }
    }
    if !force && (store.queue.is_empty() || store.blocked || now() < store.next_retry_ms) { return Ok(store.snapshot()); }
    validate_config(&store.config)?;
    edit(|s| { s.status = "syncing".into(); s.message = "正在连接 NAS 并合并账号文件".into(); Ok(()) })?;
    let result = async {
        let remote = Remote::connect(&store.config).await?;
        // 首次成功认证后保存主机指纹，后续连接拒绝密钥改变。
        let pinned = edit(|s| {
            if s.revision == store.revision && s.config.fingerprint.is_empty() { s.config.fingerprint = remote.fingerprint.clone(); }
            Ok(())
        });
        let result = match pinned {
            Ok(()) => tokio::time::timeout(Duration::from_secs(60), remote.commit(&store.config.remote_path, &store.client_id, &store.queue)).await
                .unwrap_or_else(|_| Err(error("network_error", "同步超时，记录保留，稍后自动重试", true))),
            Err(e) => Err(e),
        };
        remote.close().await;
        result
    }.await;
    if result.is_err() {
        let mut tested = TESTED_CONFIG.lock().unwrap_or_else(|p| p.into_inner());
        if tested.as_ref().is_some_and(|c| same_connection(c, &store.config)) { *tested = None; }
    }
    let mut obsolete = false;
    let snapshot = edit(|s| {
        if s.revision != store.revision { obsolete = true; WAKE.notify_one(); return Ok(s.snapshot()); }
        match &result {
            Ok(()) => {
                s.verified = true;
                s.acknowledge(&store.queue);
                s.last_success_ms = Some(now());
                s.failures = 0; s.blocked = false; s.next_retry_ms = 0;
                s.status = if s.queue.is_empty() { "idle" } else { "pending" }.into();
                s.message = match account {
                    Some(account) => format!("{account}：同步成功"),
                    None => format!("同步成功：已同步 {} 个账号，远程重复账号已合并", store.queue.len()),
                };
                if force && account.is_none() && missing_count > 0 {
                    s.message.push_str(&format!("；{missing_count} 个账号待补充密码"));
                }
                if !s.queue.is_empty() { WAKE.notify_one(); }
            }
            Err(e) => {
                s.verified = false;
                s.failures = s.failures.saturating_add(1);
                s.blocked = !e.retryable;
                s.status = e.code.clone(); s.message = e.message.clone();
                s.next_retry_ms = if e.retryable { now() + i64::from(30 * (1u32 << s.failures.min(4))).min(300) * 1000 } else { 0 };
            }
        }
        Ok(s.snapshot())
    })?;
    if obsolete { return Err(error("config_changed", "同步期间配置已改变，请按当前配置重新同步", false)); }
    match result {
        Ok(()) => { crate::log("acct", "Steam 账号备份完成", "NAS 账号文件已合并并读回确认；密码不写入日志", true); Ok(snapshot) }
        Err(e) => {
            if force || store.status != e.code { crate::log("acct", "Steam 账号备份未完成", &e.message, false); }
            Err(e)
        }
    }
}

pub fn start(app: tauri::AppHandle) {
    let _ = APP.set(app);
    tauri::async_runtime::spawn(async {
        loop {
            let _ = sync(false, None).await;
            tokio::select! { _ = WAKE.notified() => (), _ = tokio::time::sleep(Duration::from_secs(15)) => () }
        }
    });
}

#[cfg(test)]
mod protocol_tests {
//! Loopback SSH/SFTP fixture. No external server or user credentials are used.
use super::*;
use std::collections::{BTreeSet, HashMap};
use russh::{server, Channel, ChannelId};
use russh_sftp::protocol::{Attrs, Data, File as RemoteFile, Handle, Name, Status};

#[derive(Default)]
struct Files {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeMap<String, u32>,
    deny_replace: bool,
    truncate_read: bool,
    fail_confirmation: bool,
    replaced: bool,
    posix_rename: bool,
    removed: Vec<String>,
    truncated: Vec<String>,
}

type Shared = Arc<Mutex<Files>>;
struct Ssh {
    files: Shared,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
}

impl server::Handler for Ssh {
    type Error = russh::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> std::result::Result<server::Auth, Self::Error> {
        Ok(if user == "fixture" && password == "fixture-password" { server::Auth::Accept }
            else { server::Auth::Reject { proceed_with_methods: None, partial_success: false } })
    }
    async fn channel_open_session(&mut self, channel: Channel<server::Msg>, reply: server::ChannelOpenHandle,
        _: &mut server::Session) -> std::result::Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut server::Session)
        -> std::result::Result<(), Self::Error> {
        if name != "sftp" { session.channel_failure(id)?; return Ok(()); }
        session.channel_success(id)?;
        let channel = self.channels.remove(&id).unwrap();
        let handler = Sftp { files: self.files.clone(), listed: BTreeSet::new() };
        tokio::spawn(russh_sftp::server::run(channel.into_stream(), handler));
        Ok(())
    }
}

struct Sftp { files: Shared, listed: BTreeSet<String> }
fn ok(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: String::new(), language_tag: String::new() }
}
impl Sftp {
    fn attrs(&self, path: &str) -> std::result::Result<FileAttributes, StatusCode> {
        let files = self.files.lock().unwrap();
        if let Some(bytes) = files.files.get(path) {
            return Ok(FileAttributes { size: Some(bytes.len() as u64), ..Default::default() });
        }
        if let Some(time) = files.dirs.get(path) {
            return Ok(FileAttributes { mtime: Some(*time), ..Default::default() });
        }
        Err(StatusCode::NoSuchFile)
    }
}
impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> Self::Error { StatusCode::OpUnsupported }
    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> std::result::Result<russh_sftp::protocol::Version, Self::Error> {
        let mut version = russh_sftp::protocol::Version::new();
        if self.files.lock().unwrap().posix_rename { version.extensions.insert("posix-rename@openssh.com".into(), "1".into()); }
        Ok(version)
    }
    async fn extended(&mut self, id: u32, request: String, data: Vec<u8>) -> std::result::Result<Packet, Self::Error> {
        if request != "posix-rename@openssh.com" { return Err(StatusCode::OpUnsupported); }
        let mut cursor = data.as_slice();
        fn string(cursor: &mut &[u8]) -> std::result::Result<String, StatusCode> {
            if cursor.len() < 4 { return Err(StatusCode::BadMessage); }
            let len = u32::from_be_bytes(cursor[..4].try_into().unwrap()) as usize;
            *cursor = &cursor[4..];
            if cursor.len() < len { return Err(StatusCode::BadMessage); }
            let text = std::str::from_utf8(&cursor[..len]).map_err(|_| StatusCode::BadMessage)?.to_string();
            *cursor = &cursor[len..]; Ok(text)
        }
        let from = string(&mut cursor)?; let to = string(&mut cursor)?;
        Ok(Packet::Status(russh_sftp::server::Handler::rename(self, id, from, to).await?))
    }
    async fn open(&mut self, id: u32, path: String, flags: OpenFlags, _: FileAttributes)
        -> std::result::Result<Handle, Self::Error> {
        let mut files = self.files.lock().unwrap();
        if flags.contains(OpenFlags::EXCLUDE) && files.files.contains_key(&path) { return Err(StatusCode::Failure); }
        if !files.files.contains_key(&path) && !flags.contains(OpenFlags::CREATE) { return Err(StatusCode::NoSuchFile); }
        let parent = path.rsplit_once('/').unwrap().0;
        if !files.dirs.contains_key(parent) { return Err(StatusCode::NoSuchFile); }
        if flags.contains(OpenFlags::TRUNCATE) {
            files.truncated.push(path.clone());
            files.files.insert(path.clone(), Vec::new());
        }
        files.files.entry(path.clone()).or_default();
        Ok(Handle { id, handle: path })
    }
    async fn close(&mut self, id: u32, _: String) -> std::result::Result<Status, Self::Error> { Ok(ok(id)) }
    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32)
        -> std::result::Result<Data, Self::Error> {
        let files = self.files.lock().unwrap();
        if files.fail_confirmation && files.replaced && handle == "/accounts.txt" { return Err(StatusCode::ConnectionLost); }
        let bytes = files.files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        if files.truncate_read { return Ok(Data { id, data: Vec::new() }); }
        if offset as usize >= bytes.len() { return Err(StatusCode::Eof); }
        Ok(Data { id, data: bytes[offset as usize..bytes.len().min(offset as usize + len as usize)].to_vec() })
    }
    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>)
        -> std::result::Result<Status, Self::Error> {
        let mut files = self.files.lock().unwrap();
        let bytes = files.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        bytes.resize(bytes.len().max(offset as usize + data.len()), 0);
        bytes[offset as usize..offset as usize + data.len()].copy_from_slice(&data);
        Ok(ok(id))
    }
    async fn stat(&mut self, id: u32, path: String) -> std::result::Result<Attrs, Self::Error> {
        Ok(Attrs { id, attrs: self.attrs(&path)? })
    }
    async fn fstat(&mut self, id: u32, handle: String) -> std::result::Result<Attrs, Self::Error> {
        Ok(Attrs { id, attrs: self.attrs(&handle)? })
    }
    async fn mkdir(&mut self, id: u32, path: String, _: FileAttributes) -> std::result::Result<Status, Self::Error> {
        let mut files = self.files.lock().unwrap();
        if files.dirs.contains_key(&path) { return Err(StatusCode::Failure); }
        files.dirs.insert(path, (now() / 1000) as u32);
        Ok(ok(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> std::result::Result<Status, Self::Error> {
        let mut files = self.files.lock().unwrap();
        if files.files.keys().any(|k| k.starts_with(&format!("{path}/"))) { return Err(StatusCode::Failure); }
        files.dirs.remove(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> std::result::Result<Status, Self::Error> {
        let mut files = self.files.lock().unwrap();
        files.removed.push(path.clone());
        files.files.remove(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(ok(id))
    }
    async fn rename(&mut self, id: u32, from: String, to: String) -> std::result::Result<Status, Self::Error> {
        let mut files = self.files.lock().unwrap();
        if files.deny_replace && files.files.contains_key(&to) { return Err(StatusCode::Failure); }
        if let Some(bytes) = files.files.remove(&from) {
            files.files.insert(to.clone(), bytes);
            if to == "/accounts.txt" { files.replaced = true; }
        } else if let Some(time) = files.dirs.remove(&from) {
            files.dirs.insert(to.clone(), time);
            let keys: Vec<_> = files.files.keys().filter(|k| k.starts_with(&format!("{from}/"))).cloned().collect();
            for k in keys { let bytes = files.files.remove(&k).unwrap(); files.files.insert(format!("{to}{}", &k[from.len()..]), bytes); }
        } else { return Err(StatusCode::NoSuchFile); }
        Ok(ok(id))
    }
    async fn opendir(&mut self, id: u32, path: String) -> std::result::Result<Handle, Self::Error> {
        self.attrs(&path)?;
        self.listed.remove(&path);
        Ok(Handle { id, handle: path })
    }
    async fn readdir(&mut self, id: u32, handle: String) -> std::result::Result<Name, Self::Error> {
        if !self.listed.insert(handle.clone()) { return Err(StatusCode::Eof); }
        let files = self.files.lock().unwrap();
        let prefix = format!("{handle}/");
        let entries = files.files.keys().filter_map(|k| k.strip_prefix(&prefix))
            .filter(|k| !k.contains('/')).map(|k| RemoteFile::dummy(k.to_string())).collect();
        Ok(Name { id, files: entries })
    }
}

struct Fixture { config: Config, files: Shared, listener: tokio::task::JoinHandle<()> }
impl Drop for Fixture { fn drop(&mut self) { self.listener.abort(); } }
async fn fixture() -> Fixture {
    let key = russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let config = Arc::new(server::Config { keys: vec![key], auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO), ..Default::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let files = Arc::new(Mutex::new(Files::default()));
    files.lock().unwrap().dirs.insert(String::new(), (now() / 1000) as u32);
    let shared = files.clone();
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let config = config.clone();
            let handler = Ssh { files: shared.clone(), channels: HashMap::new() };
            tokio::spawn(async move { if let Ok(session) = server::run_stream(config, socket, handler).await { let _ = session.await; } });
        }
    });
    Fixture { config: Config { enabled: true, host: "127.0.0.1".into(), port, username: "fixture".into(),
        password: "fixture-password".into(), remote_path: "/accounts.txt".into(), fingerprint }, files, listener: task }
}

#[tokio::test]
async fn sync_only_adds_or_updates_and_never_deletes_or_truncates_the_account_file() {
    for posix in [false, true] {
        let f = fixture().await;
        {
            let mut files = f.files.lock().unwrap();
            files.posix_rename = posix;
            files.files.insert("/accounts.txt".into(), b"nas_only----keep\nplayer----old\n".to_vec());
        }
        let remote = Remote::connect(&f.config).await.unwrap();
        remote.probe("/accounts.txt").await.unwrap();
        let mut store = Store::default();
        store.enqueue("player", "updated", "手动同步");
        store.enqueue("added", "new", "手动同步");
        remote.commit("/accounts.txt", "device", &store.queue).await.unwrap();
        // Local removal or an empty upload must never remove NAS-only accounts.
        remote.commit("/accounts.txt", "device", &[]).await.unwrap();
        {
            let files = f.files.lock().unwrap();
            assert_eq!(files.files["/accounts.txt"], b"added----new\nnas_only----keep\nplayer----updated\n");
            assert!(!files.removed.iter().any(|p| p == "/accounts.txt"));
            assert!(!files.truncated.iter().any(|p| p == "/accounts.txt"));
            assert_eq!(files.files.len(), 1);
        }
        remote.close().await;
    }
}

#[tokio::test]
async fn authentication_and_host_key_fail_closed() {
    let f = fixture().await;
    let mut config = f.config.clone();
    config.password = "incorrect".into();
    assert_eq!(Remote::connect(&config).await.err().unwrap().code, "auth_error");
    config = f.config.clone(); config.fingerprint = "SHA256:wrong".into();
    assert_eq!(Remote::connect(&config).await.err().unwrap().code, "host_key_error");
    assert!(f.files.lock().unwrap().files.is_empty());
}

#[tokio::test]
async fn probe_merge_and_retry_after_lost_confirmation() {
    let f = fixture().await;
    let remote = Remote::connect(&f.config).await.unwrap();
    remote.probe("/accounts.txt").await.unwrap();
    assert!(f.files.lock().unwrap().files.is_empty());
    f.files.lock().unwrap().files.insert("/accounts.txt".into(), b"User----old\nUSER----previous\nother----keep\n".to_vec());
    let mut store = Store::default(); store.enqueue("user", "latest", "登录");
    f.files.lock().unwrap().fail_confirmation = true;
    assert!(remote.commit("/accounts.txt", "device-a", &store.queue).await.unwrap_err().retryable);
    assert_eq!(store.queue.len(), 1);
    f.files.lock().unwrap().fail_confirmation = false;
    remote.commit("/accounts.txt", "device-a", &store.queue).await.unwrap();
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], b"other----keep\nuser----latest\n");
    store.acknowledge(&store.queue.clone()); assert!(store.queue.is_empty());
    remote.close().await;
}

#[tokio::test]
async fn manual_unconfirmed_password_syncs_directly_and_all_skips_receipts() {
    let f = fixture().await;
    let remote = Remote::connect(&f.config).await.unwrap();
    let mut store = Store::default();
    let creds = BTreeMap::from([("player".into(), " saved ---- password ".into()), ("other".into(), "second".into())]);
    let inventory = classify_inventory(&creds, &["missing".into()], &store.confirmed_credentials);
    prepare_manual(&mut store, &inventory, Some("player")).unwrap();
    assert!(store.confirmed_credentials.is_empty());
    remote.commit("/accounts.txt", "manual-device", &store.queue).await.unwrap();
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], b"player---- saved ---- password \n");
    store.acknowledge(&store.queue.clone());
    prepare_manual(&mut store, &inventory, None).unwrap();
    assert_eq!(store.queue.len(), 1);
    assert_eq!(store.queue[0].account, "other");
    remote.commit("/accounts.txt", "manual-device", &store.queue).await.unwrap();
    store.acknowledge(&store.queue.clone());
    let snapshot = store.snapshot_from_inventory(Ok(inventory));
    assert!(snapshot.queue.is_empty() && snapshot.unconfirmed_password_accounts.is_empty());
    assert_eq!(snapshot.missing_password_accounts, vec!["missing"]);
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], b"other----second\nplayer---- saved ---- password \n");
    remote.close().await;
}

#[tokio::test]
async fn failed_replace_and_partial_reads_preserve_original() {
    let f = fixture().await;
    let original = b"user----original\n".to_vec();
    f.files.lock().unwrap().files.insert("/accounts.txt".into(), original.clone());
    let remote = Remote::connect(&f.config).await.unwrap();
    let mut store = Store::default(); store.enqueue("user", "new", "登录");
    f.files.lock().unwrap().deny_replace = true;
    assert!(remote.commit("/accounts.txt", "device-a", &store.queue).await.is_err());
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], original);
    f.files.lock().unwrap().deny_replace = false;
    f.files.lock().unwrap().truncate_read = true;
    assert!(remote.commit("/accounts.txt", "device-a", &store.queue).await.is_err());
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], original);
    remote.close().await;
}

#[tokio::test]
async fn concurrent_devices_and_abandoned_locks() {
    let f = fixture().await;
    let a = Remote::connect(&f.config).await.unwrap();
    let b = Remote::connect(&f.config).await.unwrap();
    let lock = "/accounts.txt.gameready-lock";
    let owner = a.acquire(lock, "device-a").await.unwrap();
    assert_eq!(b.acquire(lock, "device-b").await.unwrap_err().code, "remote_busy");
    // Simulate a device that has exited, with a lock older than the lease window.
    f.files.lock().unwrap().dirs.insert(lock.into(), (now() / 1000 - 601) as u32);
    let new_owner = b.acquire(lock, "device-b").await.unwrap();
    assert!(!f.files.lock().unwrap().files.contains_key(&format!("{lock}/owner-{owner}")));
    a.release(lock, &owner).await.unwrap_err();
    assert!(f.files.lock().unwrap().files.contains_key(&format!("{lock}/owner-{new_owner}")));
    b.release(lock, &new_owner).await.unwrap();
    a.close().await; b.close().await;
}

#[tokio::test]
async fn openssh_rename_extension_and_network_outage() {
    let mut f = fixture().await;
    f.files.lock().unwrap().posix_rename = true;
    let remote = Remote::connect(&f.config).await.unwrap();
    assert!(remote.posix_rename);
    remote.probe("/accounts.txt").await.unwrap();
    let mut store = Store::default(); store.enqueue("player", "fixture-latest", "登录");
    remote.commit("/accounts.txt", "device-a", &store.queue).await.unwrap();
    assert_eq!(f.files.lock().unwrap().files["/accounts.txt"], b"player----fixture-latest\n");
    remote.close().await;
    f.listener.abort();
    let _ = (&mut f.listener).await;
    let err = Remote::connect(&f.config).await.err().unwrap();
    assert_eq!(err.code, "network_error"); assert!(err.retryable);
    assert_eq!(store.queue.len(), 1);
}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_accounts_use_last_remote_then_new_password() {
        let mut s = Store::default();
        s.enqueue("Player", " newest ---- with spaces ", "新增登录");
        let result = merge_remote(b"\xef\xbb\xbfplayer----old\r\nOTHER----keep\r\nPLAYER----later\r\n", &s.queue).unwrap();
        assert_eq!(String::from_utf8(result).unwrap(), "OTHER----keep\nPlayer---- newest ---- with spaces \n");
    }

    #[test]
    fn retries_are_idempotent_and_coalesce_to_latest_password() {
        let mut s = Store::default();
        s.enqueue("user", "old", "登录");
        s.enqueue("USER", "new", "登录");
        assert_eq!(s.queue.len(), 1);
        let first = merge_remote(b"user----previous\n", &s.queue).unwrap();
        assert_eq!(merge_remote(&first, &s.queue).unwrap(), first);
        let sent = s.queue.clone();
        s.enqueue("user", "newer-during-upload", "登录");
        s.acknowledge(&sent);
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.queue[0].password, "newer-during-upload");
    }

    #[test]
    fn damaged_remote_is_rejected_instead_of_discarded() {
        assert!(merge_remote(b"bad line\n", &[]).is_err());
        assert!(merge_remote(b"user----\n", &[]).is_err());
        assert!(merge_remote(b"\xff\xfe", &[]).is_err());
    }

    #[test]
    fn durable_queue_and_corrupt_local_file_are_preserved() {
        let dir = std::env::temp_dir().join(format!("gameready-backup-test-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("backup.json");
        let mut s = Store::default();
        s.enqueue("user", " local password ", "新增登录");
        s.blocked = true; s.status = "auth_error".into();
        s.verified = true; s.credentials_seeded = true;
        s.confirmed_credentials.insert("user".into(), credential_digest("user", " local password "));
        write_store(&path, &s).unwrap();
        let restored = read_store(&path).unwrap();
        assert!(restored.blocked);
        assert!(restored.verified && restored.credentials_seeded);
        assert!(credential_confirmed(&restored.confirmed_credentials, "USER", " local password "));
        assert_eq!(restored.queue[0].password, " local password ");
        let corrupt = b"{unfinished";
        fs::write(&path, corrupt).unwrap();
        assert!(read_store(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn historical_accounts_require_usable_unambiguous_passwords() {
        let creds = BTreeMap::from([
            ("Ready".into(), " with spaces ".into()),
            ("Duplicate".into(), "old".into()),
            ("DUPLICATE".into(), "new".into()),
            ("Invalid".into(), "line\nbreak".into()),
        ]);
        let history = vec!["READY".into(), "HistoryOnly".into()];
        let confirmed = BTreeMap::from([("ready".into(), credential_digest("ready", " with spaces "))]);
        let inventory = classify_inventory(&creds, &history, &confirmed);
        assert_eq!(inventory.credentials, vec![("ready".into(), " with spaces ".into())]);
        assert_eq!(inventory.missing, vec!["duplicate", "historyonly", "invalid"]);
    }

    #[test]
    fn historical_seed_does_not_replace_newer_queued_passwords() {
        let inventory = classify_inventory(&BTreeMap::from([("player".into(), "historical".into()),
            ("other".into(), "saved".into())]), &["unknown".into()],
            &BTreeMap::from([("player".into(), credential_digest("player", "historical")),
                ("other".into(), credential_digest("other", "saved"))]));
        let mut store = Store::default();
        store.confirmed_credentials.insert("player".into(), credential_digest("player", "newer"));
        store.confirmed_credentials.insert("other".into(), credential_digest("other", "saved"));
        store.enqueue("PLAYER", "newer", "新增登录");
        let id = store.queue[0].id.clone();
        seed_inventory(&mut store, &inventory, "历史账号补传");
        assert!(store.credentials_seeded);
        assert_eq!(store.queue.len(), 2);
        assert_eq!(store.queue[0].password, "newer");
        assert_eq!(store.queue[0].id, id);
        seed_inventory(&mut store, &inventory, "再次补传");
        assert_eq!(store.queue.len(), 2);
    }

    #[test]
    fn verification_is_bound_to_credentials_and_destination() {
        let original = Config { host: "nas.example.com".into(), port: 2022, username: "fixture".into(),
            password: "fixture-password".into(), remote_path: "/backup/accounts.txt".into(), ..Default::default() };
        let mut changed = original.clone();
        changed.enabled = true; changed.fingerprint = "pinned".into();
        assert!(same_connection(&original, &changed));
        for field in 0..5 {
            let mut changed = original.clone();
            match field {
                0 => changed.host = "other.example.com".into(),
                1 => changed.port = 2222,
                2 => changed.username = "other".into(),
                3 => changed.password = "different".into(),
                _ => changed.remote_path = "/other/accounts.txt".into(),
            }
            assert!(!same_connection(&original, &changed));
        }
        let legacy: Store = serde_json::from_str(r#"{"config":{"enabled":true}}"#).unwrap();
        assert!(!legacy.verified && !legacy.credentials_seeded);
    }

    #[test]
    fn input_and_legacy_queues_do_not_qualify_for_upload() {
        let mut store = Store::default();
        store.enqueue("legacy", "wrong-password", "旧版队列");
        prune_unconfirmed(&mut store);
        assert!(store.queue.is_empty());
        let creds = BTreeMap::from([("player".into(), "not-yet-confirmed".into())]);
        let inventory = classify_inventory(&creds, &[], &store.confirmed_credentials);
        assert!(inventory.credentials.is_empty());
        assert_eq!(inventory.unconfirmed, vec!["player"]);
        seed_inventory(&mut store, &inventory, "历史补传");
        assert!(store.queue.is_empty());
    }

    #[test]
    fn confirmation_is_bound_to_exact_password_and_cleared_by_retry() {
        let mut store = Store::default();
        store.config.enabled = true;
        store.confirm_success("Player", " right password ", "成功登录").unwrap();
        assert_eq!(store.queue.len(), 1);
        assert!(credential_confirmed(&store.confirmed_credentials, "PLAYER", " right password "));
        assert!(!credential_confirmed(&store.confirmed_credentials, "player", "right password"));
        forget_confirmation(&mut store, "PLAYER");
        assert!(store.queue.is_empty());
        assert!(!credential_confirmed(&store.confirmed_credentials, "player", " right password "));
        store.confirm_success("player", "new-confirmed", "晚到成功").unwrap();
        assert_eq!(store.queue[0].password, "new-confirmed");
        let changed = BTreeMap::from([("player".into(), "different-unconfirmed".into())]);
        assert!(classify_inventory(&changed, &[], &store.confirmed_credentials).credentials.is_empty());
    }

    #[test]
    fn success_while_disabled_can_be_seeded_only_after_enabling() {
        let mut store = Store::default();
        store.confirm_success("player", "verified", "成功登录").unwrap();
        assert!(store.queue.is_empty());
        let creds = BTreeMap::from([("player".into(), "verified".into())]);
        let inventory = classify_inventory(&creds, &[], &store.confirmed_credentials);
        store.config.enabled = true;
        seed_inventory(&mut store, &inventory, "历史补传");
        assert_eq!(store.queue.len(), 1);
    }

    #[test]
    fn password_only_moves_missing_to_unconfirmed_without_auto_upload() {
        let mut store = Store::default();
        store.config.enabled = true;
        let history = vec!["player".into()];
        let before = store.snapshot_from_inventory(Ok(classify_inventory(&BTreeMap::new(), &history, &store.confirmed_credentials)));
        assert_eq!(before.missing_password_accounts, history);
        let creds = BTreeMap::from([("player".into(), " new password ---- ".into())]);
        let inventory = classify_inventory(&creds, &history, &store.confirmed_credentials);
        seed_inventory(&mut store, &inventory, "自动补传");
        assert!(store.queue.is_empty());
        let after = store.snapshot_from_inventory(Ok(inventory));
        assert!(after.missing_password_accounts.is_empty());
        assert_eq!(after.unconfirmed_password_accounts, history);
        assert!(store.confirmed_credentials.is_empty());
    }

    #[test]
    fn manual_single_upload_is_scoped_and_does_not_confirm_login() {
        let mut store = Store::default();
        let creds = BTreeMap::from([("player".into(), " first ".into()), ("other".into(), "second".into())]);
        let inventory = classify_inventory(&creds, &[], &store.confirmed_credentials);
        prepare_manual(&mut store, &inventory, Some("PLAYER")).unwrap();
        prune_unconfirmed(&mut store);
        assert_eq!(store.queue.len(), 1);
        assert_eq!(store.queue[0].account, "player");
        assert!(upload_allowed(&store, &store.queue[0]));
        assert!(store.confirmed_credentials.is_empty());
        let snapshot = store.snapshot_from_inventory(Ok(classify_inventory(&creds, &[], &store.confirmed_credentials)));
        assert_eq!(snapshot.queue.len(), 1);
        assert!(!snapshot.queue[0].login_confirmed);
        assert_eq!(snapshot.unconfirmed_password_accounts, vec!["other"]);
        let batch = store.queue.clone();
        store.acknowledge(&batch);
        prepare_manual(&mut store, &inventory, Some("PLAYER")).unwrap();
        assert!(store.queue.is_empty());
        assert!(prepare_manual(&mut store, &inventory, Some("missing")).is_err());
    }

    #[test]
    fn sync_all_skips_synced_passwords_and_keeps_missing_accounts_visible() {
        let mut store = Store::default();
        store.synced_credentials.insert("done".into(), credential_digest("done", "same"));
        let creds = BTreeMap::from([("done".into(), "same".into()), ("player".into(), "new".into())]);
        let history = vec!["missing".into()];
        let inventory = classify_inventory(&creds, &history, &store.confirmed_credentials);
        prepare_manual(&mut store, &inventory, None).unwrap();
        assert_eq!(store.queue.len(), 1);
        let sent = store.queue.clone();
        store.acknowledge(&sent);
        let snapshot = store.snapshot_from_inventory(Ok(classify_inventory(&creds, &history, &store.confirmed_credentials)));
        assert!(snapshot.queue.is_empty() && snapshot.unconfirmed_password_accounts.is_empty());
        assert_eq!(snapshot.missing_password_accounts, history);
        prepare_manual(&mut store, &inventory, None).unwrap();
        assert!(store.queue.is_empty());
        let changed = BTreeMap::from([("player".into(), "changed".into())]);
        prepare_manual(&mut store, &classify_inventory(&changed, &[], &BTreeMap::new()), None).unwrap();
        assert_eq!(store.queue[0].password, "changed");
    }

    #[test]
    fn confirmed_login_auto_queues_once_and_old_ack_does_not_hide_new_password() {
        let mut store = Store::default();
        store.config.enabled = true;
        store.confirm_success("player", "first", "列表登录").unwrap();
        let sent = store.queue.clone();
        store.confirm_success("player", "second", "新增登录确认").unwrap();
        store.acknowledge(&sent);
        assert_eq!(store.queue.len(), 1);
        assert_eq!(store.queue[0].password, "second");
        let sent = store.queue.clone();
        store.acknowledge(&sent);
        store.confirm_success("player", "second", "晚到确认").unwrap();
        assert!(store.queue.is_empty());
        let creds = BTreeMap::from([("player".into(), "second".into())]);
        let inventory = classify_inventory(&creds, &[], &store.confirmed_credentials);
        seed_inventory(&mut store, &inventory, "历史补传");
        assert!(store.queue.is_empty());
        // 换一个 NAS 目标时，原目标的同步记录不能用于跳过上传。
        store.synced_credentials.clear();
        seed_inventory(&mut store, &inventory, "新目标补传");
        assert_eq!(store.queue.len(), 1);
    }

    #[test]
    fn manual_retry_and_sync_receipts_survive_restart() {
        let mut store = Store::default();
        let creds = BTreeMap::from([("player".into(), "saved".into())]);
        let inventory = classify_inventory(&creds, &[], &store.confirmed_credentials);
        prepare_manual(&mut store, &inventory, None).unwrap();
        let mut restored: Store = serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        prune_unconfirmed(&mut restored);
        assert_eq!(restored.queue.len(), 1);
        let sent = restored.queue.clone();
        restored.acknowledge(&sent);
        let mut restored: Store = serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
        prepare_manual(&mut restored, &inventory, None).unwrap();
        assert!(restored.queue.is_empty());
    }
}
