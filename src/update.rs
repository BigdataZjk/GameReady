//! Release checks and explicit downloads. Downloaded programs are never launched automatically.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

const RELEASE_API: &str = "https://api.github.com/repos/BigdataZjk/GameReady/releases/latest";
const REPOSITORY_PATH: &str = "/BigdataZjk/GameReady/releases/";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_API_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EXE_BYTES: u64 = 512 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub available: bool,
    pub status: String,
    pub message: String,
    pub downloaded_path: Option<String>,
}

struct State {
    snapshot: Snapshot,
    candidate: Option<Candidate>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State {
    snapshot: Snapshot {
        current_version: CURRENT_VERSION.into(), latest_version: None, available: false,
        status: "idle".into(), message: String::new(), downloaded_path: None,
    },
    candidate: None,
}));
static BUSY: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Clone, Debug)]
struct Candidate {
    version: String,
    asset: Asset,
    sha256: Option<String>,
}

// Numeric comparison prevents v0.1.10 from sorting before v0.1.9. Build metadata
// has no precedence; stable releases sort after prereleases of the same version.
#[derive(Debug, Eq, PartialEq)]
struct Version {
    numbers: [u64; 3],
    prerelease: Vec<String>,
}

impl Version {
    fn parse(input: &str) -> Result<Self, String> {
        let input = input.strip_prefix('v').unwrap_or(input);
        let (version, metadata) = match input.split_once('+') {
            Some((version, metadata)) => (version, Some(metadata)),
            None => (input, None),
        };
        if metadata.is_some_and(|value| !valid_identifiers(value, false)) {
            return Err("版本号的构建标记无效".into());
        }
        let (numbers, prerelease) = match version.split_once('-') {
            Some((numbers, pre)) if valid_identifiers(pre, true) =>
                (numbers, pre.split('.').map(str::to_string).collect()),
            Some(_) => return Err("版本号的预发布标记无效".into()),
            None => (version, Vec::new()),
        };
        let components = numbers.split('.').collect::<Vec<_>>();
        if components.len() != 3 { return Err("版本号必须含主版本、次版本和补丁号".into()); }
        let mut parsed = [0; 3];
        for (index, component) in components.into_iter().enumerate() {
            if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit())
                || (component.len() > 1 && component.starts_with('0')) {
                return Err("版本号的数字格式无效".into());
            }
            parsed[index] = component.parse().map_err(|_| "版本号数字过大")?;
        }
        Ok(Self { numbers: parsed, prerelease })
    }
}

fn valid_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    value.split('.').all(|part| !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !(reject_numeric_leading_zero && part.len() > 1 && part.starts_with('0')
            && part.bytes().all(|byte| byte.is_ascii_digit())))
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| {
            match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => {},
            }
            for (left, right) in self.prerelease.iter().zip(&other.prerelease) {
                let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
                let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
                let order = match (left_numeric, right_numeric) {
                    (true, true) => left.len().cmp(&right.len()).then_with(|| left.cmp(right)),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left.cmp(right),
                };
                if order != Ordering::Equal { return order; }
            }
            self.prerelease.len().cmp(&other.prerelease.len())
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

fn is_https(url: &Url) -> bool {
    url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
}

fn official_release_url(url: &Url, category: &str) -> bool {
    is_https(url) && url.host_str() == Some("github.com") && url.query().is_none()
        && url.fragment().is_none()
        && url.path().starts_with(&format!("{REPOSITORY_PATH}{category}/"))
}

fn trusted_download_url(url: &Url) -> bool {
    if !is_https(url) { return false; }
    match url.host_str() {
        Some("github.com") => official_release_url(url, "download"),
        Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com"
            | "github-releases.githubusercontent.com") => true,
        _ => false,
    }
}

fn api_client() -> Result<Client, String> {
    Client::builder().user_agent(format!("GameReady/{CURRENT_VERSION}"))
        .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(30))
        .redirect(Policy::none()).build().map_err(|error| format!("创建更新连接失败：{error}"))
}

