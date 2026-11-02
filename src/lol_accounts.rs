//! LOL 账号与备份独立存储；仅复用 Steam 的 SFTP 传输，不共享账号或同步回执。
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::Emitter;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use crate::backup::{self, Config, Error, Snapshot, QueueItem, TestReport, error};
use crate::backup_settings::{self, Game, Resolved};

type Result<T> = std::result::Result<T, Error>;
static STORE_LOCK: Mutex<()> = Mutex::new(());
static SYNC_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
// 每次进程各有身份，复制 Data 不会让两台机器回收对方的在途写锁。
static CLIENT: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct SavedAccount {
    password: String,
    favorite: bool,
    updated_ms: i64,
    synced_digest: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize)]
pub struct Account {
    pub account_name: String,
    pub password: String,
    pub favorite: bool,
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u32,
    #[serde(default)]
    accounts: BTreeMap<String, SavedAccount>,
    #[serde(default)]
    manual_pending: BTreeSet<String>,
    #[serde(default = "default_config")]
    config: Config,
    #[serde(default)]
    shared_target: String,
    #[serde(default)]
    shared_generation: u64,
    #[serde(skip)]
    legacy_config: Option<Config>,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    state_version: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    last_success_ms: Option<i64>,
    #[serde(default)]
    failures: u32,
    #[serde(default)]
    blocked: bool,
    #[serde(default)]
    next_retry_ms: i64,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn default_config() -> Config { Config { remote_path: "/GameReady/lol-accounts.txt".into(), ..Config::default() } }
impl Default for Store {
    fn default() -> Self {
        Self { version: 1, accounts: BTreeMap::new(), manual_pending: BTreeSet::new(), config: default_config(), shared_target: String::new(), shared_generation: 0, legacy_config: None, verified: false,
            revision: 0, state_version: 0, status: "disabled".into(), message: "自动备份未开启".into(),
            last_success_ms: None, failures: 0, blocked: false, next_retry_ms: 0, extra: BTreeMap::new() }
    }
}

fn path() -> PathBuf { crate::data_dir().join("lol-accounts.json") }
fn now() -> i64 { chrono::Utc::now().timestamp_millis() }
fn local_error(message: String) -> Error { error("local_error", &message, false) }
fn read(path: &Path) -> Result<Store> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(_) => return Err(error("local_error", "读取 LOL 账号文件失败，原文件未修改", false)),
    };
    let store: Store = serde_json::from_slice(&bytes).map_err(|_| error("local_error",
        "Data/lol-accounts.json 格式损坏，原文件已保留", false))?;
    if store.version != 1 { return Err(error("local_error", "LOL 账号文件版本不受支持，原文件已保留", false)); }
    Ok(store)
}
pub(crate) fn legacy_backup_at(path: &Path) -> Result<(Config, bool)> {
    let store = read(path)?;
    Ok((store.config, store.verified))
}
fn edit_at<T>(path: &Path, action: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = read(path)?;
    let result = action(&mut store)?;
    store.state_version = store.state_version.saturating_add(1);
    crate::atomic_json(path, &store).map_err(local_error)?;
    Ok(result)
}
fn load() -> Result<Store> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    read(&path())
}

