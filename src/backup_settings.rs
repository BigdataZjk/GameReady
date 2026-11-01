//! 公共 SFTP 连接与各游戏目标的唯一活动配置。
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Emitter;
use crate::backup::{self, Config, Error, TestReport, error};

type Result<T> = std::result::Result<T, Error>;
static LOCK: Mutex<()> = Mutex::new(());
static TESTED: Mutex<Option<Tested>> = Mutex::new(None);
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Game { Steam, Lol }
impl Game { fn key(self) -> &'static str { match self { Self::Steam => "steam", Self::Lol => "lol" } } }

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Connection { pub host: String, pub port: u16, pub username: String, pub password: String, pub fingerprint: String }
impl Default for Connection {
    fn default() -> Self { Self { host: String::new(), port: 2022, username: String::new(), password: String::new(), fingerprint: String::new() } }
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Destination { pub enabled: bool, pub remote_path: String }
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Games { pub steam: Destination, pub lol: Destination }
impl Default for Games {
    fn default() -> Self { Self { steam: Destination { enabled: false, remote_path: "/GameReady/accounts.txt".into() },
        lol: Destination { enabled: false, remote_path: "/GameReady/lol-accounts.txt".into() } } }
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings { pub connection: Connection, pub games: Games }
#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub state_version: u64, pub connection: Connection, pub games: Games, pub migration_warning: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)] state_version: u64,
    #[serde(default)] generation: u64,
    #[serde(flatten)] settings: Settings,
    #[serde(default)] migration_warning: Option<String>,
    #[serde(default)] verified_targets: BTreeMap<String, String>,
}
impl Document {
    fn new(settings: Settings) -> Self { Self { version: 1, state_version: 1, generation: 1, settings, migration_warning: None, verified_targets: BTreeMap::new() } }
    fn snapshot(&self) -> Snapshot { Snapshot { state_version: self.state_version, connection: self.settings.connection.clone(),
        games: self.settings.games.clone(), migration_warning: self.migration_warning.clone() } }
}
#[derive(Clone)]
struct Tested { settings: Settings, generation: u64, fingerprint: String, targets: Vec<Game> }
enum Update { All(Settings), Connection(Connection), Destination(Game, Destination) }
#[derive(Clone, Copy)]
enum Scope { All, Connection, Destination(Game) }
pub(crate) struct Resolved { pub config: Config, pub digest: String, pub generation: u64, pub verified: bool, pub suspended: bool }

fn path() -> PathBuf { crate::data_dir().join("backup-settings.json") }
fn local(message: &str) -> Error { error("local_error", message, false) }
fn write(path: &Path, document: &Document) -> Result<()> { crate::atomic_json(path, document).map_err(|_| local("保存公共备份配置失败，原文件已保留")) }
fn config(settings: &Settings, game: Game) -> Config {
    let connection = &settings.connection;
    let destination = match game { Game::Steam => &settings.games.steam, Game::Lol => &settings.games.lol };
    Config { enabled: destination.enabled, host: connection.host.clone(), port: connection.port,
        username: connection.username.clone(), password: connection.password.clone(), fingerprint: connection.fingerprint.clone(),
        remote_path: destination.remote_path.clone() }
}
fn connection(config: &Config) -> Connection { Connection { host: config.host.clone(), port: config.port, username: config.username.clone(), password: config.password.clone(), fingerprint: config.fingerprint.clone() } }
fn same_login(a: &Connection, b: &Connection) -> bool { a.host.eq_ignore_ascii_case(&b.host) && a.port == b.port && a.username == b.username && a.password == b.password }
fn same_settings(a: &Settings, b: &Settings) -> bool { same_login(&a.connection, &b.connection) && a.games == b.games }
fn usable(c: &Connection) -> bool { !c.host.is_empty() && c.port != 0 && !c.username.is_empty() && !c.password.is_empty() }
fn has_connection(c: &Connection) -> bool { !c.host.is_empty() || !c.username.is_empty() || !c.password.is_empty() || !c.fingerprint.is_empty() }

pub(crate) fn normalized_path(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() { return Ok(String::new()); }
    if !value.starts_with('/') || value.ends_with('/') || value.contains(['\r', '\n', '\0', '\\']) || value.split('/').any(|part| part == "..") {
        return Err(error("config_error", "备份文件请填写 SFTP 绝对文件路径，例如 /GameReady/steam-accounts.txt", false));
    }
    let parts: Vec<_> = value.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    if parts.is_empty() { return Err(error("config_error", "备份路径必须指向文件，不能只填写目录", false)); }
    Ok(format!("/{}", parts.join("/")))
}
fn normalize(settings: &mut Settings) -> Result<()> {
    settings.connection.host = settings.connection.host.trim().into();
    settings.connection.username = settings.connection.username.trim().into();
    settings.games.steam.remote_path = normalized_path(&settings.games.steam.remote_path)?;
    settings.games.lol.remote_path = normalized_path(&settings.games.lol.remote_path)?;
    if !settings.games.steam.remote_path.is_empty() && settings.games.steam.remote_path == settings.games.lol.remote_path {
        return Err(error("config_error", "Steam 和 LOL 必须使用不同的远程备份文件", false));
    }
    for game in [Game::Steam, Game::Lol] {
        let cfg = config(settings, game);
        if cfg.enabled { backup::validate_config(&cfg)?; }
    }
    Ok(())
}
pub(crate) fn target_digest(config: &Config) -> String {
    // 首次主机密钥固定不改变传输目标代次，避免本次上传回执被自身的 TOFU 写入作废。
    let mut hash = Sha256::new();
    for value in [config.host.trim().to_ascii_lowercase(), config.port.to_string(), config.username.trim().into(),
        config.password.clone(), normalized_path(&config.remote_path).unwrap_or_else(|_| config.remote_path.clone())] {
        hash.update((value.len() as u64).to_le_bytes()); hash.update(value.as_bytes());
    }
    format!("{:x}", hash.finalize())
}
pub(crate) fn same_target(a: &Config, b: &Config) -> bool { target_digest(a) == target_digest(b) }

fn legacy_at(folder: &Path) -> Result<[(Config, bool); 2]> {
    let steam_value = crate::account_store::read_backup(&folder.join("accounts.json")).map_err(|_| local("旧 Steam 账号文件损坏，无法迁移公共备份配置，原文件已保留"))?;
    let steam: Config = serde_json::from_value(steam_value.get("config").cloned().unwrap_or(serde_json::json!({})))
        .map_err(|_| local("旧 Steam 备份配置损坏，原文件已保留"))?;
    let (lol, lol_verified) = crate::lol_accounts::legacy_backup_at(&folder.join("lol-accounts.json"))?;
    Ok([(steam, steam_value.get("verified").and_then(serde_json::Value::as_bool).unwrap_or(false)),
        (lol, lol_verified)])
}
fn migrated(legacy: [(Config, bool); 2]) -> Document {
    let [(mut steam, steam_verified), (mut lol, lol_verified)] = legacy;
    for cfg in [&mut steam, &mut lol] { cfg.host = cfg.host.trim().into(); cfg.username = cfg.username.trim().into(); cfg.remote_path = cfg.remote_path.trim().into(); }
    let a = connection(&steam); let b = connection(&lol);
    let both = has_connection(&a) && has_connection(&b);
    let conflict = both && (!same_login(&a, &b) || (!a.fingerprint.is_empty() && !b.fingerprint.is_empty() && a.fingerprint != b.fingerprint));
    let mut settings = Settings::default();
    settings.games.steam = Destination { enabled: steam.enabled, remote_path: if steam.remote_path.is_empty() { settings.games.steam.remote_path } else { steam.remote_path.clone() } };
    settings.games.lol = Destination { enabled: lol.enabled, remote_path: if lol.remote_path.is_empty() { settings.games.lol.remote_path } else { lol.remote_path.clone() } };
    let mut warning = None;
    if conflict {
        warning = Some("检测到 Steam 与 LOL 的旧 SFTP 连接或主机密钥不同，自动备份已暂停。请在这里明确填写统一连接并保存；原账号和待同步记录均已保留。".into());
    } else {
        settings.connection = if has_connection(&a) { a.clone() } else { b.clone() };
        if settings.connection.fingerprint.is_empty() && both { settings.connection.fingerprint = b.fingerprint.clone(); }
        if has_connection(&settings.connection) && !usable(&settings.connection) {
            warning = Some("旧 SFTP 连接不完整，自动备份已暂停，请补全公共连接并保存。".into());
        }
    }
    if (steam.enabled && steam.remote_path.is_empty()) || (lol.enabled && lol.remote_path.is_empty()) {
        warning.get_or_insert_with(|| "旧自动备份没有有效的文件路径，已暂停。请明确填写各游戏的备份文件后保存。".into());
    }
    // 路径原样保留供用户修正；有冲突或非法路径时不能自动开始上传。
    let mut normalized = settings.clone();
    if normalize(&mut normalized).is_err() {
        warning.get_or_insert_with(|| "旧备份路径重复或无效，自动备份已暂停，请为各游戏填写不同的文件路径后保存。".into());
    } else { settings = normalized; }
    if warning.is_some() { settings.games.steam.enabled = false; settings.games.lol.enabled = false; }
    let mut doc = Document::new(settings);
    doc.migration_warning = warning;
    if doc.migration_warning.is_none() {
        for (game, old, verified) in [(Game::Steam, steam, steam_verified), (Game::Lol, lol, lol_verified)] {
            let current = config(&doc.settings, game);
            if verified && same_target(&old, &current) { doc.verified_targets.insert(game.key().into(), target_digest(&current)); }
        }
    }
    doc
}
fn load_at(file: &Path) -> Result<Document> {
    match fs::read(file) {
        Ok(bytes) => {
            let doc: Document = serde_json::from_slice(&bytes).map_err(|_| local("Data/backup-settings.json 格式损坏，原文件已保留"))?;
            if doc.version != 1 { return Err(local("公共备份配置版本不受支持，原文件已保留")); }
            Ok(doc)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let folder = file.parent().ok_or_else(|| local("公共备份目录无效"))?;
            let doc = migrated(legacy_at(folder)?);
            write(file, &doc)?; Ok(doc)
        }
        Err(_) => Err(local("读取公共备份配置失败，原文件已保留")),
    }
}
fn publish(doc: &Document) {
    if let Some(app) = APP.get() {
        let mut snapshot = doc.snapshot(); snapshot.connection.password.clear();
        let _ = app.emit("shared-backup-changed", snapshot);
    }
    backup::wake_shared_settings(); crate::lol_accounts::wake_shared_settings();
}
pub(crate) fn resolved(game: Game) -> Result<Resolved> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let doc = load_at(&path())?;
    resolve_document(&doc, game)
}
fn resolve_document(doc: &Document, game: Game) -> Result<Resolved> {
    let mut active = doc.settings.clone();
    if doc.migration_warning.is_none() { normalize(&mut active)?; }
    let mut cfg = config(&active, game);
    // 冲突不止禁自动开关；手动失败重试也不得绕过迁移暂停。
    if doc.migration_warning.is_some() { cfg.host.clear(); cfg.password.clear(); cfg.enabled = false; }
    let digest = target_digest(&cfg);
    Ok(Resolved { verified: doc.verified_targets.get(game.key()) == Some(&digest), config: cfg, digest, generation: doc.generation, suspended: doc.migration_warning.is_some() })
}
#[tauri::command]
pub fn shared_backup_get() -> Result<Snapshot> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    Ok(load_at(&path())?.snapshot())
}
fn destination_mut(settings: &mut Settings, game: Game) -> &mut Destination {
    match game { Game::Steam => &mut settings.games.steam, Game::Lol => &mut settings.games.lol }
}
fn connection_config(settings: &Settings) -> Config {
    let mut cfg = config(settings, Game::Steam);
    // 这里只为复用连接字段校验提供一个合法路径；连接测试不会探测此文件。
    cfg.remote_path = "/connection-test.txt".into(); cfg
}
fn scoped_settings(saved: &Document, update: Update) -> Result<(Settings, Scope)> {
    let mut settings = saved.settings.clone();
    let scope = match update {
        Update::All(value) => { settings = value; normalize(&mut settings)?; Scope::All }
        Update::Connection(mut value) => {
            value.host = value.host.trim().into(); value.username = value.username.trim().into();
            settings.connection = value;
            backup::validate_config(&connection_config(&settings))?;
            Scope::Connection
        }
        Update::Destination(game, mut value) => {
            value.remote_path = normalized_path(&value.remote_path)?;
            if value.enabled && value.remote_path.is_empty() {
                return Err(error("config_error", "开启自动备份前请填写当前游戏的远程文件路径", false));
            }
            let other = config(&settings, if game == Game::Steam { Game::Lol } else { Game::Steam });
            if !value.remote_path.is_empty() && normalized_path(&other.remote_path).ok().as_ref() == Some(&value.remote_path) {
                return Err(error("config_error", "Steam 和 LOL 必须使用不同的远程备份文件", false));
            }
            *destination_mut(&mut settings, game) = value;
            Scope::Destination(game)
        }
    };
    // 主机指纹只能来自已经固定的连接或实际测试结果，不能由表单覆盖。
    settings.connection.fingerprint = if settings.connection.host.eq_ignore_ascii_case(&saved.settings.connection.host) && settings.connection.port == saved.settings.connection.port {
        saved.settings.connection.fingerprint.clone()
    } else { String::new() };
    Ok((settings, scope))
}
fn apply_saved_settings(doc: &mut Document, mut settings: Settings, scope: Scope, tested: Option<&Tested>) -> Result<()> {
    if matches!(scope, Scope::All) && doc.migration_warning.is_some() {
        backup::validate_config(&connection_config(&settings))?;
    }
    let tested = tested.filter(|value| value.generation == doc.generation && same_settings(&value.settings, &settings)
        && (settings.connection.fingerprint.is_empty() || settings.connection.fingerprint == value.fingerprint));
    if settings.connection.fingerprint.is_empty() {
        if let Some(tested) = tested { settings.connection.fingerprint = tested.fingerprint.clone(); }
    }
    // 分页配置可逐项修复旧数据；未修好的另一页不会阻止保存，但仍暂停传输。
    let mut checked = settings.clone();
    let valid_targets = normalize(&mut checked).is_ok();
    let valid_connection = backup::validate_config(&connection_config(&settings)).is_ok();
    let needs_confirmation = doc.migration_warning.is_some() && !valid_connection;
    doc.migration_warning = if !valid_targets || needs_confirmation {
        Some(if !valid_connection { "备份已暂停，请在设置页补全并保存公共 SFTP 连接；各游戏的文件路径在各自账号备份页填写。" }
            else { "备份已暂停，请到各游戏账号备份页修正重复或无效的文件路径并保存。" }.into())
    } else { None };
    let verified = doc.verified_targets.clone();
    doc.settings = settings; doc.verified_targets.clear();
    if doc.migration_warning.is_none() {
        for game in [Game::Steam, Game::Lol] {
            let cfg = config(&doc.settings, game); let digest = target_digest(&cfg);
            if !cfg.remote_path.is_empty() && (tested.is_some_and(|value| value.targets.contains(&game)) || verified.get(game.key()) == Some(&digest)) {
                doc.verified_targets.insert(game.key().into(), digest);
            }
        }
    }
    doc.generation = doc.generation.saturating_add(1); doc.state_version = doc.state_version.saturating_add(1);
    Ok(())
}
fn save_update(update: Update) -> Result<Snapshot> {
    let _transfer = backup::TRANSFER_LOCK.try_lock().map_err(|_| error("backup_busy", "账号备份正在传输或测试，请完成后再修改配置", false))?;
    let document = {
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut doc = load_at(&path())?;
        let (settings, scope) = scoped_settings(&doc, update)?;
        let tested = TESTED.lock().unwrap_or_else(|p| p.into_inner()).clone();
        apply_saved_settings(&mut doc, settings, scope, tested.as_ref())?;
        write(&path(), &doc)?; *TESTED.lock().unwrap_or_else(|p| p.into_inner()) = None; doc
    };
    publish(&document); Ok(document.snapshot())
}
#[tauri::command]
pub fn shared_backup_save(settings: Settings) -> Result<Snapshot> { save_update(Update::All(settings)) }
#[tauri::command]
pub fn backup_connection_save(connection: Connection) -> Result<Snapshot> { save_update(Update::Connection(connection)) }
#[tauri::command]
pub fn backup_destination_save(game: Game, destination: Destination) -> Result<Snapshot> { save_update(Update::Destination(game, destination)) }