fn download_client() -> Result<Client, String> {
    Client::builder().user_agent(format!("GameReady/{CURRENT_VERSION}"))
        .connect_timeout(Duration::from_secs(15)).timeout(Duration::from_secs(10 * 60))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 { attempt.error("更新下载重定向次数过多") }
            else if trusted_download_url(attempt.url()) { attempt.follow() }
            else { attempt.error("更新下载被重定向到非 GitHub 地址") }
        })).build().map_err(|error| format!("创建下载连接失败：{error}"))
}

fn read_bounded(mut source: impl Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    source.by_ref().take(limit + 1).read_to_end(&mut bytes)
        .map_err(|error| format!("读取更新信息失败：{error}"))?;
    if bytes.len() as u64 > limit { return Err("更新信息超过允许大小".into()); }
    Ok(bytes)
}

fn fetch_release() -> Result<Release, String> {
    let response = api_client()?.get(RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").send()
        .map_err(|error| format!("连接 GitHub 检查更新失败：{error}"))?;
    if response.status().as_u16() == 403 || response.status().as_u16() == 429 {
        return Err("GitHub 更新检查暂时受限，稍后会自动重试".into());
    }
    let response = response.error_for_status().map_err(|error| format!("读取 GitHub 最新版本失败：{error}"))?;
    let bytes = read_bounded(response, MAX_API_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("GitHub 更新信息格式无效：{error}"))
}

fn digest_sha256(value: Option<&str>) -> Result<Option<String>, String> {
    match value {
        None | Some("") => Ok(None),
        Some(value) => {
            let digest = value.strip_prefix("sha256:").ok_or("更新文件的校验算法不受支持")?;
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("更新文件的 SHA256 校验信息无效".into());
            }
            Ok(Some(digest.to_ascii_lowercase()))
        },
    }
}

fn inspect_release(release: Release, current: &str) -> Result<(String, Option<Candidate>), String> {
    let remote = Version::parse(&release.tag_name)?;
    let local = Version::parse(current)?;
    let page = Url::parse(&release.html_url).map_err(|_| "最新版本的 GitHub 地址无效")?;
    if release.draft || release.prerelease || !remote.prerelease.is_empty()
        || !official_release_url(&page, "tag")
        || page.path_segments().and_then(|mut segments| segments.next_back()) != Some(release.tag_name.as_str()) {
        return Err("最新版本信息不是本项目的正式发布".into());
    }
    let version = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name).to_string();
    if remote <= local { return Ok((version, None)); }
    // The release pipeline publishes one specifically named Windows x64 EXE.
    // Requiring this name also excludes installers, ZIPs and path traversal.
    let expected_name = format!("GameReady-{version}-windows-x64.exe");
    let mut matches = release.assets.into_iter().filter(|asset| asset.name == expected_name);
    let asset = matches.next().ok_or("最新版本未提供可用的 Windows x64 EXE")?;
    if matches.next().is_some() { return Err("最新版本含重复的 EXE 文件，已停止下载".into()); }
    if asset.size < 64 || asset.size > MAX_EXE_BYTES { return Err("更新文件大小不在允许范围内".into()); }
    let url = Url::parse(&asset.browser_download_url).map_err(|_| "更新文件地址无效")?;
    let expected_path = format!("{REPOSITORY_PATH}download/{}/{}", release.tag_name, expected_name);
    if !official_release_url(&url, "download") || url.path() != expected_path {
        return Err("更新文件不属于本项目的 GitHub Release".into());
    }
    let sha256 = digest_sha256(asset.digest.as_deref())?;
    Ok((version.clone(), Some(Candidate { version, asset, sha256 })))
}

fn mutate(app: &AppHandle, change: impl FnOnce(&mut State)) -> Snapshot {
    let snapshot = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        change(&mut state);
        state.snapshot.clone()
    };
    let _ = app.emit("update-changed", &snapshot);
    snapshot
}

fn failure(app: &AppHandle, message: String) -> Snapshot {
    // A temporary network failure must not clear a previously discovered update.
    mutate(app, |state| {
        state.snapshot.status = "error".into();
        state.snapshot.message = message;
    })
}

struct Operation;
impl Operation {
    fn begin() -> Option<Self> {
        BUSY.compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire).ok().map(|_| Self)
    }
}
impl Drop for Operation {
    fn drop(&mut self) { BUSY.store(false, AtomicOrdering::Release); }
}