fn apply_shared(store: &mut Store, shared: Resolved) -> bool {
    if shared.suspended {
        store.legacy_config = Some(store.config.clone()); store.config = shared.config; store.verified = false;
        store.status = "config_conflict".into(); store.message = "公共备份设置需要确认，账号和待同步记录已保留".into();
        return false;
    }
    let changed = if store.shared_target.is_empty() { !backup_settings::same_target(&store.config, &shared.config) }
        else { store.shared_target != shared.digest };
    let updated = changed || store.shared_generation != shared.generation || store.config.enabled != shared.config.enabled
        || store.config.remote_path != shared.config.remote_path || store.verified != shared.verified
        || !store.config.host.is_empty() || !store.config.password.is_empty() || !store.config.username.is_empty() || !store.config.fingerprint.is_empty();
    if changed { for account in store.accounts.values_mut() { account.synced_digest = None; } }
    let config_changed = changed || store.shared_generation != shared.generation;
    if config_changed {
        store.revision = store.revision.saturating_add(1);
    }
    if config_changed || (!store.verified && shared.verified) {
        store.failures = 0; store.blocked = false; store.next_retry_ms = 0;
        if config_changed || store.status != "syncing" {
            store.status = if shared.config.enabled { "pending" } else { "disabled" }.into(); store.message = "公共备份配置已应用".into();
        }
    }
    store.config = shared.config; store.shared_target = shared.digest; store.shared_generation = shared.generation;
    store.verified = shared.verified; store.legacy_config = None;
    if updated { store.state_version = store.state_version.saturating_add(1); }
    updated
}
fn write_backup_store(path: &Path, store: &Store) -> Result<()> {
    let mut value = serde_json::to_value(store).map_err(|_| local_error("LOL 账号数据序列化失败".into()))?;
    if let Some(legacy) = &store.legacy_config { value["config"] = serde_json::to_value(legacy).map_err(|_| local_error("旧备份配置保存失败".into()))?; }
    else {
        for key in ["host", "username", "password", "fingerprint"] { value["config"][key] = Value::String(String::new()); }
        value["config"]["port"] = serde_json::json!(2022);
    }
    crate::atomic_json(path, &value).map_err(local_error)
}
fn load_backup() -> Result<Store> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = read(&path())?;
    if apply_shared(&mut store, backup_settings::resolved(Game::Lol)?) { write_backup_store(&path(), &store)?; }
    Ok(store)
}
fn edit_backup<T>(action: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = read(&path())?;
    apply_shared(&mut store, backup_settings::resolved(Game::Lol)?);
    let result = action(&mut store)?;
    store.state_version = store.state_version.saturating_add(1);
    write_backup_store(&path(), &store)?;
    Ok(result)
}
pub(crate) fn wake_shared_settings() { WAKE.notify_one(); }
fn snapshot(store: &Store) -> Snapshot {
    let mut missing = Vec::new();
    let mut queue = Vec::new();
    for (name, a) in &store.accounts {
        if !backup::valid_credential(name, &a.password) { missing.push(name.clone()); continue; }
        if a.synced_digest.as_ref() == Some(&backup::credential_digest(name, &a.password)) { continue; }
        queue.push(QueueItem { account: name.clone(), source: "已保存账号".into(), ts_ms: a.updated_ms, login_confirmed: false });
    }
    Snapshot { state_version: store.state_version, config: store.config.clone(), config_verified: store.verified,
        missing_password_accounts: missing, unconfirmed_password_accounts: Vec::new(), inventory_error: None,
        queue, status: store.status.clone(), message: store.message.clone(), last_success_ms: store.last_success_ms,
        next_retry_ms: store.next_retry_ms }
}
fn publish() -> Result<Snapshot> {
    let value = snapshot(&load_backup()?);
    if let Some(app) = APP.get() {
        let mut public = value.clone();
        public.config.password.clear();
        let _ = app.emit("lol-backup-changed", public);
    }
    Ok(value)
}
fn changed(store: &mut Store) {
    store.revision = store.revision.saturating_add(1);
    store.failures = 0; store.blocked = false; store.next_retry_ms = 0;
    store.status = if store.config.enabled { "pending" } else { "disabled" }.into();
    store.message = "LOL 账号已保存".into();
}
fn save_account_at(path: &Path, name: &str, password: &str) -> Result<()> {
    let name = name.trim();
    if !backup::valid_credential(name, password) { return Err(error("format_error", "请输入有效账号和密码，不能包含换行或账号分隔符", false)); }
    edit_at(path, |store| {
        let variants: Vec<_> = store.accounts.keys().filter(|key| key.eq_ignore_ascii_case(name)).cloned().collect();
        if variants.is_empty() { store.accounts.insert(name.into(), SavedAccount::default()); }
        for (key, a) in &mut store.accounts {
            if key.eq_ignore_ascii_case(name) {
                if a.password != password { a.synced_digest = None; store.manual_pending.remove(&key.to_ascii_lowercase()); }
                a.password = password.into(); a.updated_ms = now();
            }
        }
        changed(store); Ok(())
    })
}

#[tauri::command]
pub fn lol_accounts_list() -> Result<Vec<Account>> {
    let store = load()?;
    let mut merged = BTreeMap::<String, Account>::new();
    for (name, value) in store.accounts {
        let key = name.to_ascii_lowercase();
        if let Some(existing) = merged.get_mut(&key) { existing.favorite |= value.favorite; }
        else { merged.insert(key, Account { account_name: name, password: value.password, favorite: value.favorite }); }
    }
    Ok(merged.into_values().collect())
}
#[tauri::command]
pub fn lol_account_save(account_name: String, password: String) -> Result<()> {
    save_account_at(&path(), &account_name, &password)?;
    let _ = publish(); WAKE.notify_one(); Ok(())
}

