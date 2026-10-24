//! 便携账号数据的唯一活动存储；只读写 v2 格式，所有读改写共用一把锁。
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

static STORE_LOCK: Mutex<()> = Mutex::new(());
const VERSION: u32 = 2;

fn empty_object() -> Value { Value::Object(Map::new()) }

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Account {
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steam_id64: Option<String>,
    last_login: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed_credential: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    synced_credential: Option<String>,
    pending: Vec<Value>,
    // 保留回执映射的键，不改变哈希查找所用的规范化账号名。
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed_credential_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    synced_credential_key: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct PendingPosition {
    account: String,
    index: usize,
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u32,
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
    #[serde(default = "empty_object")]
    backup: Value,
    #[serde(default)]
    unassigned_login_times: BTreeMap<String, i64>,
    #[serde(default)]
    pending_order: Vec<PendingPosition>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Default for Store {
    fn default() -> Self {
        Self { version: VERSION, accounts: BTreeMap::new(), backup: empty_object(),
            unassigned_login_times: BTreeMap::new(), pending_order: Vec::new(), extra: BTreeMap::new() }
    }
}

fn invalid(path: &Path) -> String {
    format!("{} 格式损坏或不受支持，原文件已保留，请修复或还原后重试", path.file_name().unwrap_or_default().to_string_lossy())
}

fn matching_key(accounts: &BTreeMap<String, Account>, name: &str) -> String {
    if accounts.contains_key(name) { return name.into(); }
    let mut matches = accounts.keys().filter(|key| key.eq_ignore_ascii_case(name));
    match (matches.next(), matches.next()) {
        (Some(key), None) => key.clone(),
        _ => name.to_ascii_lowercase(),
    }
}

fn string_map(value: Option<Value>) -> Result<BTreeMap<String, String>, ()> {
    match value {
        Some(value) => serde_json::from_value(value).map_err(|_| ()),
        None => Ok(BTreeMap::new()),
    }
}

fn valid_backup_global(global: &Map<String, Value>) -> bool {
    for key in ["verified", "credentials_seeded", "blocked"] {
        if global.get(key).is_some_and(|value| !value.is_boolean()) { return false; }
    }
    for key in ["revision", "state_version"] {
        if global.get(key).is_some_and(|value| value.as_u64().is_none()) { return false; }
    }
    if global.get("failures").is_some_and(|value| value.as_u64().is_none_or(|number| number > u32::MAX as u64)) { return false; }
    if global.get("next_retry_ms").is_some_and(|value| value.as_i64().is_none()) { return false; }
    if global.get("last_success_ms").is_some_and(|value| !value.is_null() && value.as_i64().is_none()) { return false; }
    for key in ["client_id", "status", "message"] {
        if global.get(key).is_some_and(|value| !value.is_string()) { return false; }
    }
    if let Some(config) = global.get("config") {
        let Some(config) = config.as_object() else { return false; };
        if config.get("enabled").is_some_and(|value| !value.is_boolean()) { return false; }
        if config.get("port").is_some_and(|value| value.as_u64().is_none_or(|number| number > u16::MAX as u64)) { return false; }
        for key in ["host", "username", "password", "remote_path", "fingerprint"] {
            if config.get(key).is_some_and(|value| !value.is_string()) { return false; }
        }
    }
    true
}

fn preserve_missing_fields(current: &mut Map<String, Value>, previous: &Map<String, Value>) {
    for (key, value) in previous {
        match current.get_mut(key) {
            Some(Value::Object(current)) => {
                if let Value::Object(previous) = value { preserve_missing_fields(current, previous); }
            }
            Some(_) => (),
            None => { current.insert(key.clone(), value.clone()); },
        }
    }
}

/// 将备份模块的完整快照拆回账号字段；调用方已取得中心锁并读入最新文档。
fn apply_backup(store: &mut Store, value: &Value) -> Result<(), ()> {
    let mut global = value.as_object().cloned().ok_or(())?;
    let confirmed = string_map(global.remove("confirmed_credentials"))?;
    let synced = string_map(global.remove("synced_credentials"))?;
    let queue: Vec<Value> = match global.remove("queue") {
        Some(Value::Array(queue)) => queue,
        Some(_) => return Err(()),
        None => Vec::new(),
    };
    if !valid_backup_global(&global) { return Err(()); }
    for pending in &queue {
        let pending = pending.as_object().ok_or(())?;
        if ["id", "account", "password", "source"].iter().any(|key| !pending.get(*key).is_some_and(Value::is_string))
            || !pending.get("ts_ms").is_some_and(|value| value.as_i64().is_some())
            || pending.get("manual").is_some_and(|value| !value.is_boolean()) { return Err(()); }
    }
    for account in store.accounts.values_mut() {
        account.confirmed_credential = None;
        account.synced_credential = None;
        account.confirmed_credential_key = None;
        account.synced_credential_key = None;
        account.pending.clear();
    }
    store.pending_order.clear();
    // 回执可能同时保存 PLAYER / Player / player。先为所有原始键保留
    // 独立槽位，避免较早的大小写匹配占用稍后还需要写入的精确键。
    for name in confirmed.keys().chain(synced.keys()) { store.accounts.entry(name.clone()).or_default(); }
    for (name, digest) in confirmed {
        let mut key = matching_key(&store.accounts, &name);
        if store.accounts.get(&key).is_some_and(|account| account.confirmed_credential.is_some()) { key = name.clone(); }
        let account = store.accounts.entry(key).or_default();
        account.confirmed_credential = Some(digest);
        account.confirmed_credential_key = Some(name);
    }
    for (name, digest) in synced {
        let mut key = matching_key(&store.accounts, &name);
        if store.accounts.get(&key).is_some_and(|account| account.synced_credential.is_some()) { key = name.clone(); }
        let account = store.accounts.entry(key).or_default();
        account.synced_credential = Some(digest);
        account.synced_credential_key = Some(name);
    }
    for pending in queue {
        let name = pending["account"].as_str().ok_or(())?;
        let key = matching_key(&store.accounts, name);
        let account = store.accounts.entry(key.clone()).or_default();
        store.pending_order.push(PendingPosition { account: key, index: account.pending.len() });
        account.pending.push(pending);
    }
    // 备份模块不认识将来的扩展字段，写回时仍保留文档中的这些属性。
    if let Some(previous) = store.backup.as_object() { preserve_missing_fields(&mut global, previous); }
    store.backup = Value::Object(global);
    Ok(())
}

fn backup_value(store: &Store) -> Result<Value, String> {
    let mut global = store.backup.as_object().cloned().ok_or_else(|| "accounts.json 中的备份配置格式损坏，原文件已保留".to_string())?;
    let mut confirmed = BTreeMap::new();
    let mut synced = BTreeMap::new();
    for (name, account) in &store.accounts {
        if let Some(digest) = &account.confirmed_credential {
            confirmed.insert(account.confirmed_credential_key.clone().unwrap_or_else(|| name.trim().to_ascii_lowercase()), digest.clone());
        }
        if let Some(digest) = &account.synced_credential {
            synced.insert(account.synced_credential_key.clone().unwrap_or_else(|| name.trim().to_ascii_lowercase()), digest.clone());
        }
    }
    let mut queue = Vec::new();
    let mut included = BTreeSet::new();
    for position in &store.pending_order {
        if let Some(pending) = store.accounts.get(&position.account).and_then(|account| account.pending.get(position.index)) {
            if included.insert((position.account.clone(), position.index)) { queue.push(pending.clone()); }
        }
    }
    for (name, account) in &store.accounts {
        for (index, pending) in account.pending.iter().enumerate() {
            if included.insert((name.clone(), index)) { queue.push(pending.clone()); }
        }
    }
    global.insert("confirmed_credentials".into(), serde_json::to_value(confirmed).map_err(|_| "备份回执序列化失败".to_string())?);
    global.insert("synced_credentials".into(), serde_json::to_value(synced).map_err(|_| "备份回执序列化失败".to_string())?);
    global.insert("queue".into(), Value::Array(queue));
    Ok(Value::Object(global))
}

fn load_locked(path: &Path) -> Result<Store, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(format!("读取 {} 失败：{error}", path.file_name().unwrap_or_default().to_string_lossy())),
    };
    let store: Store = serde_json::from_slice(&bytes).map_err(|_| invalid(path))?;
    if store.version != VERSION || !store.backup.as_object().is_some_and(valid_backup_global) { return Err(invalid(path)); }
    let snapshot = backup_value(&store)?;
    apply_backup(&mut Store::default(), &snapshot).map_err(|_| invalid(path))?;
    Ok(store)
}