fn refresh(app: &AppHandle) -> Result<Snapshot, String> {
    mutate(app, |state| {
        state.snapshot.status = "checking".into(); state.snapshot.message = "正在检查更新…".into();
    });
    let (version, candidate) = inspect_release(fetch_release()?, CURRENT_VERSION)?;
    Ok(mutate(app, |state| {
        let changed_version = state.snapshot.latest_version.as_deref() != Some(&version);
        if changed_version { state.snapshot.downloaded_path = None; }
        state.snapshot.latest_version = Some(version);
        state.snapshot.available = candidate.is_some();
        state.candidate = candidate;
        if state.snapshot.available {
            state.snapshot.status = if state.snapshot.downloaded_path.is_some() { "downloaded" } else { "available" }.into();
            state.snapshot.message = if state.snapshot.downloaded_path.is_some() {
                "新版 EXE 已下载，点击可打开所在文件夹".into()
            } else { "发现新版本，点击下载更新".into() };
        } else {
            state.snapshot.status = "idle".into(); state.snapshot.message = "当前已是最新版本".into();
            state.snapshot.downloaded_path = None;
        }
    }))
}

fn verify_pe(file: &mut File, size: u64) -> Result<(), String> {
    let mut dos = [0u8; 64];
    file.seek(SeekFrom::Start(0)).map_err(|error| format!("读取更新文件失败：{error}"))?;
    file.read_exact(&mut dos).map_err(|_| "更新文件的 EXE 头不完整")?;
    if &dos[..2] != b"MZ" { return Err("更新文件不是有效的 Windows EXE".into()); }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if !(64..=1024 * 1024).contains(&offset) || offset.saturating_add(26) > size {
        return Err("更新文件的 EXE 结构无效".into());
    }
    let mut header = [0u8; 26];
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.read_exact(&mut header))
        .map_err(|_| "更新文件的 PE 头不完整")?;
    if &header[..4] != b"PE\0\0" || u16::from_le_bytes([header[4], header[5]]) != 0x8664
        || u16::from_le_bytes([header[24], header[25]]) != 0x20b {
        return Err("更新文件不是可用的 Windows x64 EXE".into());
    }
    Ok(())
}

fn copy_verified(mut source: impl Read, file: &mut File, candidate: &Candidate) -> Result<(), String> {
    let mut total = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|error| format!("下载更新失败：{error}"))?;
        if count == 0 { break; }
        total = total.checked_add(count as u64).ok_or("更新文件大小异常")?;
        if total > candidate.asset.size || total > MAX_EXE_BYTES { return Err("下载文件超出发布大小，已停止下载".into()); }
        file.write_all(&buffer[..count]).map_err(|error| format!("保存更新文件失败：{error}"))?;
        hash.update(&buffer[..count]);
    }
    if total != candidate.asset.size { return Err("更新下载不完整，文件大小与 GitHub 发布不一致".into()); }
    let actual_digest = format!("{:x}", hash.finalize());
    if candidate.sha256.as_ref().is_some_and(|expected| expected != &actual_digest) {
        return Err("更新文件 SHA256 校验失败，已丢弃下载".into());
    }
    file.flush().and_then(|_| file.sync_all()).map_err(|error| format!("写入更新文件失败：{error}"))?;
    verify_pe(file, total)
}

fn verify_existing(path: &Path, candidate: &Candidate) -> bool {
    let Ok(mut file) = File::open(path) else { return false; };
    if file.metadata().map(|metadata| metadata.len()).ok() != Some(candidate.asset.size) { return false; }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => hash.update(&buffer[..count]),
            Err(_) => return false,
        }
    }
    if candidate.sha256.as_ref().is_some_and(|expected| expected != &format!("{:x}", hash.finalize())) { return false; }
    verify_pe(&mut file, candidate.asset.size).is_ok()
}

fn fetch_exe(root: &Path, candidate: &Candidate) -> Result<PathBuf, String> {
    let response = download_client()?.get(&candidate.asset.browser_download_url).send()
        .map_err(|error| format!("连接 GitHub 下载更新失败：{error}"))?
        .error_for_status().map_err(|error| format!("GitHub 更新下载失败：{error}"))?;
    if !trusted_download_url(response.url()) { return Err("更新下载来源校验失败".into()); }
    if response.content_length().is_some_and(|length| length != candidate.asset.size) {
        return Err("下载响应大小与 GitHub 发布不一致".into());
    }
    save_download(root, candidate, response)
}

