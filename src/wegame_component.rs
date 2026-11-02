//! The reviewed, unmodified MSI is embedded in the portable EXE. Installation
//! is only started by the explicit UI action, never by startup or a fill request.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Security::WinTrust::*;
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NOASYNC};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::wegame_hid::Keyboard;

type Result<T> = std::result::Result<T, String>;
const MSI: &[u8] = include_bytes!("../assets/third-party/fakerinput/FakerInput_Setup_0.1.1_x64.msi");
const LICENSE: &str = include_str!("../assets/third-party/fakerinput/LICENSE.txt");
const SHA256: &str = "4c0aefb7340051a91d606776243298b5cd1143ef5508bbae6800c474f9ed0840";
const DOWNLOAD_URL: &str = "https://github.com/Ryochan7/FakerInput/releases/download/v0.1.1/FakerInput_Setup_0.1.1_x64.msi";
const SOURCE_URL: &str = "https://github.com/Ryochan7/FakerInput";
static OPERATION: Mutex<()> = Mutex::new(());
static NEEDS_RESTART: AtomicBool = AtomicBool::new(false);
static INSTALL_UNCERTAIN: AtomicBool = AtomicBool::new(false);

fn confirmed_installer_state() -> Result<()> {
    if INSTALL_UNCERTAIN.load(Ordering::Acquire) {
        return Err("安装程序状态未能确认；请等待 Windows 安装结束后重启 GameReady".into());
    }
    Ok(())
}

pub fn lock_operation() -> Result<MutexGuard<'static, ()>> {
    let guard = OPERATION.try_lock().map_err(|_| "正在填入或安装 WeGame 组件，请稍候".to_string())?;
    confirmed_installer_state()?;
    if NEEDS_RESTART.load(Ordering::Acquire) {
        return Err("组件安装要求重启 Windows；重启后即可使用填入".into());
    }
    Ok(guard)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State { Missing, Ready, RestartRequired }

#[derive(Serialize)]
pub struct Snapshot { state: State, license: &'static str }

fn state() -> Result<State> {
    confirmed_installer_state()?;
    if NEEDS_RESTART.load(Ordering::Acquire) { return Ok(State::RestartRequired); }
    if crate::native_arch() != "x64" { return Err("内置 FakerInput 组件仅支持 Windows x64".into()); }
    // Opening and dropping an untouched keyboard sends no HID report.
    Ok(if Keyboard::open()?.is_some() { State::Ready } else { State::Missing })
}

#[tauri::command]
pub async fn wegame_component_status() -> Result<Snapshot> {
    tauri::async_runtime::spawn_blocking(|| Ok(Snapshot { state: state()?, license: LICENSE }))
        .await.map_err(|_| "组件检测异常结束，请重试".to_string())?
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum InstallExit { Success, RestartRequired }

fn installer_exit(code: u32) -> Result<InstallExit> {
    match code {
        0 => Ok(InstallExit::Success),
        3010 | 1641 => Ok(InstallExit::RestartRequired),
        1602 => Err("已取消安装，尚未启用 WeGame 填入组件".into()),
        1618 => Err("另一个 Windows 安装程序正在运行，请等待其结束后重试".into()),
        1638 => Err("已存在其他版本的 FakerInput，请先在 Windows 已安装的应用中检查该组件".into()),
        _ => Err(format!("组件安装失败（Windows Installer 错误 {code}），可重试或查看官方项目")),
    }
}

fn verify_hash(bytes: &[u8]) -> Result<()> {
    if format!("{:x}", Sha256::digest(bytes)) != SHA256 {
        return Err("内置组件校验失败，未启动安装；请重新下载 GameReady".into());
    }
    Ok(())
}

fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.is_empty() || wide.iter().any(|c| *c == 0 || *c == b'"' as u16) {
        return Err("临时安装路径无效，未启动安装".into());
    }
    wide.push(0);
    Ok(wide)
}

fn installer_args(path: &Path) -> Result<Vec<u16>> {
    let path = wide_path(path)?;
    // Direct ShellExecute arguments: no cmd.exe, PowerShell or path interpolation
    // into shell code. Quotes cannot occur in the validated Windows file path.
    Ok("/i \"".encode_utf16().chain(path[..path.len() - 1].iter().copied())
        .chain("\" /passive /norestart".encode_utf16()).chain(Some(0)).collect())
}

fn verify_signature(path: &Path, file: &File) -> Result<()> {
    let path = wide_path(path)?;
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path.as_ptr()), hFile: HANDLE(file.as_raw_handle()), ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32, dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE, dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file_info }, dwStateAction: WTD_STATEACTION_VERIFY,
        // The exact MSI is hash-pinned. Verify Authenticode using local trust so
        // the bundled installation also works offline; no system policy changes.
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL, ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let result = unsafe { WinVerifyTrust(HWND::default(), &mut action, (&mut data as *mut WINTRUST_DATA).cast()) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(HWND::default(), &mut action, (&mut data as *mut WINTRUST_DATA).cast()); }
    if result != 0 {
        return Err(format!("Windows 未能验证内置组件签名（0x{:08X}），未启动安装", result as u32));
    }
    Ok(())
}