pub fn read_credentials(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    Ok(load_locked(path)?.accounts.into_iter().filter_map(|(name, account)| account.password.map(|password| (name, password))).collect())
}

/// 每次从 exe 旁的 Data 读取完整账号目录，不依赖本机 Steam 历史或密码是否已保存。
/// 对外只返回显示资料；密码、备份凭据和队列不会进入前端账号列表。
pub fn read_accounts(path: &Path) -> Result<Vec<crate::SteamAccount>, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let store = load_locked(path)?;
    let times = login_times(&store);
    let mut accounts: BTreeMap<String, crate::SteamAccount> = BTreeMap::new();
    for (name, account) in store.accounts {
        if name.trim().is_empty() { continue; }
        let sid = account.steam_id64.filter(|sid| valid_sid(sid)).unwrap_or_default();
        let timestamp = (if valid_time(account.last_login) { account.last_login } else { 0 })
            .max(times.get(&sid).copied().unwrap_or(0));
        let persona = account.extra.get("persona_name").and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let key = name.to_ascii_lowercase();
        if let Some(existing) = accounts.get_mut(&key) {
            // 大小写变体可能由密码/回执分别写入；显示成一行，并保留最完整的资料。
            if !sid.is_empty() && (existing.steam_id64.is_empty() || timestamp > existing.timestamp) {
                existing.steam_id64 = sid;
            }
            if let Some(persona) = persona {
                if existing.persona_name == existing.account_name || timestamp >= existing.timestamp {
                    existing.persona_name = persona.into();
                }
            }
            existing.timestamp = existing.timestamp.max(timestamp);
        } else {
            accounts.insert(key, crate::SteamAccount { steam_id64: sid, account_name: name.clone(),
                persona_name: persona.unwrap_or(&name).into(), auto_login: false, timestamp });
        }
    }
    Ok(accounts.into_values().collect())
}

