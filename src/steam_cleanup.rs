//! 删除其他账号只修改便携记录；Steam 停止后再清理历史和 userdata。
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::{account_store, field, login_history, sid64_to_sid3, write_vdf, SteamAccount, LOGIN_TIMES_OP};

pub trait Runtime {
    fn running(&self) -> bool;
    fn stop(&mut self) -> Result<(), String>;
    fn restart(&mut self, root: &str, account: &str) -> Result<(), String>;
    fn remove_record(&mut self, account: &str) -> Result<(), String>;
}

/// 只有删除当前会话（或当前登录目标）才允许停止、重新启动 Steam。
pub fn delete_selected(selected: &SteamAccount, root: Option<&str>, path: &Path,
    current_session: bool, runtime: &mut impl Runtime) -> Result<(), String> {
    let restart = current_session && root.is_some() && runtime.running();
    if restart { runtime.stop()?; }
    if let Err(error) = runtime.remove_record(&selected.account_name) {
        if restart {
            runtime.restart(root.unwrap(), &selected.account_name)
                .map_err(|recovery| format!("{error}；恢复 Steam 失败：{recovery}"))?;
        }
        return Err(error);
    }
    let mut errors = Vec::new();
    if let Some(root) = root {
        if let Err(error) = clean_when_stopped(root, path, || runtime.running()) { errors.push(error); }
        if restart {
            if let Err(error) = runtime.restart(root, "") { errors.push(format!("恢复 Steam 失败：{error}")); }
        }
    }
    if errors.is_empty() { Ok(()) }
    else { Err(format!("账号已删除，部分本机缓存尚未清理，将在 Steam 退出后重试：{}", errors.join("；"))) }
}