#[tauri::command]
pub fn lol_account_delete(account_name: String) -> Result<()> {
    edit_at(&path(), |store| {
        store.accounts.retain(|key, _| !key.eq_ignore_ascii_case(account_name.trim()));
        store.manual_pending.remove(&account_name.trim().to_ascii_lowercase());
        changed(store); store.message = "LOL 账号已删除".into(); Ok(())
    })?;
    let _ = publish(); WAKE.notify_one(); Ok(())
}
#[tauri::command]
pub fn lol_account_favorite(account_name: String, favorite: bool) -> Result<()> {
    edit_at(&path(), |store| {
        let mut found = false;
        for (key, a) in &mut store.accounts {
            if key.eq_ignore_ascii_case(account_name.trim()) { a.favorite = favorite; found = true; }
        }
        if !found { return Err(error("not_found", "账号已删除，请刷新列表", false)); }
        Ok(())
    })
}

#[tauri::command]
pub async fn lol_account_fill_wegame(account_name: String) -> Result<crate::wegame::FillOutcome> {
    tauri::async_runtime::spawn_blocking(move || {
        // 点击时重新读取同级 Data，避免使用已修改或已删除账号的列表缓存。
        let store = load()?;
        let (name, account) = store.accounts.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(account_name.trim()))
            .ok_or_else(|| error("not_found", "账号已删除，请刷新列表", false))?;
        crate::wegame::fill_credentials(name, &account.password)
            .map_err(|message| error("wegame_fill_error", &message, false))
    }).await.map_err(|_| error("wegame_fill_error", "填写任务异常结束，请重试", false))?
}

#[tauri::command]
pub fn lol_backup_get() -> Result<Snapshot> { Ok(snapshot(&load_backup()?)) }

#[cfg(test)]
fn target_path(value: &str) -> String {
    value.split('/').filter(|part| !part.is_empty() && *part != ".").collect::<Vec<_>>().join("/")
}
#[cfg(test)]
fn targets_collide(a: &Config, b: &Config) -> bool {
    !a.host.trim().is_empty() && !b.host.trim().is_empty()
        && a.host.trim().eq_ignore_ascii_case(b.host.trim()) && a.port == b.port
        && a.username.trim() == b.username.trim() && target_path(&a.remote_path) == target_path(&b.remote_path)
}
#[cfg(test)]
fn check_target(a: &Config, b: &Config) -> Result<()> {
    if targets_collide(a, b) { Err(error("config_error", "LOL 与 Steam 备份必须使用不同远程文件，避免账号混写", false)) } else { Ok(()) }
}
#[tauri::command]
pub fn lol_backup_save(config: Config) -> Result<Snapshot> {
    let settings = backup_settings::legacy_settings(Game::Lol, &config)?;
    backup_settings::shared_backup_save(settings)?;
    lol_backup_get()
}
#[tauri::command]
pub async fn lol_backup_test(config: Config) -> Result<TestReport> {
    let settings = backup_settings::legacy_settings(Game::Lol, &config)?;
    backup_settings::shared_backup_test(settings).await
}