/// 删除便携目录中的完整账号，包含所有大小写变体和备份记录，避免刷新时重新出现。
pub fn remove_account(path: &Path, name: &str) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    let before = serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())?;
    let removed_ids: BTreeSet<_> = store.accounts.iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .filter_map(|(_, account)| account.steam_id64.clone()).collect();
    store.accounts.retain(|key, _| !key.eq_ignore_ascii_case(name));
    store.unassigned_login_times.retain(|sid, _| !removed_ids.contains(sid));
    // 通常队列与所属账号同名；对历史中错位的队列也按实际账号清理，并重映射索引。
    let mut positions = BTreeMap::new();
    for (key, account) in &mut store.accounts {
        let mut pending = Vec::new();
        for (old_index, entry) in std::mem::take(&mut account.pending).into_iter().enumerate() {
            if entry.get("account").and_then(Value::as_str).is_some_and(|account| account.eq_ignore_ascii_case(name)) { continue; }
            positions.insert((key.clone(), old_index), pending.len());
            pending.push(entry);
        }
        account.pending = pending;
    }
    store.pending_order = store.pending_order.into_iter().filter_map(|mut position| {
        position.index = *positions.get(&(position.account.clone(), position.index))?;
        Some(position)
    }).collect();
    if serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())? != before {
        // 与删除一同提交失效标记，任何正在上传的旧快照都不能再写回账号回执。
        let has_pending = store.accounts.values().any(|account| !account.pending.is_empty());
        let global = store.backup.as_object_mut().ok_or_else(|| invalid(path))?;
        for key in ["revision", "state_version"] {
            let next = global.get(key).and_then(Value::as_u64).unwrap_or(0).saturating_add(1);
            global.insert(key.into(), Value::from(next));
        }
        let enabled = global.get("config").and_then(|config| config.get("enabled")).and_then(Value::as_bool).unwrap_or(false);
        global.insert("status".into(), Value::String(if !enabled { "disabled" } else if has_pending { "pending" } else { "idle" }.into()));
        global.insert("message".into(), Value::String("本地账号已删除".into()));
        crate::atomic_json(path, &store)?;
    }
    Ok(())
}

pub fn set_credential(path: &Path, name: &str, password: &str) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    // 只替换密码；大小写重复记录里的时间、回执、在途队列均继续保留。
    for (key, account) in &mut store.accounts {
        if key.eq_ignore_ascii_case(name) { account.password = None; }
    }
    store.accounts.entry(name.to_ascii_lowercase()).or_default().password = Some(password.into());
    crate::atomic_json(path, &store)
}