fn save_download(root: &Path, candidate: &Candidate, source: impl Read) -> Result<PathBuf, String> {
    let directory = root.join(format!("v{}-{}", candidate.version, uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).map_err(|error| format!("创建更新目录失败：{error}"))?;
    let destination = directory.join(&candidate.asset.name);
    let partial = destination.with_extension("exe.part");
    let result = (|| {
        let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&partial)
            .map_err(|error| format!("创建更新文件失败：{error}"))?;
        copy_verified(source, &mut file, candidate)?;
        drop(file);
        fs::rename(&partial, &destination).map_err(|error| format!("完成更新文件保存失败：{error}"))?;
        Ok(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_dir(&directory);
    }
    result
}

fn open_download_directory(path: &Path) -> Result<(), String> {
    let directory = path.parent().ok_or("更新文件目录无效")?;
    std::process::Command::new("explorer.exe").arg(directory).spawn()
        .map(|_| ()).map_err(|error| format!("EXE 已下载；打开所在文件夹失败：{error}"))
}

fn finish_download(app: &AppHandle, path: &Path) -> Snapshot {
    let open_result = open_download_directory(path);
    mutate(app, |state| {
        state.snapshot.status = "downloaded".into();
        state.snapshot.downloaded_path = Some(path.display().to_string());
        state.snapshot.message = match open_result {
            Ok(()) => "新版 EXE 已下载并校验，已打开所在文件夹；退出 GameReady 后可替换旧 EXE".into(),
            Err(message) => message,
        };
    })
}

fn check_once(app: &AppHandle) -> Snapshot {
    let Some(_operation) = Operation::begin() else { return update_status(); };
    match refresh(app) { Ok(snapshot) => snapshot, Err(error) => failure(app, error) }
}

#[tauri::command]
pub fn update_status() -> Snapshot {
    STATE.lock().unwrap_or_else(|error| error.into_inner()).snapshot.clone()
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Snapshot {
    let worker_app = app.clone();
    match tauri::async_runtime::spawn_blocking(move || check_once(&worker_app)).await {
        Ok(snapshot) => snapshot,
        Err(error) => failure(&app, format!("更新检查未完成：{error}")),
    }
}

#[tauri::command]
pub async fn download_update(app: AppHandle) -> Snapshot {
    let worker_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let Some(_operation) = Operation::begin() else { return update_status(); };
        let (cached_candidate, cached_path) = {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            (state.candidate.clone(), state.snapshot.downloaded_path.as_ref().map(PathBuf::from))
        };
        if let Some(path) = cached_path {
            // The UI offers to open the saved file's folder. This must remain
            // available offline, while still detecting a deleted or altered EXE.
            if cached_candidate.as_ref().is_some_and(|candidate| verify_existing(&path, candidate)) {
                return finish_download(&worker_app, &path);
            }
            mutate(&worker_app, |state| {
                state.snapshot.downloaded_path = None;
                state.snapshot.status = "checking".into();
                state.snapshot.message = "已下载文件不可用，正在重新检查更新…".into();
            });
        }
        if let Err(error) = refresh(&worker_app) { return failure(&worker_app, error); }
        let candidate = {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            state.candidate.clone()
        };
        let Some(candidate) = candidate else { return update_status(); };
        mutate(&worker_app, |state| {
            state.snapshot.status = "downloading".into(); state.snapshot.message = "正在下载新版 EXE…".into();
        });
        let path = match fetch_exe(&crate::data_dir().join("updates"), &candidate) {
            Ok(path) => path,
            Err(error) => return failure(&worker_app, error),
        };
        finish_download(&worker_app, &path)
    }).await;
    match result {
        Ok(snapshot) => snapshot,
        Err(error) => failure(&app, format!("更新下载未完成：{error}")),
    }
}

pub fn start(app: AppHandle) {
    if STARTED.swap(true, AtomicOrdering::AcqRel) { return; }
    std::thread::spawn(move || loop {
        check_once(&app);
        std::thread::sleep(CHECK_INTERVAL);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    #[ignore = "需要访问 GitHub；仅下载官方 Release 到临时目录，不执行程序"]
    fn live_official_release_download_smoke() {
        let release = fetch_release().unwrap();
        let (version, candidate) = inspect_release(release, "0.0.0").unwrap();
        let candidate = candidate.expect("official latest must contain a Windows x64 EXE");
        let root = std::env::temp_dir().join(format!("gameready-live-update-{}", uuid::Uuid::new_v4()));
        let result = fetch_exe(&root, &candidate);
        if let Ok(path) = &result {
            assert!(verify_existing(path, &candidate));
            println!("Verified official Release v{version}, {} bytes", candidate.asset.size);
            fs::remove_file(path).unwrap();
            fs::remove_dir(path.parent().unwrap()).unwrap();
        }
        if root.exists() { fs::remove_dir(&root).unwrap(); }
        result.unwrap();
    }

    fn release(version: &str) -> Release {
        let number = version.trim_start_matches('v');
        let name = format!("GameReady-{number}-windows-x64.exe");
        Release {
            tag_name: version.into(), html_url: format!("https://github.com/BigdataZjk/GameReady/releases/tag/{version}"),
            draft: false, prerelease: false,
            assets: vec![Asset { name: name.clone(), size: 256,
                browser_download_url: format!("https://github.com/BigdataZjk/GameReady/releases/download/{version}/{name}"), digest: None }],
        }
    }

    #[test]
    fn versions_follow_numeric_and_semver_precedence() {
        let parse = |value| Version::parse(value).unwrap();
        assert!(parse("v0.1.10") > parse("0.1.9"));
        assert!(parse("0.2.0") > parse("0.1.99"));
        let sorted = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"];
        for pair in sorted.windows(2) { assert!(parse(pair[0]) < parse(pair[1])); }
        assert_eq!(parse("1.0.0+build.123"), parse("1.0.0+other.456"));
        assert!(parse("1.0.0-99999999999999999999999999") > parse("1.0.0-9999999999999999999999999"));
        for invalid in ["1.2", "v1.2.3.4", "01.2.3", "1.2.3-01", "1.2.3-", "1.2.3+", "1.2.3+x+y", "1.2.3 "] {
            assert!(Version::parse(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn latest_release_selects_only_official_matching_x64_executable() {
        let (version, candidate) = inspect_release(release("v0.1.16"), "0.1.15").unwrap();
        assert_eq!(version, "0.1.16"); assert_eq!(candidate.unwrap().asset.name, "GameReady-0.1.16-windows-x64.exe");
        assert!(inspect_release(release("v0.1.15"), "0.1.15").unwrap().1.is_none());
        assert!(inspect_release(release("v0.1.14"), "0.1.15").unwrap().1.is_none());
        let mut duplicated = release("v0.1.16"); duplicated.assets.push(duplicated.assets[0].clone());
        assert!(inspect_release(duplicated, "0.1.15").is_err());
        for mutate in [
            |release: &mut Release| release.prerelease = true,
            |release: &mut Release| release.draft = true,
            |release: &mut Release| release.html_url = "https://github.com/attacker/GameReady/releases/tag/v0.1.16".into(),
            |release: &mut Release| release.html_url.push_str("?other=1"),
            |release: &mut Release| release.assets[0].name = "GameReady-0.1.16-windows-arm64.exe".into(),
            |release: &mut Release| release.assets[0].name = "../GameReady-0.1.16-windows-x64.exe".into(),
            |release: &mut Release| release.assets[0].size = MAX_EXE_BYTES + 1,
            |release: &mut Release| release.assets[0].browser_download_url = "https://github.com/attacker/GameReady/releases/download/v0.1.16/GameReady-0.1.16-windows-x64.exe".into(),
            |release: &mut Release| release.assets[0].browser_download_url = "https://github.com/BigdataZjk/GameReady/releases/download/v0.1.15/GameReady-0.1.16-windows-x64.exe".into(),
        ] {
            let mut bad = release("v0.1.16"); mutate(&mut bad);
            assert!(inspect_release(bad, "0.1.15").is_err());
        }
    }

    #[test]
    fn redirect_policy_rejects_lookalike_hosts_and_downgrades() {
        for value in [
            "https://github.com/BigdataZjk/GameReady/releases/download/v1.0.0/GameReady.exe",
            "https://release-assets.githubusercontent.com/github-production-release-asset/file?signature=test",
            "https://objects.githubusercontent.com/file",
        ] { assert!(trusted_download_url(&Url::parse(value).unwrap()), "rejected {value}"); }
        for value in [
            "http://github.com/BigdataZjk/GameReady/releases/download/v1.0.0/GameReady.exe",
            "https://github.com.evil.test/BigdataZjk/GameReady/releases/download/file.exe",
            "https://evilgithub.com/BigdataZjk/GameReady/releases/download/file.exe",
            "https://github.com/other/repo/releases/download/file.exe",
            "https://user:pass@release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com:444/file",
            "https://github.com/BigdataZjk/GameReady/releases/download/../../../other/file.exe",
        ] { assert!(!trusted_download_url(&Url::parse(value).unwrap()), "accepted {value}"); }
    }

    fn pe_bytes() -> Vec<u8> {
        let mut bytes = vec![0u8; 256]; bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes()); bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes()); bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes
    }

    fn copy_fixture(bytes: &[u8], candidate: &Candidate) -> Result<(), String> {
        let path = std::env::temp_dir().join(format!("gameready-update-test-{}.part", uuid::Uuid::new_v4()));
        let result = {
            let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
            copy_verified(Cursor::new(bytes), &mut file, candidate)
        };
        fs::remove_file(path).unwrap(); result
    }

    #[test]
    fn downloads_require_complete_size_valid_sha256_and_windows_x64_pe() {
        let bytes = pe_bytes();
        let mut candidate = inspect_release(release("v0.1.16"), "0.1.15").unwrap().1.unwrap();
        candidate.sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
        assert!(copy_fixture(&bytes, &candidate).is_ok());
        assert!(copy_fixture(&bytes[..200], &candidate).is_err());
        let mut extra = bytes.clone(); extra.push(0); assert!(copy_fixture(&extra, &candidate).is_err());
        candidate.sha256 = Some("0".repeat(64)); assert!(copy_fixture(&bytes, &candidate).is_err());
        candidate.sha256 = None;
        let mut invalid = bytes.clone(); invalid[0] = 0; assert!(copy_fixture(&invalid, &candidate).is_err());
        let mut x86 = bytes.clone(); x86[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        assert!(copy_fixture(&x86, &candidate).is_err());
        let mut bad_offset = bytes; bad_offset[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(copy_fixture(&bad_offset, &candidate).is_err());
    }

    #[test]
    fn failed_download_removes_partial_and_only_verified_exe_is_published() {
        let root = std::env::temp_dir().join(format!("gameready-update-store-test-{}", uuid::Uuid::new_v4()));
        let candidate = inspect_release(release("v0.1.16"), "0.1.15").unwrap().1.unwrap();
        assert!(save_download(&root, &candidate, Cursor::new(b"incomplete")).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        struct BrokenReader;
        impl Read for BrokenReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture interruption"))
            }
        }
        assert!(save_download(&root, &candidate, BrokenReader).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let path = save_download(&root, &candidate, Cursor::new(pe_bytes())).unwrap();
        assert_eq!(path.file_name().unwrap(), "GameReady-0.1.16-windows-x64.exe");
        assert!(verify_existing(&path, &candidate));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_file(&path).unwrap();
        fs::remove_dir(path.parent().unwrap()).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn api_bounds_digest_validation_and_operation_exclusion() {
        assert_eq!(read_bounded(Cursor::new(b"abc"), 3).unwrap(), b"abc");
        assert!(read_bounded(Cursor::new(b"abcd"), 3).is_err());
        assert_eq!(digest_sha256(None).unwrap(), None);
        assert_eq!(digest_sha256(Some(&format!("sha256:{}", "A".repeat(64)))).unwrap(), Some("a".repeat(64)));
        assert!(digest_sha256(Some("sha256:invalid")).is_err());
        assert!(digest_sha256(Some("sha512:invalid")).is_err());
        let operation = Operation::begin().unwrap(); assert!(Operation::begin().is_none());
        drop(operation); assert!(Operation::begin().is_some());
    }
}