fn test_targets(settings: &Settings, scope: Scope) -> Result<Vec<Game>> {
    backup::validate_config(&connection_config(settings))?;
    let targets = match scope {
        Scope::Connection => Vec::new(),
        Scope::Destination(game) => { backup::validate_config(&config(settings, game))?; vec![game] }
        Scope::All => [Game::Steam, Game::Lol].into_iter().filter(|game| !config(settings, *game).remote_path.is_empty()).collect(),
    };
    Ok(targets)
}
fn accept_test_result(current: &mut Document, settings: &Settings, scope: Scope, targets: &[Game], result: &Result<TestReport>) -> bool {
    let matches = match scope {
        Scope::Connection => same_login(&current.settings.connection, &settings.connection),
        Scope::Destination(game) => same_target(&config(&current.settings, game), &config(settings, game)),
        Scope::All => same_settings(&current.settings, settings),
    };
    if !matches { return false; }
    match result {
        Ok(report) => {
            if current.settings.connection.fingerprint.is_empty() { current.settings.connection.fingerprint = report.fingerprint.clone(); }
            if current.migration_warning.is_none() {
                for game in targets { current.verified_targets.insert(game.key().into(), target_digest(&config(settings, *game))); }
            }
        }
        Err(_) => match scope {
            Scope::Destination(game) => { current.verified_targets.remove(game.key()); }
            Scope::Connection | Scope::All => current.verified_targets.clear(),
        }
    }
    current.state_version = current.state_version.saturating_add(1); true
}
async fn test_update(update: Update) -> Result<TestReport> {
    let _transfer = backup::TRANSFER_LOCK.try_lock().map_err(|_| error("backup_busy", "账号备份正在传输或测试，请稍后重试", false))?;
    let saved = { let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner()); load_at(&path())? };
    let (mut settings, scope) = scoped_settings(&saved, update)?;
    let targets = test_targets(&settings, scope)?;
    let paths: Vec<_> = targets.iter().map(|game| config(&settings, *game).remote_path).collect();
    let mut result = backup::test_shared_connection(&connection_config(&settings), &paths).await;
    if let Ok(report) = &mut result {
        report.message = match scope {
            Scope::Connection => "公共 SFTP 连接验证通过；各游戏文件路径请在各自账号备份页测试".into(),
            Scope::Destination(_) => "当前游戏备份文件的写入、替换及读回验证通过".into(),
            Scope::All => report.message.clone(),
        };
    }
    let document = {
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut current = load_at(&path())?;
        if current.generation != saved.generation || !same_settings(&current.settings, &saved.settings)
            || current.settings.connection.fingerprint != saved.settings.connection.fingerprint {
            return Err(error("config_changed", "测试期间公共配置已改变，请按当前配置重新测试", false));
        }
        match &result {
            Ok(report) => {
                settings.connection.fingerprint = report.fingerprint.clone();
                *TESTED.lock().unwrap_or_else(|p| p.into_inner()) = Some(Tested { settings: settings.clone(), generation: current.generation,
                    fingerprint: report.fingerprint.clone(), targets: targets.clone() });
            }
            Err(_) => { *TESTED.lock().unwrap_or_else(|p| p.into_inner()) = None; }
        }
        if accept_test_result(&mut current, &settings, scope, &targets, &result) { write(&path(), &current)?; }
        current
    };
    publish(&document); result
}
#[tauri::command]
pub async fn shared_backup_test(settings: Settings) -> Result<TestReport> { test_update(Update::All(settings)).await }
#[tauri::command]
pub async fn backup_connection_test(connection: Connection) -> Result<TestReport> { test_update(Update::Connection(connection)).await }
#[tauri::command]
pub async fn backup_destination_test(game: Game, destination: Destination) -> Result<TestReport> { test_update(Update::Destination(game, destination)).await }