pub fn remove_credential(path: &Path, name: &str) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    for (key, account) in &mut store.accounts {
        if key.eq_ignore_ascii_case(name) { account.password = None; }
    }
    crate::atomic_json(path, &store)
}

pub fn read_backup(path: &Path) -> Result<Value, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    backup_value(&load_locked(path)?)
}

pub fn write_backup(path: &Path, value: &Value) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    apply_backup(&mut store, value).map_err(|_| "备份数据格式损坏，原账号文件已保留".to_string())?;
    crate::atomic_json(path, &store)
}

fn valid_sid(sid64: &str) -> bool {
    sid64.parse::<u64>().ok().and_then(|sid| sid.checked_sub(76_561_197_960_265_728))
        .is_some_and(|sid| sid > 0 && sid <= u32::MAX as u64)
}

fn valid_time(timestamp: i64) -> bool { timestamp > 0 && timestamp <= chrono::Utc::now().timestamp().saturating_add(300) }

fn login_times(store: &Store) -> BTreeMap<String, i64> {
    let mut times = store.unassigned_login_times.clone();
    for account in store.accounts.values() {
        if let Some(sid) = &account.steam_id64 {
            if valid_time(account.last_login) { times.entry(sid.clone()).and_modify(|time| *time = (*time).max(account.last_login)).or_insert(account.last_login); }
        }
    }
    times.retain(|sid, time| valid_sid(sid) && valid_time(*time));
    times
}

#[cfg(test)]
pub fn read_login_times(path: &Path) -> Result<BTreeMap<String, i64>, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    Ok(login_times(&load_locked(path)?))
}

#[cfg(test)]
pub fn merge_login_times(path: &Path, values: &[(String, String, i64)]) -> Result<BTreeMap<String, i64>, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    let before = serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())?;
    merge_login_values(&mut store, values);
    if serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())? != before { crate::atomic_json(path, &store)?; }
    Ok(login_times(&store))
}

fn merge_login_values(store: &mut Store, values: &[(String, String, i64)]) {
    let mut times = login_times(store);
    for (sid, name, observed) in values {
        if !valid_sid(sid) { continue; }
        let mut timestamp = times.get(sid).copied().unwrap_or(0).max(if valid_time(*observed) { *observed } else { 0 });
        if name.is_empty() {
            if timestamp > 0 { store.unassigned_login_times.insert(sid.clone(), timestamp); }
        } else {
            let key = matching_key(&store.accounts, name);
            let account = store.accounts.entry(key).or_default();
            if account.steam_id64.as_deref().is_none_or(|previous| !valid_sid(previous) || previous == sid.as_str())
                && valid_time(account.last_login) {
                timestamp = timestamp.max(account.last_login);
            }
            if let Some(previous) = &account.steam_id64 {
                if previous != sid && valid_time(account.last_login) {
                    store.unassigned_login_times.entry(previous.clone()).and_modify(|time| *time = (*time).max(account.last_login)).or_insert(account.last_login);
                }
            }
            account.steam_id64 = Some(sid.clone());
            account.last_login = timestamp;
            store.unassigned_login_times.remove(sid);
        }
        if timestamp > 0 { times.insert(sid.clone(), timestamp); }
    }
}

/// 将本机可观察到的资料补存到便携目录；空昵称不会清除从其他机器带来的昵称。
pub fn merge_account_profiles(path: &Path, accounts: &[crate::SteamAccount]) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    let before = serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())?;
    let values: Vec<_> = accounts.iter().map(|account|
        (account.steam_id64.clone(), account.account_name.clone(), account.timestamp)).collect();
    merge_login_values(&mut store, &values);
    for profile in accounts {
        if profile.account_name.trim().is_empty() { continue; }
        let key = matching_key(&store.accounts, &profile.account_name);
        let account = store.accounts.entry(key).or_default();
        if !profile.persona_name.trim().is_empty() {
            account.extra.insert("persona_name".into(), Value::String(profile.persona_name.clone()));
        }
    }
    if serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())? != before { crate::atomic_json(path, &store)?; }
    Ok(())
}