fn batch(store: &Store, target: Option<&str>) -> Vec<(String, String)> {
    store.accounts.iter().filter(|(name, a)| target.is_none_or(|t| name.eq_ignore_ascii_case(t))
        && backup::valid_credential(name, &a.password)
        && a.synced_digest.as_ref() != Some(&backup::credential_digest(name, &a.password)))
        .map(|(name, a)| (name.clone(), a.password.clone())).collect()
}
fn automatic_batch(store: &Store) -> Vec<(String, String)> {
    if store.blocked || now() < store.next_retry_ms { return Vec::new(); }
    batch(store, None).into_iter().filter(|(name, _)| store.config.enabled
        || store.manual_pending.contains(&name.to_ascii_lowercase())).collect()
}
fn acknowledge(store: &mut Store, revision: u64, sent: &[(String, String)]) -> bool {
    if store.revision != revision { return false; }
    for (name, password) in sent {
        if let Some(a) = store.accounts.get_mut(name).filter(|a| &a.password == password) {
            a.synced_digest = Some(backup::credential_digest(name, password));
            store.manual_pending.remove(&name.to_ascii_lowercase());
        }
    }
    true
}
async fn sync(force: bool, account: Option<&str>) -> Result<Snapshot> {
    let _sync = SYNC_LOCK.lock().await;
    let _transfer = backup::TRANSFER_LOCK.lock().await;
    let saved = load_backup()?;
    let pending = if force { batch(&saved, account) } else { automatic_batch(&saved) };
    if pending.is_empty() {
        if force { edit_backup(|s| { s.message = "暂无待同步的 LOL 账号".into(); Ok(()) })?; return publish(); }
        return Ok(snapshot(&saved));
    }
    backup::validate_config(&saved.config)?;
    if force {
        edit_backup(|s| {
            if s.revision != saved.revision { return Err(error("config_changed", "账号或配置已改变，请重试", false)); }
            s.manual_pending.extend(pending.iter().map(|(name, _)| name.to_ascii_lowercase()));
            Ok(())
        })?;
    }
    edit_backup(|s| {
        if s.revision != saved.revision { return Err(error("config_changed", "账号或配置已改变，请重试", false)); }
        s.status = "syncing".into(); s.message = "正在备份 LOL 账号".into(); Ok(())
    })?;
    publish()?;
    let result = backup::upload_credentials(&saved.config, &CLIENT, &pending, |fingerprint| {
        backup_settings::pin(Game::Lol, &saved.config, fingerprint)?;
        edit_backup(|s| {
            if s.revision != saved.revision { return Err(error("config_changed", "账号或备份配置已改变，请重试", false)); }
            Ok(())
        })
    }).await;
    backup_settings::verification(Game::Lol, &saved.config, saved.shared_generation, result.is_ok())?;
    let mut obsolete = false;
    edit_backup(|s| {
        if s.revision != saved.revision { obsolete = true; return Ok(()); }
        match &result {
            Ok(()) => {
                acknowledge(s, saved.revision, &pending);
                s.verified = true; s.failures = 0; s.blocked = false; s.next_retry_ms = 0;
                s.last_success_ms = Some(now()); s.status = "idle".into();
                s.message = format!("同步成功：已备份 {} 个 LOL 账号", pending.len());
            }
            Err(e) => {
                s.verified = false; s.failures = s.failures.saturating_add(1); s.blocked = !e.retryable;
                s.status = e.code.clone(); s.message = e.message.clone();
                s.next_retry_ms = if e.retryable { now() + i64::from(30 * (1u32 << s.failures.min(4))).min(300) * 1000 } else { 0 };
            }
        }
        Ok(())
    })?;
    let snap = publish()?;
    if obsolete { WAKE.notify_one(); return Err(error("config_changed", "同步期间账号或配置已改变，请重试", false)); }
    result?;
    Ok(snap)
}
#[tauri::command]
pub async fn lol_backup_sync(account: Option<String>) -> Result<Snapshot> { sync(true, account.as_deref()).await }
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
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self { let dir = std::env::temp_dir().join(format!("gameready-lol-test-{}", uuid::Uuid::new_v4())); fs::create_dir(&dir).unwrap(); Self(dir) }
        fn path(&self) -> PathBuf { self.0.join("lol-accounts.json") }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn shared_fixture(config: &Config, generation: u64, verified: bool) -> Resolved {
        Resolved { config: config.clone(), digest: backup_settings::target_digest(config), generation, verified, suspended: false }
    }
    #[test]
    fn shared_migration_and_target_changes_keep_local_accounts_favorites_and_pending() {
        let f = Fixture::new(); let p = f.path(); save_account_at(&p, "fixture-player", "fixture-password").unwrap();
        let config = Config { enabled: true, host: "nas.example.test".into(), username: "fixture".into(), password: "fixture-nas".into(), remote_path: "/lol.txt".into(), ..Default::default() };
        let mut store = read(&p).unwrap(); store.config = config.clone(); store.accounts.get_mut("fixture-player").unwrap().favorite = true;
        store.accounts.get_mut("fixture-player").unwrap().synced_digest = Some("receipt".into()); store.manual_pending.insert("fixture-player".into());
        apply_shared(&mut store, shared_fixture(&config, 1, true)); assert_eq!(store.accounts["fixture-player"].synced_digest.as_deref(), Some("receipt"));
        write_backup_store(&p, &store).unwrap(); let mut loaded = read(&p).unwrap(); assert!(loaded.config.password.is_empty() && loaded.config.host.is_empty());
        assert!(!apply_shared(&mut loaded, shared_fixture(&config, 1, true)));
        let mut changed = config.clone(); changed.remote_path = "/new-lol.txt".into(); apply_shared(&mut loaded, shared_fixture(&changed, 2, false));
        assert!(loaded.accounts["fixture-player"].synced_digest.is_none()); assert!(loaded.accounts["fixture-player"].favorite);
        assert_eq!(loaded.accounts["fixture-player"].password, "fixture-password"); assert!(loaded.manual_pending.contains("fixture-player"));
        // Local CRUD uses the raw store even if a sibling shared configuration is damaged.
        fs::write(f.0.join("backup-settings.json"), b"broken").unwrap(); save_account_at(&p, "other-fixture", "fixture-value").unwrap();
        assert!(read(&p).unwrap().accounts.contains_key("other-fixture"));
    }
    #[test]
    fn shared_verification_preserves_inflight_revision_and_resumes_retry() {
        let config = default_config(); let mut store = Store::default(); apply_shared(&mut store, shared_fixture(&config, 1, false));
        store.accounts.insert("fixture-player".into(), SavedAccount { password: "fixture-password".into(), ..Default::default() });
        let sent = batch(&store, None); let revision = store.revision; store.status = "syncing".into(); store.blocked = true; store.failures = 2; store.next_retry_ms = 100;
        apply_shared(&mut store, shared_fixture(&config, 1, true));
        assert_eq!(store.revision, revision); assert!(!store.blocked); assert_eq!(store.status, "syncing"); assert!(acknowledge(&mut store, revision, &sent));
        assert!(batch(&store, None).is_empty()); assert_eq!(store.failures, 0); assert_eq!(store.next_retry_ms, 0);
    }
    #[test]
    fn accounts_remain_isolated_preserve_favorites_and_reject_corruption() {
        let f = Fixture::new(); let p = f.path();
        fs::write(f.0.join("accounts.json"), b"Steam fixture untouched").unwrap();
        save_account_at(&p, "Player", "first").unwrap();
        edit_at(&p, |s| { s.accounts.get_mut("Player").unwrap().favorite = true; s.extra.insert("future".into(), Value::Bool(true)); Ok(()) }).unwrap();
        save_account_at(&p, "PLAYER", "second").unwrap();
        let s = read(&p).unwrap();
        assert_eq!(s.accounts.len(), 1); assert!(s.accounts["Player"].favorite); assert_eq!(s.accounts["Player"].password, "second");
        assert_eq!(s.extra["future"], true);
        assert_eq!(fs::read(f.0.join("accounts.json")).unwrap(), b"Steam fixture untouched");
        fs::write(&p, b"broken").unwrap();
        assert!(save_account_at(&p, "other", "secret").is_err()); assert_eq!(fs::read(&p).unwrap(), b"broken");
    }
    #[test]
    fn stale_sync_receipts_do_not_restore_deleted_or_changed_credentials() {
        let mut s = Store::default();
        s.accounts.insert("player".into(), SavedAccount { password: "old".into(), ..Default::default() });
        let pending = batch(&s, None); let revision = s.revision;
        s.accounts.get_mut("player").unwrap().password = "new".into(); changed(&mut s);
        assert!(!acknowledge(&mut s, revision, &pending)); assert!(s.accounts["player"].synced_digest.is_none());
        s.accounts.clear(); assert!(!acknowledge(&mut s, revision, &pending)); assert!(s.accounts.is_empty());
        s.accounts.insert("other".into(), SavedAccount { password: "saved".into(), ..Default::default() });
        let pending = batch(&s, None); let revision = s.revision;
        assert!(acknowledge(&mut s, revision, &pending)); assert!(batch(&s, None).is_empty());
    }
    #[test]
    fn backup_targets_cannot_alias_and_default_path_is_separate() {
        let mut a = default_config(); a.host = "nas.example.test".into(); a.username = "fixture".into();
        let mut b = a.clone(); b.host = "NAS.EXAMPLE.TEST".into(); b.remote_path = "/GameReady/./lol-accounts.txt".into();
        assert!(check_target(&a, &b).is_err()); b.remote_path = "/GameReady/accounts.txt".into(); assert!(check_target(&a, &b).is_ok());
    }
    #[test]
    fn manual_retry_is_durable_scoped_and_survives_auto_backup_disabled() {
        let f = Fixture::new(); let p = f.path();
        save_account_at(&p, "Player", "fixture-one").unwrap();
        save_account_at(&p, "Other", "fixture-two").unwrap();
        edit_at(&p, |s| { s.config.enabled = false; s.manual_pending.insert("player".into()); Ok(()) }).unwrap();
        let mut restored = read(&p).unwrap();
        assert_eq!(automatic_batch(&restored), vec![("Player".into(), "fixture-one".into())]);
        restored.next_retry_ms = now() + 60_000; assert!(automatic_batch(&restored).is_empty());
        restored.next_retry_ms = 0; restored.blocked = true; assert!(automatic_batch(&restored).is_empty());
        restored.blocked = false;
        let sent = automatic_batch(&restored); let revision = restored.revision;
        assert!(acknowledge(&mut restored, revision, &sent)); assert!(automatic_batch(&restored).is_empty());
        assert!(restored.manual_pending.is_empty());
        save_account_at(&p, "PLAYER", "changed-fixture").unwrap();
        assert!(automatic_batch(&read(&p).unwrap()).is_empty(), "Editing a manually queued credential does not authorize uploading its replacement when auto backup is disabled");
    }
}