pub(crate) fn pin(game: Game, expected: &Config, fingerprint: &str) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut doc = load_at(&path())?;
    if doc.migration_warning.is_some() || !same_target(&config(&doc.settings, game), expected) { return Err(error("config_changed", "公共备份目标已改变，已停止上传", false)); }
    if !doc.settings.connection.fingerprint.is_empty() && doc.settings.connection.fingerprint != fingerprint {
        return Err(error("host_key_changed", "SFTP 主机密钥已改变，已停止上传", false));
    }
    if doc.settings.connection.fingerprint.is_empty() {
        doc.settings.connection.fingerprint = fingerprint.into(); doc.state_version = doc.state_version.saturating_add(1); write(&path(), &doc)?;
    }
    Ok(())
}
pub(crate) fn verification(game: Game, expected: &Config, generation: u64, verified: bool) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut doc = load_at(&path())?;
    if doc.generation != generation || !same_target(&config(&doc.settings, game), expected) { return Err(error("config_changed", "公共备份配置已改变，请重新同步", false)); }
    if verified { doc.verified_targets.insert(game.key().into(), target_digest(expected)); }
    else { doc.verified_targets.remove(game.key()); }
    doc.state_version = doc.state_version.saturating_add(1); write(&path(), &doc)
}
pub(crate) fn legacy_settings(game: Game, config: &Config) -> Result<Settings> {
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut doc = load_at(&path())?;
    if !same_login(&doc.settings.connection, &connection(config)) {
        return Err(error("config_error", "SFTP 连接为公共设置，请到设置中的账号备份修改", false));
    }
    let destination = Destination { enabled: config.enabled, remote_path: config.remote_path.clone() };
    match game { Game::Steam => doc.settings.games.steam = destination, Game::Lol => doc.settings.games.lol = destination }
    Ok(doc.settings)
}
pub fn start(app: tauri::AppHandle) { let _ = APP.set(app); }

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(path: &str) -> Config { Config { enabled: true, host: "nas.example.test".into(), port: 2022, username: "fixture".into(), password: "fixture-secret".into(), remote_path: path.into(), fingerprint: "SHA256:fixture".into() } }
    fn saved_fixture() -> Document { migrated([(fixture("/steam.txt"), true), (fixture("/lol.txt"), true)]) }
    fn patch(doc: &mut Document, update: Update) {
        let (settings, scope) = scoped_settings(doc, update).unwrap();
        apply_saved_settings(doc, settings, scope, None).unwrap();
    }
    #[test]
    fn scoped_saves_keep_the_latest_other_page_and_its_verification() {
        let mut doc = saved_fixture();
        let original_connection = doc.settings.connection.clone();
        let lol_receipt = doc.verified_targets["lol"].clone();
        patch(&mut doc, Update::Destination(Game::Steam, Destination { enabled: false, remote_path: "/steam-new.txt".into() }));
        assert!(doc.settings.connection == original_connection); assert!(!doc.settings.games.steam.enabled);
        assert_eq!(doc.settings.games.lol.remote_path, "/lol.txt"); assert_eq!(doc.verified_targets["lol"], lol_receipt);
        assert!(!doc.verified_targets.contains_key("steam"));
        let mut edited_connection = original_connection; edited_connection.username = "new-fixture".into();
        patch(&mut doc, Update::Connection(edited_connection));
        assert_eq!(doc.settings.games.steam.remote_path, "/steam-new.txt"); assert_eq!(doc.settings.games.lol.remote_path, "/lol.txt");
        assert!(!doc.settings.games.steam.enabled && doc.settings.games.lol.enabled); assert!(doc.verified_targets.is_empty());
    }
    #[test]
    fn scoped_tests_check_only_the_requested_connection_or_destination() {
        let mut doc = saved_fixture(); doc.settings.games.lol.remote_path = "invalid-old-path".into();
        let (settings, scope) = scoped_settings(&doc, Update::Connection(doc.settings.connection.clone())).unwrap();
        assert!(test_targets(&settings, scope).unwrap().is_empty());
        let (settings, scope) = scoped_settings(&doc, Update::Destination(Game::Steam, doc.settings.games.steam.clone())).unwrap();
        assert_eq!(test_targets(&settings, scope).unwrap(), vec![Game::Steam]);
        assert!(scoped_settings(&doc, Update::Destination(Game::Lol, Destination { enabled: true, remote_path: "//./steam.txt".into() })).is_err());
        let (settings, scope) = scoped_settings(&doc, Update::Destination(Game::Lol, Destination::default())).unwrap();
        assert!(test_targets(&settings, scope).is_err());
    }
    #[test]
    fn verification_is_limited_to_the_tested_game_and_connection_test_never_verifies_files() {
        let mut doc = saved_fixture(); doc.verified_targets.clear();
        let settings = doc.settings.clone();
        let success = Ok(TestReport { fingerprint: "SHA256:fixture".into(), message: "fixture".into() });
        assert!(accept_test_result(&mut doc, &settings, Scope::Connection, &[], &success));
        assert!(doc.verified_targets.is_empty());
        assert!(accept_test_result(&mut doc, &settings, Scope::Destination(Game::Steam), &[Game::Steam], &success));
        assert!(doc.verified_targets.contains_key("steam")); assert!(!doc.verified_targets.contains_key("lol"));
        doc.verified_targets.insert("lol".into(), target_digest(&config(&settings, Game::Lol)));
        let failure = Err(error("path_error", "fixture failure", false));
        let mut toggled = settings.clone(); toggled.games.steam.enabled = false;
        assert!(accept_test_result(&mut doc, &toggled, Scope::Destination(Game::Steam), &[Game::Steam], &failure));
        assert!(!doc.verified_targets.contains_key("steam")); assert!(doc.verified_targets.contains_key("lol"));
        let mut draft = settings.clone(); draft.games.lol.remote_path = "/unsaved-path.txt".into();
        assert!(!accept_test_result(&mut doc, &draft, Scope::Destination(Game::Lol), &[Game::Lol], &failure));
        assert!(doc.verified_targets.contains_key("lol"));
    }
    #[test]
    fn successful_test_cache_does_not_verify_unprobed_destinations_when_saved() {
        for targets in [Vec::new(), vec![Game::Steam]] {
            let mut doc = saved_fixture(); doc.verified_targets.clear(); doc.settings.connection.fingerprint.clear();
            let settings = doc.settings.clone();
            let tested = Tested { settings: settings.clone(), generation: doc.generation, fingerprint: "SHA256:tested".into(), targets: targets.clone() };
            apply_saved_settings(&mut doc, settings, Scope::Connection, Some(&tested)).unwrap();
            assert_eq!(doc.settings.connection.fingerprint, "SHA256:tested");
            assert_eq!(doc.verified_targets.contains_key("steam"), targets.contains(&Game::Steam));
            assert!(!doc.verified_targets.contains_key("lol"));
        }
    }
    #[test]
    fn migration_can_repair_both_game_paths_and_connection_in_either_order() {
        for connection_first in [false, true] {
            let mut doc = Document::new(Settings::default());
            doc.settings.games.steam.remote_path = "invalid-steam".into(); doc.settings.games.lol.remote_path = "invalid-lol".into();
            doc.migration_warning = Some("fixture conflict".into());
            if connection_first { patch(&mut doc, Update::Connection(connection(&fixture("/unused")))); assert!(doc.migration_warning.is_some()); }
            patch(&mut doc, Update::Destination(Game::Steam, Destination { enabled: false, remote_path: "/fixed-steam.txt".into() }));
            assert!(doc.migration_warning.is_some()); assert_eq!(doc.settings.games.lol.remote_path, "invalid-lol");
            patch(&mut doc, Update::Destination(Game::Lol, Destination { enabled: false, remote_path: "/fixed-lol.txt".into() }));
            if !connection_first {
                assert!(doc.migration_warning.is_some());
                patch(&mut doc, Update::Connection(connection(&fixture("/unused"))));
            }
            assert!(doc.migration_warning.is_none()); assert_eq!(doc.settings.games.steam.remote_path, "/fixed-steam.txt");
            assert_eq!(doc.settings.games.lol.remote_path, "/fixed-lol.txt"); assert!(doc.verified_targets.is_empty());
        }
    }
    #[test]
    fn shared_migration_preserves_matching_targets_and_independent_paths() {
        let steam = fixture("/GameReady/steam.txt"); let lol = fixture("/GameReady/lol.txt");
        let doc = migrated([(steam.clone(), true), (lol.clone(), true)]);
        assert!(doc.migration_warning.is_none()); assert!(doc.settings.games.steam.enabled); assert!(doc.settings.games.lol.enabled);
        assert_eq!(doc.verified_targets["steam"], target_digest(&steam)); assert_eq!(doc.verified_targets["lol"], target_digest(&lol));
    }
    #[test]
    fn conflicting_passwords_or_pins_pause_both_without_selecting_a_server() {
        for different_pin in [false, true] {
            let steam = fixture("/steam.txt"); let mut lol = fixture("/lol.txt");
            if different_pin { lol.fingerprint = "SHA256:different".into(); } else { lol.password = "other-fixture-secret".into(); }
            let doc = migrated([(steam, true), (lol, true)]);
            assert!(doc.migration_warning.is_some()); assert!(doc.settings.connection.host.is_empty());
            assert!(!doc.settings.games.steam.enabled && !doc.settings.games.lol.enabled); assert!(doc.verified_targets.is_empty());
            assert_eq!(doc.settings.games.steam.remote_path, "/steam.txt"); assert_eq!(doc.settings.games.lol.remote_path, "/lol.txt");
        }
    }
    #[test]
    fn path_aliases_are_rejected_and_parent_traversal_is_not_normalized_away() {
        let mut settings = Settings::default(); settings.games.steam.remote_path = "/GameReady//./same.txt".into(); settings.games.lol.remote_path = "/GameReady/same.txt".into();
        assert!(normalize(&mut settings).is_err()); assert!(normalized_path("/a/../b.txt").is_err()); assert!(normalized_path("/").is_err());
        assert_eq!(normalized_path(" /a//./b.txt ").unwrap(), "/a/b.txt");
    }
    #[test]
    fn each_target_identity_changes_only_for_connection_or_its_own_path() {
        let mut settings = Settings { connection: connection(&fixture("/unused")), ..Default::default() };
        let steam = target_digest(&config(&settings, Game::Steam)); let lol = target_digest(&config(&settings, Game::Lol));
        settings.games.lol.remote_path = "/new-lol.txt".into();
        assert_eq!(steam, target_digest(&config(&settings, Game::Steam))); assert_ne!(lol, target_digest(&config(&settings, Game::Lol)));
        settings.connection.password = "new-fixture".into(); assert_ne!(steam, target_digest(&config(&settings, Game::Steam)));
    }
    #[test]
    fn initial_host_key_pin_does_not_change_target_identity() {
        let mut config = fixture("/test.txt"); config.fingerprint.clear(); let before = target_digest(&config);
        config.fingerprint = "SHA256:fixture".into(); assert_eq!(before, target_digest(&config));
    }
    #[test]
    fn runtime_rejects_aliases_in_a_directly_replaced_central_file() {
        let mut settings = Settings { connection: connection(&fixture("/unused")), ..Default::default() };
        settings.games.steam.remote_path = "/GameReady//./same.txt".into(); settings.games.lol.remote_path = "/GameReady/same.txt".into();
        let doc = Document::new(settings);
        assert!(resolve_document(&doc, Game::Steam).is_err()); assert!(resolve_document(&doc, Game::Lol).is_err());
    }
    #[test]
    fn corrupt_shared_or_legacy_documents_are_preserved_without_creating_defaults() {
        let folder = std::env::temp_dir().join(format!("gameready-shared-config-{}", uuid::Uuid::new_v4())); fs::create_dir(&folder).unwrap();
        let shared = folder.join("backup-settings.json");
        fs::write(&shared, b"broken").unwrap(); assert!(load_at(&shared).is_err()); assert_eq!(fs::read(&shared).unwrap(), b"broken");
        fs::remove_file(&shared).unwrap();
        fs::write(folder.join("lol-accounts.json"), br#"{"version":1,"accounts":"broken"}"#).unwrap();
        assert!(load_at(&shared).is_err()); assert!(!shared.exists());
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn migration_only_writes_central_file_and_conflict_resolution_remains_suspended() {
        let folder = std::env::temp_dir().join(format!("gameready-shared-migration-{}", uuid::Uuid::new_v4())); fs::create_dir(&folder).unwrap();
        let steam = fixture("/steam.txt"); let mut lol = fixture("/lol.txt"); lol.password = "other-fixture".into();
        let steam_bytes = serde_json::to_vec(&serde_json::json!({"version":2,"accounts":{},"backup":{"config":steam}})).unwrap();
        let lol_bytes = serde_json::to_vec(&serde_json::json!({"version":1,"accounts":{},"config":lol})).unwrap();
        fs::write(folder.join("accounts.json"), &steam_bytes).unwrap(); fs::write(folder.join("lol-accounts.json"), &lol_bytes).unwrap();
        let doc = load_at(&folder.join("backup-settings.json")).unwrap();
        for game in [Game::Steam, Game::Lol] {
            let resolved = resolve_document(&doc, game).unwrap(); assert!(resolved.suspended); assert!(!resolved.config.enabled); assert!(resolved.config.host.is_empty());
        }
        assert_eq!(fs::read(folder.join("accounts.json")).unwrap(), steam_bytes); assert_eq!(fs::read(folder.join("lol-accounts.json")).unwrap(), lol_bytes);
        fs::remove_dir_all(folder).unwrap();
    }
}