#[cfg(test)]
pub fn remove_login_time(path: &Path, sid64: &str) -> Result<(), String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    store.unassigned_login_times.remove(sid64);
    for account in store.accounts.values_mut() {
        if account.steam_id64.as_deref() == Some(sid64) { account.steam_id64 = None; account.last_login = 0; }
    }
    crate::atomic_json(path, &store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!("gameready-account-store-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn path(&self) -> PathBuf { self.0.join("accounts.json") }
    }

    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn backup_fixture() -> Value {
        json!({
            "config": {"enabled":true,"host":"nas.example","port":2022,"username":"backup-user",
                "password":" NAS password ","remote_path":"/accounts.txt","fingerprint":"SHA256:fixture"},
            "verified":true,"credentials_seeded":true,"revision":9,"state_version":13,
            "client_id":"stable-client","status":"error","message":"retry preserved",
            "last_success_ms":1234,"failures":3,"blocked":false,"next_retry_ms":5678,
            "confirmed_credentials":{"player":"confirmed-digest","other":"other-digest"},
            "synced_credentials":{"other":"synced-digest"},
            "queue":[
                {"id":"one","account":"Player","password":" password one ","source":"manual","ts_ms":10,"manual":true,"future":"kept"},
                {"id":"two","account":"other","password":"password two","source":"login","ts_ms":11,"manual":false},
                {"id":"three","account":"Player","password":"new password","source":"login","ts_ms":12}
            ],
            "future_global":{"preserve":true}
        })
    }

    fn seed_credentials(path: &Path, credentials: &[(&str, &str)]) {
        let mut store = Store::default();
        for (name, password) in credentials {
            store.accounts.insert((*name).into(), Account { password: Some((*password).into()), ..Default::default() });
        }
        crate::atomic_json(path, &store).unwrap();
    }

    #[test]
    fn portable_inventory_includes_accounts_without_local_history_or_ids_and_never_exposes_passwords() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let document = json!({"version":2,"accounts":{
            "Player":{"password":"private fixture password","steam_id64":"76561197960265729","last_login":80,
                "persona_name":"Portable Player"},
            "player":{"last_login":100},
            "password_only":{"password":"another private fixture password"},
            "missing_password":{},
            "invalid_id":{"steam_id64":"invalid","last_login":-1},
            " ":{}
        },"unassigned_login_times":{"76561197960265729":120}});
        let bytes = serde_json::to_vec(&document).unwrap();
        fs::write(&path, &bytes).unwrap();
        let accounts = read_accounts(&path).unwrap();
        assert_eq!(accounts.len(), 4);
        let player = accounts.iter().find(|a| a.account_name.eq_ignore_ascii_case("player")).unwrap();
        assert_eq!(player.steam_id64, "76561197960265729");
        assert_eq!(player.timestamp, 120);
        assert_eq!(player.persona_name, "Portable Player");
        for name in ["password_only", "missing_password", "invalid_id"] {
            let account = accounts.iter().find(|a| a.account_name == name).unwrap();
            assert_eq!(account.steam_id64, "");
            assert_eq!(account.persona_name, name);
            assert!(!account.auto_login);
        }
        let public = serde_json::to_string(&accounts).unwrap();
        assert!(!public.contains("private fixture password"));
        assert!(serde_json::to_value(&accounts).unwrap().as_array().unwrap().iter().all(|a| a.get("password").is_none()));
        assert_eq!(fs::read(&path).unwrap(), bytes, "Reading the directory must not rewrite user data");
    }

    #[test]
    fn portable_inventory_reloads_replaced_data_on_every_read() {
        let fixture = Fixture::new();
        let path = fixture.path();
        assert!(read_accounts(&path).unwrap().is_empty());
        assert!(!path.exists());
        fs::write(&path, br#"{"version":2,"accounts":{"first":{"password":"one"}}}"#).unwrap();
        assert_eq!(read_accounts(&path).unwrap()[0].account_name, "first");
        fs::write(&path, br#"{"version":2,"accounts":{"replacement":{"password":"two"}}}"#).unwrap();
        assert_eq!(read_accounts(&path).unwrap()[0].account_name, "replacement");
    }

    #[test]
    fn observed_profiles_preserve_portable_names_credentials_and_unknown_fields() {
        let fixture = Fixture::new();
        let path = fixture.path();
        fs::write(&path, br#"{"version":2,"accounts":{"player":{"password":"fixture secret","future":"kept"},"portable_only":{}},"future_root":42}"#).unwrap();
        let mut profile = crate::SteamAccount { steam_id64:"76561197960265729".into(),
            account_name:"Player".into(),persona_name:"Portable Nickname".into(),auto_login:true,timestamp:100 };
        merge_account_profiles(&path, &[profile.clone()]).unwrap();
        profile.timestamp = 0;
        profile.persona_name.clear();
        merge_account_profiles(&path, &[profile]).unwrap();
        let accounts = read_accounts(&path).unwrap();
        assert_eq!(accounts.len(), 2);
        let player = accounts.iter().find(|a| a.account_name == "player").unwrap();
        assert_eq!(player.persona_name, "Portable Nickname");
        assert_eq!(player.timestamp, 100);
        assert_eq!(read_credentials(&path).unwrap()["player"], "fixture secret");
        let data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(data["accounts"]["player"]["future"], "kept");
        assert_eq!(data["future_root"], 42);
    }

    #[test]
    fn binding_a_new_steam_id_keeps_the_portable_accounts_known_login_time() {
        let fixture = Fixture::new();
        let path = fixture.path();
        for saved_id in [None, Some("invalid"), Some("76561197960265729")] {
            let mut account = json!({"last_login":100});
            if let Some(sid) = saved_id { account["steam_id64"] = json!(sid); }
            fs::write(&path, serde_json::to_vec(&json!({"version":2,"accounts":{"player":account}})).unwrap()).unwrap();
            let profile = crate::SteamAccount { steam_id64:"76561197960265729".into(),
                account_name:"PLAYER".into(),persona_name:String::new(),auto_login:false,timestamp:0 };
            merge_account_profiles(&path, &[profile]).unwrap();
            let account = &read_accounts(&path).unwrap()[0];
            assert_eq!(account.steam_id64, "76561197960265729");
            assert_eq!(account.timestamp, 100);
            assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 100);
        }
        // 确实换成另一个有效 SteamID 时，旧时间仍属于旧身份。
        merge_account_profiles(&path, &[crate::SteamAccount { steam_id64:"76561197960265730".into(),
            account_name:"player".into(),persona_name:String::new(),auto_login:false,timestamp:50 }]).unwrap();
        assert_eq!(read_accounts(&path).unwrap()[0].timestamp, 50);
        assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 100);
    }

    #[test]
    fn deleting_portable_account_removes_every_case_and_preserves_other_queue_positions() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let pending = |id: &str, name: &str| json!({"id":id,"account":name,"password":"fixture",
            "source":"test","ts_ms":1});
        let document = json!({"version":2,"accounts":{
            "Player":{"password":"fixture","steam_id64":"76561197960265729","last_login":100,
                "pending":[pending("removed","Player")],"confirmed_credential":"receipt"},
            "PLAYER":{"steam_id64":"76561197960265730","last_login":80,"synced_credential":"receipt"},
            "other":{"password":"keep","steam_id64":"76561197960265731","last_login":60,
                "pending":[pending("one","other"),pending("misfiled","pLaYeR"),pending("two","other")],"future":"kept"}
        },"unassigned_login_times":{"76561197960265729":100,"76561197960265730":80,"76561197960265731":60},
        "pending_order":[{"account":"other","index":2},{"account":"Player","index":0},
            {"account":"other","index":0},{"account":"other","index":1}],
        "backup":{"future":"kept","revision":9,"state_version":13,"config":{"enabled":true}},"future_root":42});
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        remove_account(&path, "player").unwrap();
        assert_eq!(read_accounts(&path).unwrap().len(), 1);
        assert_eq!(read_accounts(&path).unwrap()[0].account_name, "other");
        assert_eq!(read_credentials(&path).unwrap(), BTreeMap::from([("other".into(), "keep".into())]));
        assert_eq!(read_login_times(&path).unwrap(), BTreeMap::from([("76561197960265731".into(), 60)]));
        let backup = read_backup(&path).unwrap();
        assert_eq!(backup["queue"], json!([pending("two","other"),pending("one","other")]));
        assert_eq!(backup["confirmed_credentials"], json!({}));
        assert_eq!(backup["synced_credentials"], json!({}));
        assert_eq!(backup["revision"], 10, "Deleting must invalidate an upload snapshot in the same write");
        assert_eq!(backup["state_version"], 14);
        assert_eq!(backup["status"], "pending");
        let deleted_bytes = fs::read(&path).unwrap();
        remove_account(&path, "pLaYeR").unwrap();
        assert_eq!(fs::read(&path).unwrap(), deleted_bytes, "A repeated deletion must not change the surviving records");
        write_backup(&path, &backup).unwrap();
        assert_eq!(read_accounts(&path).unwrap().len(), 1, "Backup round trip must not resurrect deleted records");
        let data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(data["accounts"]["other"]["future"], "kept");
        assert_eq!(data["backup"]["future"], "kept");
        assert_eq!(data["future_root"], 42);
    }

    #[test]
    fn failed_account_delete_keeps_credentials_and_backup_revision_together() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let fixture = Fixture::new();
        let path = fixture.path();
        let original = br#"{"version":2,"accounts":{"player":{"password":"fixture"}},"backup":{"revision":9,"state_version":13}}"#;
        fs::write(&path, original).unwrap();
        let guard = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(&path).unwrap();
        assert!(remove_account(&path, "player").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        drop(guard);
        remove_account(&path, "player").unwrap();
        assert!(read_accounts(&path).unwrap().is_empty());
        assert_eq!(read_backup(&path).unwrap()["revision"], 10);
        assert_eq!(read_backup(&path).unwrap()["state_version"], 14);
    }

    #[test]
    fn portable_inventory_rejects_corruption_and_keeps_original_on_failed_delete_or_merge() {
        let fixture = Fixture::new();
        let path = fixture.path();
        for bytes in [b"{broken".as_slice(), br#"{"version":3,"accounts":{}}"#] {
            fs::write(&path, bytes).unwrap();
            assert!(read_accounts(&path).is_err());
            assert!(remove_account(&path, "player").is_err());
            assert!(merge_account_profiles(&path, &[]).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn v2_round_trip_preserves_passwords_nas_configuration_receipts_queue_order_and_login_times() {
        let fixture = Fixture::new();
        let path = fixture.path();
        seed_credentials(&path, &[("Player", " password one "), ("PLAYER", "different password"),
            ("other", "password two"), ("version", "account named version"), ("accounts", "account named accounts")]);
        let backup = backup_fixture();
        write_backup(&path, &backup).unwrap();
        merge_login_times(&path, &[("76561197960265729".into(), String::new(), 100),
            ("76561197960265730".into(), String::new(), 80)]).unwrap();
        let credentials = read_credentials(&path).unwrap();
        assert_eq!(credentials.len(), 5);
        assert_eq!(credentials["Player"], " password one ");
        assert_eq!(credentials["PLAYER"], "different password");
        assert_eq!(credentials["version"], "account named version");
        assert_eq!(credentials["accounts"], "account named accounts");
        assert_eq!(read_backup(&path).unwrap(), backup);
        assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 100);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        let times = merge_login_times(&path, &[("76561197960265729".into(), "Player".into(), 0)]).unwrap();
        assert_eq!(times["76561197960265729"], 100);
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["version"], VERSION);
        assert_eq!(value["accounts"]["Player"]["last_login"], 100);
        assert!(value["unassigned_login_times"].get("76561197960265729").is_none());
        assert_eq!(read_backup(&path).unwrap(), backup);

    }

    #[test]
    fn legacy_unsupported_and_corrupt_inputs_are_rejected_without_changes() {
        for credentials in [
            b"{broken".as_slice(),
            br#"{"player":"password"}"#,
            br#"{"version":"password","accounts":"another password"}"#,
            br#"{"version":3,"accounts":{}}"#,
            br#"{"version":2,"accounts":{"player":{"password":42}}}"#,
            br#"{"version":2,"accounts":{"player":{"pending":[{"account":"player"}]}}}"#,
        ] {
            let fixture = Fixture::new();
            let path = fixture.path();
            fs::write(&path, credentials).unwrap();
            let error = set_credential(&path, "player", "must not overwrite").unwrap_err();
            assert!(!error.contains("password"));
            assert!(!error.contains("must not overwrite"));
            assert_eq!(fs::read(&path).unwrap(), credentials);
            assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn stale_sibling_files_are_ignored_with_existing_or_missing_v2_store() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let backup_bytes = b"{broken stale backup}";
        let times_bytes = br#"{"76561197960265729":100}"#;
        fs::write(fixture.0.join("backup.json"), backup_bytes).unwrap();
        fs::write(fixture.0.join("steam-login-times.json"), times_bytes).unwrap();
        assert!(read_credentials(&path).unwrap().is_empty());
        assert!(read_login_times(&path).unwrap().is_empty());
        assert_eq!(read_backup(&path).unwrap()["queue"], json!([]));
        assert!(!path.exists());
        set_credential(&path, "player", "password").unwrap();
        assert_eq!(read_credentials(&path).unwrap()["player"], "password");
        assert!(read_login_times(&path).unwrap().is_empty());
        assert_eq!(fs::read(fixture.0.join("backup.json")).unwrap(), backup_bytes);
        assert_eq!(fs::read(fixture.0.join("steam-login-times.json")).unwrap(), times_bytes);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 3);
    }

    #[test]
    fn case_duplicate_password_replacement_preserves_metadata_and_receipts() {
        let fixture = Fixture::new();
        let path = fixture.path();
        seed_credentials(&path, &[("Player", "old"), ("PLAYER", "older")]);
        write_backup(&path, &backup_fixture()).unwrap();
        merge_login_times(&path, &[("76561197960265729".into(), "Player".into(), 100)]).unwrap();
        let backup = read_backup(&path).unwrap();
        set_credential(&path, "player", " latest password ").unwrap();
        assert_eq!(read_credentials(&path).unwrap(), BTreeMap::from([("player".into(), " latest password ".into())]));
        assert_eq!(read_backup(&path).unwrap(), backup);
        assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 100);
        remove_credential(&path, "PLAYER").unwrap();
        assert!(read_credentials(&path).unwrap().is_empty());
        assert_eq!(read_backup(&path).unwrap(), backup);
        assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 100);
        remove_login_time(&path, "76561197960265729").unwrap();
        assert!(read_login_times(&path).unwrap().is_empty());
        assert_eq!(read_backup(&path).unwrap(), backup);
    }

    #[test]
    fn case_duplicate_backup_receipts_round_trip_without_overwriting_any_variant() {
        let fixture = Fixture::new();
        let path = fixture.path();
        set_credential(&path, "player", "password").unwrap();
        let mut backup = backup_fixture();
        backup["confirmed_credentials"] = json!({"PLAYER":"upper-confirmed","Player":"mixed-confirmed","player":"lower-confirmed"});
        backup["synced_credentials"] = json!({"PLAYER":"upper-synced","Player":"mixed-synced","player":"lower-synced"});
        write_backup(&path, &backup).unwrap();
        assert_eq!(read_backup(&path).unwrap(), backup);
        set_credential(&path, "PLAYER", "replacement").unwrap();
        assert_eq!(read_backup(&path).unwrap(), backup);
        write_backup(&path, &backup).unwrap();
        assert_eq!(read_backup(&path).unwrap(), backup);
        assert_eq!(read_credentials(&path).unwrap(), BTreeMap::from([("player".into(), "replacement".into())]));
    }

    #[test]
    fn concurrent_password_backup_and_login_time_writes_preserve_every_attribute() {
        let fixture = Fixture::new();
        let path = fixture.path();
        set_credential(&path, "player", "initial").unwrap();
        let credential_path = path.clone();
        let backup_path = path.clone();
        let time_path = path.clone();
        let passwords = std::thread::spawn(move || {
            for index in 1..=24 { set_credential(&credential_path, "PLAYER", &format!("password-{index}")).unwrap(); }
        });
        let backup = std::thread::spawn(move || {
            for index in 1..=24 {
                let mut value = backup_fixture();
                value["revision"] = json!(index);
                write_backup(&backup_path, &value).unwrap();
            }
        });
        let times = std::thread::spawn(move || {
            for index in 1..=24 {
                merge_login_times(&time_path, &[("76561197960265729".into(), "player".into(), index)]).unwrap();
            }
        });
        passwords.join().unwrap(); backup.join().unwrap(); times.join().unwrap();
        assert_eq!(read_credentials(&path).unwrap()["player"], "password-24");
        assert_eq!(read_login_times(&path).unwrap()["76561197960265729"], 24);
        let mut expected = backup_fixture(); expected["revision"] = json!(24);
        assert_eq!(read_backup(&path).unwrap(), expected);
    }
}
