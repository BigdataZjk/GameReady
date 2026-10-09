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
        if flags.contains(OpenFlags::TRUNCATE) { files.files.insert(path.clone(), Vec::new()); }
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
        self.files.lock().unwrap().files.remove(&path).ok_or(StatusCode::NoSuchFile)?;
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