struct Package { directory: PathBuf, path: PathBuf, locked_file: Option<File> }
impl Drop for Package {
    fn drop(&mut self) {
        drop(self.locked_file.take());
        // Delete only the exact file and the now-empty directory we created.
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}
impl Package {
    fn extract() -> Result<Self> {
        verify_hash(MSI)?;
        let directory = std::env::temp_dir().join(format!("GameReady-FakerInput-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).map_err(|_| "无法创建组件安装临时目录".to_string())?;
        let mut package = Self { path: directory.join("FakerInput_Setup_0.1.1_x64.msi"), directory, locked_file: None };
        {
            let mut file = OpenOptions::new().write(true).create_new(true).share_mode(0).open(&package.path)
                .map_err(|_| "无法释放内置组件安装包".to_string())?;
            file.write_all(MSI).and_then(|_| file.sync_all()).map_err(|_| "内置组件写入失败".to_string())?;
        }
        let mut file = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(&package.path)
            .map_err(|_| "无法锁定内置组件安装包".to_string())?;
        let mut bytes = Vec::new();
        // Bounded read; a replaced/extended package must fail the pinned hash.
        Read::by_ref(&mut file).take(MSI.len() as u64 + 1).read_to_end(&mut bytes)
            .map_err(|_| "无法校验内置组件安装包".to_string())?;
        verify_hash(&bytes)?;
        verify_signature(&package.path, &file)?;
        // Deny writes/deletion until msiexec finishes, including during UAC.
        package.locked_file = Some(file);
        Ok(package)
    }
}

struct Process(HANDLE);
impl Drop for Process { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } } }

fn run_installer() -> Result<InstallExit> {
    let package = Package::extract()?;
    let parameters = installer_args(&package.path)?;
    let mut system_dir = [0_u16; 32768];
    let len = unsafe { GetSystemDirectoryW(Some(&mut system_dir)) } as usize;
    if len == 0 || len >= system_dir.len() { return Err("无法定位 Windows Installer，未启动安装".into()); }
    let executable: Vec<u16> = system_dir[..len].iter().copied()
        .chain("\\msiexec.exe".encode_utf16()).chain(Some(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"), lpFile: PCWSTR(executable.as_ptr()), lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWNORMAL.0, ..Default::default()
    };
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            "已取消 Windows 授权，尚未安装组件".into()
        } else { format!("无法启动组件安装（{}），未启用 WeGame 填入", error.code()) });
    }
    if info.hProcess.is_invalid() {
        // We cannot prove the installer stopped: preserve its locked source.
        INSTALL_UNCERTAIN.store(true, Ordering::Release);
        std::mem::forget(package);
        return Err("无法跟踪组件安装状态，请等待 Windows 安装程序结束后重新检测".into());
    }
    let process = Process(info.hProcess);
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        INSTALL_UNCERTAIN.store(true, Ordering::Release);
        std::mem::forget(package);
        return Err("无法确认安装程序结束，请等待其结束后重新检测".into());
    }
    let mut code = 0;
    unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(|_| "无法读取组件安装结果，请重新检测".to_string())?;
    installer_exit(code)
}

