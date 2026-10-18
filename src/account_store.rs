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

pub fn merge_login_times(path: &Path, values: &[(String, String, i64)]) -> Result<BTreeMap<String, i64>, String> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut store = load_locked(path)?;
    let before = serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())?;
    let mut times = login_times(&store);
    for (sid, name, observed) in values {
        if !valid_sid(sid) { continue; }
        let timestamp = times.get(sid).copied().unwrap_or(0).max(if valid_time(*observed) { *observed } else { 0 });
        if name.is_empty() {
            if timestamp > 0 { store.unassigned_login_times.insert(sid.clone(), timestamp); }
        } else {
            let key = matching_key(&store.accounts, name);
            let account = store.accounts.entry(key).or_default();
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
    if serde_json::to_vec(&store).map_err(|_| "账号数据序列化失败".to_string())? != before { crate::atomic_json(path, &store)?; }
    Ok(login_times(&store))
}

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