/// 调用方持有 STEAM_OP；启动和后台清理共用此入口。运行期间不改 Steam 文件。
/// 删除标记保留到显式重新添加，防止再次启动、其他安装目录或旧快照使账号复活。
pub fn clean_when_stopped(root: &str, path: &Path, mut running: impl FnMut() -> bool) -> Result<bool, String> {
    if running() { return Ok(false); }
    let _times = LOGIN_TIMES_OP.lock().unwrap_or_else(|poison| poison.into_inner());
    let deleted = account_store::read_deleted_accounts(path)?;
    if deleted.is_empty() { return Ok(true); }
    let mut accounts = login_history(root)?;
    let before = accounts.len();
    let mut ids: BTreeSet<String> = deleted.values().flatten().cloned().collect();
    accounts.retain(|account| {
        let remove = field(account, "AccountName").is_some_and(|name|
            deleted.keys().any(|key| key.eq_ignore_ascii_case(name)));
        if remove { ids.insert(account.sid64.clone()); }
        !remove
    });
    // 异常历史中同一 SID 若还属于其他账号，保留其数据，避免误删。
    ids.retain(|id| !accounts.iter().any(|account| &account.sid64 == id));
    if before != accounts.len() {
        if running() { return Ok(false); }
        write_vdf(root, &accounts)?;
    }
    for sid3 in ids.iter().filter_map(|sid| sid64_to_sid3(sid)) {
        let directory = Path::new(root).join("userdata").join(sid3);
        if !directory.try_exists().map_err(|error| format!("检查账号数据目录失败：{error}"))? { continue; }
        if running() { return Ok(false); }
        if let Err(error) = fs::remove_dir_all(&directory) {
            if error.kind() != std::io::ErrorKind::NotFound { return Err(format!("账号数据目录删除失败：{error}")); }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{portable_accounts_at, to_account, vdf_path, RawAccount};
    use std::path::PathBuf;

    struct Fixture {
        directory: PathBuf,
        running: bool,
        actions: Vec<String>,
        stop_fails: bool,
    }
    impl Fixture {
        fn new(running: bool) -> Self {
            let directory = std::env::temp_dir().join(format!("gameready-steam-delete-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(directory.join("config")).unwrap();
            let fixture = Self { directory, running, actions: Vec::new(), stop_fails: false };
            let records: Vec<_> = [("76561197960265729", "current"), ("76561197960265730", "other")].into_iter()
                .map(|(sid, name)| RawAccount { sid64: sid.into(), fields: vec![
                    ("AccountName".into(), name.into()), ("Timestamp".into(), "100".into()),
                    ("RememberPassword".into(), "1".into()), ("FutureField".into(), "preserve".into())] }).collect();
            write_vdf(&fixture.root(), &records).unwrap();
            for record in &records {
                account_store::set_credential(&fixture.path(), field(record, "AccountName").unwrap(), "fixture password").unwrap();
                let directory = fixture.directory.join("userdata").join(sid64_to_sid3(&record.sid64).unwrap());
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join("fixture.txt"), record.sid64.as_bytes()).unwrap();
            }
            account_store::merge_account_profiles(&fixture.path(), &records.iter().map(to_account).collect::<Vec<_>>()).unwrap();
            fixture
        }
        fn root(&self) -> String { self.directory.to_str().unwrap().into() }
        fn path(&self) -> PathBuf { self.directory.join("accounts.json") }
        fn account(&self, name: &str) -> SteamAccount {
            account_store::read_accounts(&self.path()).unwrap().into_iter().find(|a| a.account_name == name).unwrap()
        }
        fn delete(&mut self, name: &str, current: bool) -> Result<(), String> {
            delete_selected(&self.account(name), Some(&self.root()), &self.path(), current, self)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.directory); }
    }
    impl Runtime for Fixture {
        fn running(&self) -> bool { self.running }
        fn stop(&mut self) -> Result<(), String> {
            self.actions.push("stop".into());
            if self.stop_fails { return Err("fixture stop failure".into()); }
            self.running = false;
            Ok(())
        }
        fn restart(&mut self, _root: &str, account: &str) -> Result<(), String> {
            self.actions.push(format!("restart:{account}"));
            self.running = true;
            Ok(())
        }
        fn remove_record(&mut self, account: &str) -> Result<(), String> { account_store::remove_account(&self.path(), account) }
    }

    #[test]
    fn deleting_other_account_preserves_live_session_and_survives_steam_cache_writeback() {
        let mut fixture = Fixture::new(true);
        let history = fs::read(vdf_path(&fixture.root())).unwrap();
        let current = fs::read(fixture.directory.join("userdata/1/fixture.txt")).unwrap();
        fixture.delete("other", false).unwrap();
        assert!(fixture.running);
        assert!(fixture.actions.is_empty(), "Deleting another account must never stop or launch Steam");
        assert_eq!(fs::read(vdf_path(&fixture.root())).unwrap(), history);
        assert!(fixture.directory.join("userdata/2/fixture.txt").exists());
        assert_eq!(account_store::read_credentials(&fixture.path()).unwrap().len(), 1);
        // 模拟退出时回写旧内存历史、重新读取 Data（包括 GameReady 重启后的读取）。
        fs::write(vdf_path(&fixture.root()), &history).unwrap();
        for _ in 0..2 {
            let list = portable_accounts_at(Some(&fixture.root()), &fixture.path()).unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].account_name, "current");
        }
        fixture.running = false;
        assert!(clean_when_stopped(&fixture.root(), &fixture.path(), || fixture.running).unwrap());
        assert!(!fixture.directory.join("userdata/2").exists());
        assert_eq!(fs::read(fixture.directory.join("userdata/1/fixture.txt")).unwrap(), current);
        let remaining = login_history(&fixture.root()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(field(&remaining[0], "FutureField"), Some("preserve"));
        assert_eq!(field(&remaining[0], "RememberPassword"), Some("1"));
        assert!(fixture.actions.is_empty());
        // 其他 Steam 安装目录的旧历史也不会重新导入；明确重新添加才取消标记。
        fs::write(vdf_path(&fixture.root()), history).unwrap();
        assert_eq!(portable_accounts_at(Some(&fixture.root()), &fixture.path()).unwrap().len(), 1);
        account_store::set_credential(&fixture.path(), "OTHER", "replacement fixture password").unwrap();
        assert_eq!(portable_accounts_at(Some(&fixture.root()), &fixture.path()).unwrap().len(), 2);
        fs::create_dir_all(fixture.directory.join("userdata/2")).unwrap();
        clean_when_stopped(&fixture.root(), &fixture.path(), || false).unwrap();
        assert!(fixture.directory.join("userdata/2").exists(), "Re-added accounts must not be cleaned by an old deletion");
    }

    #[test]
    fn stopped_steam_is_cleaned_without_launching_and_current_deletion_still_stops() {
        let mut stopped = Fixture::new(false);
        stopped.delete("other", false).unwrap();
        assert!(stopped.actions.is_empty());
        assert!(!stopped.directory.join("userdata/2").exists());
        let mut active = Fixture::new(true);
        active.delete("current", true).unwrap();
        assert_eq!(active.actions, ["stop", "restart:"]);
        assert!(!active.directory.join("userdata/1").exists());
        assert_eq!(account_store::read_accounts(&active.path()).unwrap()[0].account_name, "other");
        let mut portable = Fixture::new(true);
        delete_selected(&portable.account("other"), None, &portable.path(), false, &mut portable).unwrap();
        assert!(portable.actions.is_empty());
        assert_eq!(account_store::read_accounts(&portable.path()).unwrap().len(), 1);
    }

    #[test]
    fn cleanup_rechecks_process_before_writing_and_retries_after_corrupt_history_is_repaired() {
        let mut fixture = Fixture::new(true);
        fixture.delete("other", false).unwrap();
        let history = fs::read(vdf_path(&fixture.root())).unwrap();
        let mut checks = 0;
        assert!(!clean_when_stopped(&fixture.root(), &fixture.path(), || { checks += 1; checks > 1 }).unwrap());
        assert_eq!(fs::read(vdf_path(&fixture.root())).unwrap(), history);
        assert!(fixture.directory.join("userdata/2").exists());
        fs::write(vdf_path(&fixture.root()), b"{broken").unwrap();
        assert!(clean_when_stopped(&fixture.root(), &fixture.path(), || false).is_err());
        assert_eq!(fs::read(vdf_path(&fixture.root())).unwrap(), b"{broken");
        assert!(fixture.directory.join("userdata/2").exists());
        fs::write(vdf_path(&fixture.root()), history).unwrap();
        assert!(clean_when_stopped(&fixture.root(), &fixture.path(), || false).unwrap());
        assert!(!fixture.directory.join("userdata/2").exists());
    }

    #[test]
    fn failed_store_write_or_stop_preserves_records_and_live_session() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let mut fixture = Fixture::new(true);
        let history = fs::read(vdf_path(&fixture.root())).unwrap();
        let before = fs::read(fixture.path()).unwrap();
        let guard = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(fixture.path()).unwrap();
        assert!(fixture.delete("other", false).is_err());
        assert!(fixture.actions.is_empty());
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
        assert!(fixture.delete("current", true).is_err());
        assert_eq!(fixture.actions, ["stop", "restart:current"]);
        assert!(fixture.running);
        drop(guard);
        fixture.actions.clear();
        fixture.stop_fails = true;
        assert!(fixture.delete("current", true).is_err());
        assert_eq!(fixture.actions, ["stop"]);
        assert_eq!(fs::read(fixture.path()).unwrap(), before);
        assert_eq!(fs::read(vdf_path(&fixture.root())).unwrap(), history);
    }

    #[test]
    fn unknown_active_account_and_unrelated_pending_login_do_not_authorize_a_restart() {
        let fixture = Fixture::new(true);
        let selected = fixture.account("other");
        let mut pending = crate::SteamProgress { generation: 1, phase: "waiting".into(), message: String::new(),
            sid64: Some("76561197960265729".into()), account_name: Some("current".into()) };
        assert!(!crate::deletion_targets_session(&selected, None, None));
        assert!(!crate::deletion_targets_session(&selected, Some("76561197960265729"), Some(&pending)));
        assert!(crate::deletion_targets_session(&selected, Some("76561197960265730"), None));
        pending.sid64 = None;
        pending.account_name = Some("OTHER".into());
        assert!(crate::deletion_targets_session(&selected, None, Some(&pending)));
        // 外部已切到另一个账号时，只取消旧确认任务，不能重启这个新会话。
        assert!(!crate::deletion_targets_session(&selected, Some("76561197960265729"), Some(&pending)));
        assert!(crate::deletion_targets_pending_login(&selected, Some(&pending)));
        pending.phase = "idle".into();
        assert!(!crate::deletion_targets_session(&selected, None, Some(&pending)));
    }
}