fn install_if_missing(initial: State, run: impl FnOnce() -> Result<InstallExit>, confirm: impl FnOnce() -> Result<State>) -> Result<State> {
    if initial != State::Missing { return Ok(initial); }
    match run()? {
        InstallExit::RestartRequired => Ok(State::RestartRequired),
        InstallExit::Success => match confirm()? {
            State::Ready => Ok(State::Ready),
            _ => Err("安装已结束，但组件尚未就绪；请重启 Windows 后重新检测".into()),
        },
    }
}

#[tauri::command]
pub async fn wegame_component_install() -> Result<State> {
    tauri::async_runtime::spawn_blocking(|| {
        let _guard = lock_operation()?;
        let result = install_if_missing(state()?, run_installer, || {
            let mut last = Ok(State::Missing);
            for attempt in 0..20 {
                last = state();
                if last == Ok(State::Ready) { return last; }
                if attempt < 19 { std::thread::sleep(Duration::from_millis(250)); }
            }
            last
        })?;
        if result == State::RestartRequired { NEEDS_RESTART.store(true, Ordering::Release); }
        Ok(result)
    }).await.map_err(|_| "组件安装任务异常结束，请重新检测".to_string())?
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link { Download, Source }

#[tauri::command]
pub async fn wegame_component_open_link(link: Link) -> Result<()> {
    tauri::async_runtime::spawn_blocking(move || {
        // Only these two fixed HTTPS URLs may be opened by this command.
        let url: Vec<u16> = match link { Link::Download => DOWNLOAD_URL, Link::Source => SOURCE_URL }
            .encode_utf16().chain(Some(0)).collect();
        let mut info = SHELLEXECUTEINFOW { cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC, lpVerb: w!("open"), lpFile: PCWSTR(url.as_ptr()),
            nShow: SW_SHOWNORMAL.0, ..Default::default() };
        unsafe { ShellExecuteExW(&mut info) }.map_err(|_| "无法打开浏览器，请复制官方下载链接".to_string())
    }).await.map_err(|_| "无法打开组件链接".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_package_integrity_rejects_changes() {
        verify_hash(MSI).unwrap();
        let mut tampered = MSI.to_vec();
        tampered[256] ^= 1;
        assert!(verify_hash(&tampered).is_err());
        assert!(verify_hash(&MSI[..MSI.len() - 1]).is_err());
        assert!(LICENSE.contains("Copyright (c) 2021 Travis Nickles"));
    }

    #[test]
    fn extracted_package_is_signed_locked_and_cleaned() {
        let package = Package::extract().unwrap();
        let directory = package.directory.clone();
        assert!(OpenOptions::new().write(true).open(&package.path).is_err());
        assert!(fs::remove_file(&package.path).is_err());
        drop(package);
        assert!(!directory.exists());
    }

    #[test]
    fn msi_parameters_preserve_spaces_and_cannot_start_a_shell() {
        let args = installer_args(Path::new(r"C:\fixture 安装 & files\driver.msi")).unwrap();
        assert_eq!(String::from_utf16(&args[..args.len()-1]).unwrap(),
            "/i \"C:\\fixture 安装 & files\\driver.msi\" /passive /norestart");
        assert!(installer_args(Path::new("C:\\bad\"name.msi")).is_err());
        assert!(installer_args(Path::new("C:\\bad\0name.msi")).is_err());
    }

    #[test]
    fn cancellation_reboot_and_failures_are_not_reported_as_ready() {
        assert_eq!(installer_exit(0).unwrap(), InstallExit::Success);
        assert_eq!(installer_exit(3010).unwrap(), InstallExit::RestartRequired);
        for code in [1602, 1603, 1618, 1638, 1] { assert!(installer_exit(code).is_err()); }
        assert_eq!(install_if_missing(State::Missing, || installer_exit(3010), || panic!("must not probe")), Ok(State::RestartRequired));
        assert!(install_if_missing(State::Missing, || installer_exit(1602), || panic!("must not probe")).is_err());
        assert!(install_if_missing(State::Missing, || installer_exit(0), || Ok(State::Missing)).is_err());
        assert_eq!(install_if_missing(State::Missing, || installer_exit(0), || Ok(State::Ready)), Ok(State::Ready));
    }

    #[test]
    fn ready_or_restart_pending_never_reinstalls() {
        for state in [State::Ready, State::RestartRequired] {
            assert_eq!(install_if_missing(state, || panic!("must not install"), || panic!("must not probe")), Ok(state));
        }
    }
}
