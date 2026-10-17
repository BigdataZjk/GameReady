// GameReady 主入口：WebView2 引导 → Tauri 应用
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use tauri::{AppHandle, Emitter};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE};
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_NOCLOSEPROCESS};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
use windows::Win32::System::Threading::{GetCurrentProcess, WaitForSingleObject, INFINITE};

mod backup;
mod vdf {
//! Steam VDF parsing, account records and preference merging.
#[derive(Clone, Debug, PartialEq)]
enum Value { Text(String), Object(Vec<(String, Value)>) }

fn parse(text: &str) -> Result<Vec<(String, Value)>, String> {
    let bytes = text.trim_start_matches('\u{feff}').as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b.is_ascii_whitespace() => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            },
            b'{' | b'}' => { tokens.push((bytes[i] as char).to_string()); i += 1; },
            b'"' => {
                i += 1; let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' { i += 1; }
                    i += 1;
                }
                if i >= bytes.len() { return Err("VDF 字符串未闭合，原文件已保留".into()); }
                tokens.push(format!("\"{}", std::str::from_utf8(&bytes[start..i]).map_err(|_| "VDF 编码错误")?));
                i += 1;
            },
            _ => return Err("VDF 格式无法识别，原文件已保留".into()),
        }
    }
    fn object(tokens: &[String], i: &mut usize, nested: bool, depth: usize) -> Result<Vec<(String, Value)>, String> {
        if depth > 64 { return Err("VDF 嵌套层数过多".into()); }
        let mut pairs = Vec::new();
        while let Some(token) = tokens.get(*i) {
            *i += 1;
            if token == "}" {
                if nested { return Ok(pairs); }
                return Err("VDF 大括号不匹配".into());
            }
            let key = token.strip_prefix('"').ok_or("VDF 缺少键名")?.to_string();
            let token = tokens.get(*i).ok_or("VDF 缺少字段值")?;
            *i += 1;
            let value = if token == "{" { Value::Object(object(tokens, i, true, depth + 1)?) }
                else { Value::Text(token.strip_prefix('"').ok_or("VDF 字段值无效")?.into()) };
            pairs.push((key, value));
        }
        if nested { return Err("VDF 大括号未闭合".into()); }
        Ok(pairs)
    }
    object(&tokens, &mut 0, false, 0)
}

fn merge_into(target: &mut Vec<(String, Value)>, patch: &[(String, Value)]) {
    for (key, value) in patch {
        if let Some((_, current)) = target.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            match (current, value) {
                (Value::Object(current), Value::Object(patch)) => merge_into(current, patch),
                (current, value) => *current = value.clone(),
            }
        } else { target.push((key.clone(), value.clone())); }
    }
}

fn render(pairs: &[(String, Value)], depth: usize, output: &mut String) {
    let indent = "\t".repeat(depth);
    for (key, value) in pairs {
        match value {
            Value::Text(value) => output.push_str(&format!("{indent}\"{key}\"\t\t\"{value}\"\n")),
            Value::Object(children) => {
                output.push_str(&format!("{indent}\"{key}\"\n{indent}{{\n"));
                render(children, depth + 1, output);
                output.push_str(&format!("{indent}}}\n"));
            },
        }
    }
}

pub fn merge(existing: &[u8], template: &[u8]) -> Result<Vec<u8>, String> {
    let mut target = parse(std::str::from_utf8(existing).map_err(|_| "现有 Steam 配置不是 UTF-8，原文件已保留")?)?;
    let original = target.clone();
    let patch = parse(std::str::from_utf8(template).map_err(|_| "内置 Steam 配置编码错误")?)?;
    if patch.len() != 1 { return Err("内置 Steam 配置根节点无效".into()); }
    if !target.is_empty() {
        if target.len() != 1 { return Err("现有 Steam 配置根节点无效，原文件已保留".into()); }
        if patch[0].0 == "UserLocalConfigStore" && target[0].0 == "InstallConfigStore" {
            // 修复旧版账号模板使用全局根名的问题，同时保留已有内容。
            target[0].0 = patch[0].0.clone();
        }
        if !target[0].0.eq_ignore_ascii_case(&patch[0].0) { return Err("现有 Steam 配置类型不匹配，原文件已保留".into()); }
    }
    merge_into(&mut target, &patch);
    if target == original { return Ok(existing.to_vec()); }
    let mut output = String::new(); render(&target, 0, &mut output);
    Ok(output.into_bytes())
}

#[derive(Debug, PartialEq)]
pub struct Account {
    pub sid64: String,
    pub fields: Vec<(String, String)>,
}

impl Account {
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some((_, current)) = self.fields.iter_mut().find(|(key, _)| key == name) {
            *current = value.into();
        } else {
            self.fields.push((name.into(), value.into()));
        }
    }
}

pub fn parse_users(text: &str) -> Result<Vec<Account>, String> {
    let tree = parse(text)?;
    if tree.len() != 1 || !tree[0].0.eq_ignore_ascii_case("users") { return Err("loginusers.vdf 根节点无效，原文件已保留".into()); }
    let (_, value) = tree.into_iter().next().unwrap();
    let Value::Object(users) = value else { return Err("loginusers.vdf 账号列表格式无效".into()); };
    users.into_iter().map(|(sid64, value)| {
        let Value::Object(fields) = value else { return Err("loginusers.vdf 账号块格式无效".into()); };
        if sid64.parse::<u64>().is_err() {
            return Err("loginusers.vdf 含无法识别的账号字段，原文件已保留".into());
        }
        let fields = fields.into_iter().map(|(key, value)| match value {
            Value::Text(value) => Ok((key, value)),
            _ => Err("loginusers.vdf 含无法识别的账号字段，原文件已保留".to_string()),
        }).collect::<Result<_, _>>()?;
        Ok(Account { sid64, fields })
    }).collect()
}

pub fn render_users(accounts: &[Account]) -> String {
    let users = accounts.iter().map(|a| (a.sid64.clone(), Value::Object(a.fields.iter()
        .map(|(key, value)| (key.clone(), Value::Text(value.clone()))).collect()))).collect();
    let mut output = String::new();
    render(&[("users".into(), Value::Object(users))], 0, &mut output);
    output
}

/// 登录历史可能延迟写入；全局 Accounts 表按 SteamID 绑定实际账号名。
pub fn account_for_id(text: &str, sid64: &str) -> Result<Option<String>, String> {
    let tree = parse(text)?;
    let mut entries = tree.as_slice();
    for key in ["InstallConfigStore", "Software", "Valve", "Steam", "Accounts"] {
        let found: Vec<_> = entries.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).collect();
        match found.as_slice() {
            [] => return Ok(None),
            [(_, Value::Object(children))] => entries = children,
            _ => return Err("Steam 账号映射格式无效，原文件已保留".into()),
        }
    }
    let matches: Vec<_> = entries.iter().filter(|(_, value)| matches!(value, Value::Object(fields)
        if fields.iter().any(|(key, value)| key.eq_ignore_ascii_case("SteamID") && matches!(value, Value::Text(id) if id == sid64))))
        .map(|(name, _)| name).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [name] if !name.is_empty() && !name.chars().any(char::is_control) => Ok(Some((*name).clone())),
        _ => Err("Steam 账号映射不唯一，请检查配置后重试".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_records_preserve_order_escapes_and_unknown_fields() {
        let text = "\u{feff}\"users\" { // Steam may use different whitespace\n\
            \"76561197960265729\" { \"AccountName\" \"fixture\" \"PersonaName\" \"escaped \\\"quote\\\" 中文\" \"FutureField\" \"keep\" } \
            \"76561197960265730\" { \"AccountName\" \"other\" \"AutoLogin\" \"1\" } }";
        let mut accounts = parse_users(text).unwrap();
        assert_eq!(accounts.len(), 2);
        let original_fields = accounts[0].fields.clone();
        accounts[0].set("AutoLogin", "1");
        accounts[1].set("AutoLogin", "0");
        assert_eq!(&accounts[0].fields[..3], original_fields);
        assert_eq!(accounts[1].fields.len(), 2);
        assert_eq!(parse_users(&render_users(&accounts)).unwrap(), accounts);
        assert_eq!(render_users(&[]), "\"users\"\n{\n}\n");
    }

    #[test]
    fn invalid_account_trees_are_rejected_without_partial_results() {
        for text in ["", "\"other\" {}", "\"users\" \"invalid\"", "\"users\" { \"bad-id\" {} }",
            "\"users\" { \"1\" \"invalid\" }", "\"users\" { \"1\" { \"nested\" {} } }",
            "\"users\" { \"1\" {} \"2\" {", "\"users\" {} \"extra\" {}"] {
            assert!(parse_users(text).is_err(), "Accepted invalid accounts: {text}");
        }
    }

    #[test]
    fn preferences_merge_without_losing_other_fields() {
        let original = br#""UserLocalConfigStore" { "system" { "InGameOverlayShortcutKey" "OLD" "unknown" "keep" } "session" { "fixture" "unchanged" } }"#;
        let patch = br#""UserLocalConfigStore" { "system" { "InGameOverlayShortcutKey" "KEY_NONE" } }"#;
        let result = merge(original, patch).unwrap();
        let text = String::from_utf8(result.clone()).unwrap();
        assert!(text.contains("unchanged")); assert!(text.contains("keep")); assert!(!text.contains("OLD"));
        assert_eq!(merge(&result, patch).unwrap(), result);
    }
    #[test]
    fn malformed_text_is_rejected_and_escaped_text_round_trips() {
        assert!(merge(b"broken", br#""InstallConfigStore" {}"#).is_err());
        assert!(merge(br#""InstallConfigStore" { "x" "unfinished""#, br#""InstallConfigStore" {}"#).is_err());
        let original = "\u{feff}\"InstallConfigStore\" { // comment\n\"name\" \"escaped \\\"quote\\\" 中文\" }";
        let result = merge(original.as_bytes(), br#""InstallConfigStore" {}"#).unwrap();
        assert_eq!(parse(original).unwrap(), parse(std::str::from_utf8(&result).unwrap()).unwrap());
    }
}
}
use vdf::Account as RawAccount;

/// 托盘唤起主窗口
fn show_main(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn main() {
    // ① WebView2 检测（每次启动直接查注册表，<1ms）：没有则弹原生引导小窗，装好前不创建任何 WebView
    if !has_webview2() {
        run_guide_window(); // 阻塞直到运行时就绪（或用户放弃）
    }

    // ② 进入 Tauri 主界面（同进程，无需重启）
    tauri::Builder::default()
        // 单实例（防多开）：二次启动 exe 时不新建实例，直接唤起已有主窗口。
        // single-instance 必须最先注册
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::set_paths,
            commands::get_settings,
            commands::set_settings,
            commands::open_data_dir,
            commands::start,
            commands::stop,
            commands::steam_cover,
            commands::accounts_list,
            commands::switch_account,
            commands::delete_account,
            commands::login_new,
            commands::list_logs,
            commands::backup_get,
            commands::backup_save,
            commands::backup_test,
            commands::backup_sync,
            commands::backup_save_password,
        ])
        .setup(|app| {
            // ③ 系统托盘（关闭到托盘模式的找回入口：双击/菜单"显示主窗口"，菜单"退出"）
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{TrayIconBuilder, TrayIconEvent};
            let show = MenuItem::with_id(app, "tray-show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "tray-quit", "退出 GameReady", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().cloned().expect("缺默认图标"))
                .tooltip("GameReady")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray-show" => show_main(app),
                    "tray-quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // ④ 进程活跃监听（R1 绿点）：后台轮询 LOL / Steam 客户端，变化时推 "game-active"
            spawn(app.handle().clone());
            backup::start(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("GameReady 运行失败");
}

// 设置与数据目录

/// 应用总状态（get_status 返回；字段保持 snake_case，前端直接用同名 key）
#[derive(Serialize, Deserialize, Clone)]
pub struct AppStatus {
    pub lol_root: String,
    pub steam_root: String,
    pub paths_valid: PathsValid,
    pub lol_guard_on: bool,
    pub global_cover_on: bool,
    /// steam.exe 是否在运行（写 vdf 前必须退出）
    pub steam_running: bool,
    /// 目标安装中已登录的 ActiveUser 对应 SteamID64
    pub current_steam_id64: Option<String>,
    pub steam_operation: Option<SteamProgress>,
}

/// 路径有效性：能找到 LeagueClient.exe / steam.exe
#[derive(Serialize, Deserialize, Clone)]
pub struct PathsValid {
    pub lol: bool,
    pub steam: bool,
}

/// 用户设置（Data\settings.json 的结构）
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub lol_root: Option<String>,
    pub steam_root: Option<String>,
    pub global_cover_on: bool,
    pub autostart: bool,
    pub start_minimized: bool,
    pub close_behavior: String, // "tray" | "exit"
    pub accent: String,         // 强调色，如 "#00e5ff"
}

impl Default for Settings {
    fn default() -> Self {
        Self { lol_root: None, steam_root: None, global_cover_on: false,
            autostart: false, start_minimized: false,
            close_behavior: "tray".into(), accent: "#00e5ff".into() }
    }
}

/// 覆盖日志条目（Data\logs.jsonl 一行一 JSON）
#[derive(Serialize, Deserialize, Clone)]
pub struct LogEntry {
    pub ts_ms: i64,    // Unix 毫秒
    pub kind: String,  // "lol" | "steam" | "acct"
    pub title: String,
    pub detail: String,
    pub ok: bool,
}

// ==================== 数据目录 ====================

/// exe 同级 Data\ 目录（不存在则创建）；Lazy 缓存，进程内只解析一次
pub fn data_dir() -> PathBuf {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        let base = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("Data");
        // 创建失败（只读盘等罕见情况）不 panic，保留路径让上层 IO 自然报错
        let _ = std::fs::create_dir_all(&dir);
        dir
    });
    DIR.clone()
}

// ==================== 路径探测与校验（供 packs/cover/guard/steam 复用） ====================

/// 在目录中按名字查找条目（小写比对，Windows 文件名大小写不敏感），返回真实路径
fn find_entry_ci(dir: &Path, lower_name: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        if entry.file_name().to_string_lossy().to_lowercase() == lower_name {
            return Some(entry.path());
        }
    }
    None
}

/// 兼容 LeagueClient 子目录与客户端直接位于根目录的安装。
fn lol_root_ok(root: &Path) -> bool {
    if let Some(lc_dir) = find_entry_ci(root, "leagueclient") {
        if find_entry_ci(&lc_dir, "leagueclient.exe").is_some() {
            return true;
        }
    }
    find_entry_ci(root, "leagueclient.exe").is_some()
}

/// Steam 根目录有效：存在 <root>\steam.exe
fn steam_root_ok(root: &Path) -> bool {
    find_entry_ci(root, "steam.exe").is_some()
}

/// 校验两个游戏根目录（能找到对应 exe 即有效）
pub fn validate_roots(lol: &str, steam: &str) -> PathsValid {
    let lol = lol.trim();
    let steam = steam.trim();
    PathsValid {
        lol: !lol.is_empty() && lol_root_ok(Path::new(lol)),
        steam: !steam.is_empty() && steam_root_ok(Path::new(steam)),
    }
}

/// 首次启动自动探测默认根目录，返回 (LOL, Steam)
pub fn detect_default_roots() -> (Option<String>, Option<String>) {
    (detect_lol_root(), detect_steam_root())
}

/// 按 D、C、E 盘顺序探测 WeGame 安装目录，忽略路径大小写。
fn detect_lol_root() -> Option<String> {
    for drive in ["D:", "C:", "E:"] {
        let base = PathBuf::from(format!("{drive}\\"));
        if let Some(wegame) = find_entry_ci(&base, "wegameapps") {
            if let Some(lol_dir) = find_entry_ci(&wegame, "英雄联盟") {
                if lol_root_ok(&lol_dir) {
                    return Some(lol_dir.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

/// 优先注册表，回退标准目录；只有路径有效才返回。
fn detect_steam_root() -> Option<String> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(key) = hklm.open_subkey(r"SOFTWARE\WOW6432Node\Valve\Steam") {
        if let Ok(path) = key.get_value::<String, _>("InstallPath") {
            let p = path.trim().trim_end_matches('/').trim_end_matches('\\');
            if !p.is_empty() && steam_root_ok(Path::new(p)) {
                return Some(p.to_string());
            }
        }
    }
    const FALLBACK: &str = r"C:\Program Files (x86)\Steam";
    if steam_root_ok(Path::new(FALLBACK)) {
        Some(FALLBACK.to_string())
    } else {
        None
    }
}

// ==================== 设置读写 ====================

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// 缺失时创建默认设置；损坏的原文件保留，避免丢失用户配置。
pub fn load_settings() -> Settings {
    if let Ok(text) = std::fs::read_to_string(settings_path()) {
        if let Ok(s) = serde_json::from_str::<Settings>(&text) {
            return s;
        }
    }
    if settings_path().exists() { return Settings::default(); }
    let (lol, steam) = detect_default_roots();
    let s = Settings {
        lol_root: lol,
        steam_root: steam,
        ..Settings::default()
    };
    if !settings_path().exists() { let _ = save_settings(&s); }
    s
}

/// 原子写：先写 .tmp 再 rename（Windows 下 std rename 走 MoveFileEx 可覆盖已存在文件）
pub fn save_settings(s: &Settings) -> Result<(), String> {
    atomic_json(&settings_path(), s)
}
static SETTINGS_OP: Mutex<()> = Mutex::new(());

fn checked_settings() -> Result<Settings, String> {
    match fs::read_to_string(settings_path()) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| "Data\\settings.json 格式损坏，原文件已保留，请修复后再保存设置".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(load_settings()),
        Err(e) => Err(format!("读取设置失败：{e}")),
    }
}

/// Write beside the destination, then replace it only after the complete write succeeds.
/// Frequent game-config writes skip fsync; credentials and settings persist before replacement.
fn atomic_write(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        if durable { file.sync_all()?; }
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("序列化失败：{e}"))?;
    atomic_write(path, &bytes, true).map_err(|e| format!("保存 {} 失败：{e}，原文件已保留",
        path.file_name().unwrap_or_default().to_string_lossy()))
}

// ==================== LOL 守护运行状态（AppStatus.lol_guard_on 数据源） ====================

static GUARD_RUNNING: AtomicBool = AtomicBool::new(false);

/// 由 guard 模块 start/stop 时调用，维护 LOL 守护运行标志
pub fn set_guard_running(on: bool) {
    GUARD_RUNNING.store(on, Ordering::Relaxed);
}

/// SteamID3 转 SteamID64，溢出时返回 None。
fn sid3_to_id64(sid3: &str) -> Option<String> {
    let n: u64 = sid3.trim().parse().ok()?;
    n.checked_add(SID64_BASE)
        .map(|v| v.to_string())
}

// ==================== Tauri commands ====================
// 各 command 的实现统一在文件尾 mod commands（generate_handler 路径引用需要独立 mod）。

// 日志
/// 日志保留上限（R12：超限截断保留最新）
const MAX_ENTRIES: usize = 5000;

/// 全局 AppHandle（spawn 注入一次，此后任意线程的 log() 都能推事件）
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// 文件写锁：守护线程 / blocking 线程 / async command 可能并发写日志，
/// 无锁并发的"追加+截断重写"会产生半截行或丢条（回归走查发现）
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// 注入 AppHandle（仅 spawn 调用）
pub fn set_app(app: tauri::AppHandle) {
    let _ = APP.set(app);
}

/// 日志文件路径：Data\logs.jsonl
fn logs_path() -> PathBuf {
    data_dir().join("logs.jsonl")
}

/// 写一条日志：追加到 Data\logs.jsonl 并推 "log-appended"
/// kind："lol" | "steam" | "acct"；ok：操作成败（前端结果标签）
pub fn log(kind: &str, title: &str, detail: &str, ok: bool) {
    let entry = LogEntry {
        ts_ms: now_ms() as i64,
        kind: kind.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        ok,
    };
    append_and_truncate(&entry);
    if let Some(app) = APP.get() {
        let _ = app.emit("log-appended", &entry);
    }
}

/// 追加一行 JSON；随后检查总行数，超限则重写保留最新 MAX_ENTRIES 条
fn append_and_truncate(entry: &LogEntry) {
    let _lock = LOG_LOCK.lock().unwrap(); // 追加与截断重写整体串行
    let path = logs_path();
    if let Ok(line) = serde_json::to_string(entry) {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }
    // 读回全部行判断是否截断（文件不存在或读失败则直接结束）
    let Ok(f) = File::open(&path) else { return };
    let lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
    if lines.len() <= MAX_ENTRIES {
        return;
    }
    // 文件按追加序即时间升序，保留末尾（最新）MAX_ENTRIES 条；先写临时文件再原子替换
    let keep = &lines[lines.len() - MAX_ENTRIES..];
    let bytes = keep.join("\n") + "\n";
    let _ = atomic_write(&path, bytes.as_bytes(), false);
}

// 日志读取命令位于 commands 模块，按时间降序返回。

// 进程状态监听
/// 轮询间隔（合同：2s）
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 冷启动无条件推送的轮数（覆盖前端晚加载场景，代价仅几次幂等的重复事件）
const WARMUP_ROUNDS: u32 = 5;

/// main.rs setup 调用：为 logger 注入 AppHandle 后启动后台监听线程，立即返回
pub fn spawn(app: tauri::AppHandle) {
    // log 的合同签名无 app 参数，在此注入一次全局句柄供其推 "log-appended"
    set_app(app.clone());
    // 线程创建失败仅意味着绿点不可用，不影响主功能，静默降级
    let _ = thread::Builder::new()
        .name("game-watcher".to_string())
        .spawn(move || run(app));
}

/// 监听主循环（独立线程）
fn run(app: tauri::AppHandle) {
    // 只刷新进程列表，不拉 CPU/内存/磁盘信息，降低 2s 轮询开销
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    let mut last: Option<(bool, bool, Option<String>)> = None; // 活动账号变化也要刷新列表
    let mut round: u32 = 0;
    loop {
        sys.refresh_processes_specifics(ProcessRefreshKind::new());
        let (lol, steam) = scan(&sys);
        let sid = steam.then(current_account_sid3).flatten();
        let cur = (lol, steam, sid);
        if round < WARMUP_ROUNDS || last.as_ref() != Some(&cur) {
            let _ = app.emit("game-active", json!({ "lol": lol, "steam": steam }));
            if last.as_ref().is_some_and(|old| old.2 != cur.2) {
                let _ = app.emit("steam-accounts-changed", ());
            }
            last = Some(cur);
        }
        round = round.saturating_add(1);
        thread::sleep(POLL_INTERVAL);
    }
}

/// 枚举一次进程表，返回 (LOL 活跃, Steam 活跃)；忽略进程名大小写。
fn scan(sys: &System) -> (bool, bool) {
    let mut lol = false;
    let mut steam = false;
    for p in sys.processes().values() {
        let name = p.name();
        if name.eq_ignore_ascii_case("leagueclient.exe") || name.eq_ignore_ascii_case("league of legends.exe") {
            lol = true;
        } else if name.eq_ignore_ascii_case("steam.exe") {
            steam = true;
        }
    }
    (lol, steam)
}

// 内置配置
/// 覆盖范围（write_all 按 scope 过滤内置文件）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackScope {
    /// LOL：游戏内 3 件套 + LCU 客户端 2 个 yaml（守护主战场）
    Lol,
    /// Steam 全局：config\config.vdf
    SteamGlobal,
    /// Steam 当前账号：userdata\{SID3}\config\localconfig.vdf（覆盖时替换占位符）
    SteamAccount,
}

/// 内置包单文件描述：源字节 + 目标相对路径 + 归属范围
pub struct PackFile {
    pub bytes: &'static [u8],
    /// 相对游戏根目录的目标路径（反斜杠分隔，大小写与真实目标严格一致）
    pub rel: &'static str,
    pub scope: PackScope,
}

include!(concat!(env!("OUT_DIR"), "/packs.rs"));

/// 单文件覆盖结果：ok=true 且 err=Some 表示写盘成功但时间戳设置失败（"ts-failed: ..."）
#[derive(Serialize, Clone)]
pub struct CoverResult {
    pub rel: String,
    pub ok: bool,
    pub err: Option<String>,
}

/// 应用指定配置组；账号组需 SID3。写入成功但时间戳失败只记警告。
pub fn write_all(root: &Path, scope: PackScope, sid3: Option<&str>) -> Vec<CoverResult> {
    let mut out = Vec::new();
    for f in FILES.iter().filter(|f| f.scope == scope) {
        // SteamAccount 组必须知道当前账号 SteamID3 才能定位 userdata 目录
        let sid = if matches!(f.scope, PackScope::SteamAccount) {
            match sid3 {
                Some(s) => Some(s),
                None => {
                    out.push(CoverResult {
                        rel: f.rel.to_string(),
                        ok: false,
                        err: Some("跳过：未检测到已登录的 Steam 账号（无 SteamID3）".to_string()),
                    });
                    continue;
                }
            }
        } else {
            None
        };

        let rel = match sid {
            Some(s) => f.rel.replace("{SID3}", s),
            None => f.rel.to_string(),
        };
        let target = root.join(&rel);
        let mut r = CoverResult { rel, ok: true, err: None };
        let bytes = if scope != PackScope::Lol {
            let existing = match fs::read(&target) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => {
                    r.ok = false; r.err = Some(format!("读取现有配置失败：{e}，未覆盖")); out.push(r); continue;
                },
            };
            match vdf::merge(&existing, f.bytes) {
                Ok(bytes) if bytes == existing => { out.push(r); continue; },
                Ok(bytes) => Cow::Owned(bytes),
                Err(e) => { r.ok = false; r.err = Some(e); out.push(r); continue; },
            }
        } else { Cow::Borrowed(f.bytes) };

        // 父目录不存在则创建（userdata\{SID3}\config 等深层目录）
        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                r.ok = false;
                r.err = Some(format!("创建目录失败: {e}"));
                out.push(r);
                continue;
            }
        }

        // 原子写：先写 .tmp 再 rename 覆盖（守护每 200ms 一轮，避免游戏恰好读到半截文件）
        match atomic_write(&target, &bytes, false) {
            Err(e) => {
                r.ok = false;
                r.err = Some(format!("写入失败: {e}"));
            }
            Ok(()) => {
                // 时间戳失败不判为覆盖失败：ok 保持 true，仅在 err 记 warning
                if let Err(e) = set_stamp_2000(&target) {
                    r.err = Some(format!("ts-failed: {e}"));
                }
            }
        }
        out.push(r);
    }
    out
}

/// 把目标文件的创建/修改/访问时间统一设为本地 2000-01-01 00:00:00。
fn set_stamp_2000(path: &Path) -> Result<(), String> {
    use chrono::TimeZone;
    // 本地时区 2000-01-01 00:00:00 对应的 UTC Unix 秒。
    let secs = chrono::Local
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| "本地时间 2000-01-01 00:00:00 换算失败".to_string())?
        .timestamp();

    // SetFileTime sets creation, access and modification time in one call.
    unsafe { set_create_time(path, secs) }
}

/// 用 Win32 API 设置创建时间（创建/修改/访问三项一并写入，保证标识一致）。
/// FILE_FLAG_BACKUP_SEMANTICS：按文件句柄打开已有文件（也是打开目录路径的必备标志）。
unsafe fn set_create_time(path: &Path, unix_secs: i64) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    // 路径转 UTF-16 并补 NUL 结尾（PCWSTR::from_raw 只持有裸指针，wide 必须活到调用结束）
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // dwDesiredAccess = 0x40000000（GENERIC_WRITE，已含 SetFileTime 所需的 FILE_WRITE_ATTRIBUTES）
    let handle = CreateFileW(
        PCWSTR::from_raw(wide.as_ptr()),
        0x4000_0000,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
        None,
    )
    .map_err(|e| format!("打开文件失败: {e}"))?;

    // FILETIME = 自 1601-01-01 起的 100ns 间隔：Unix 秒 ×10^7 + 116444736000000000
    let v = unix_secs * 10_000_000 + 116_444_736_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: v as u32,
        dwHighDateTime: (v >> 32) as u32,
    };
    let r = SetFileTime(
        handle,
        Some(&ft as *const FILETIME),
        Some(&ft as *const FILETIME),
        Some(&ft as *const FILETIME),
    )
    .map_err(|e| format!("创建时间设置失败: {e}"));
    let _ = CloseHandle(handle);
    r
}

// Steam 配置应用
/// 配置写入结果；手动操作经 IPC 返回，自动登录在最终结果中反馈。
#[derive(Serialize, Clone)]
pub struct CoverReport {
    pub total: usize,
    pub ok: usize,
    pub failed: Vec<CoverResult>,
    pub skipped_reason: Option<String>,
    /// 是否已经成功发起覆盖后的 Steam 重启（不等同于再次登录成功）。
    pub auto_restarted: bool,
}

// Steam 将叠加界面缩放保存在 HKCU，此开关通过注册表应用。
fn enable_overlay_scaling(registry: &RegKey) -> CoverResult {
    let result = registry.open_subkey_with_flags(r"Software\Valve\Steam", KEY_SET_VALUE)
        .and_then(|key| key.set_value("OverlayScaleInterface", &1u32));
    CoverResult {
        rel: "Steam 叠加界面缩放".into(),
        ok: result.is_ok(),
        err: result.err().map(|e| format!("开启叠加界面缩放失败：{e}")),
    }
}

/// 只在 Steam 完全退出后调用；已知目标在首次启动前应用，避免重复重启。
fn apply_steam_presets(root: &str, sid3: &str, include_global: bool) -> CoverReport {
    let mut all = write_all(Path::new(root), PackScope::SteamAccount, Some(sid3));
    if include_global { all.extend(write_all(Path::new(root), PackScope::SteamGlobal, None)); }
    all.push(enable_overlay_scaling(&RegKey::predef(HKEY_CURRENT_USER)));
    let report = CoverReport {
        total: all.len(), ok: all.iter().filter(|r| r.ok).count(),
        failed: all.into_iter().filter(|r| !r.ok).collect(),
        skipped_reason: (!include_global).then(|| "未开启「全局配置覆盖」，已跳过 config\\config.vdf".into()),
        auto_restarted: false,
    };
    let detail = report.failed.iter().map(|r| format!("{}：{}", r.rel, r.err.as_deref().unwrap_or("写入失败")))
        .collect::<Vec<_>>().join("；");
    log("steam", "Steam 配置写入", &format!("成功 {}/{}{}{}", report.ok, report.total,
        if detail.is_empty() { "" } else { "；" }, detail), report.failed.is_empty());
    report
}

/// 手动覆盖当前活动账号仍需退出后写入，防止 Steam 内存回写覆盖磁盘。
fn cover_locked(include_global: bool, app: AppHandle, generation: u64) -> Result<CoverReport, String> {
    let root = steam_root_or_err()?;
    let sid3 = active_steam_sid3(&root).ok_or("未检测到已登录的 Steam 账号，请先完成登录")?;
    let sid64 = sid3_to_id64(&sid3).ok_or("当前 Steam 账号无效")?;
    let account = session_account(&root, &sid64, &login_history(&root)?)?
        .ok_or("当前 Steam 账号记录不可用，请完成登录后重试")?;
    steam_progress(&app, generation, "stopping", "正在退出 Steam，等待保存配置", Some(&sid64));
    kill_and_wait()?;
    if let Err(error) = retain_session_account(&root, &account) {
        let recovery = registry_set_autologin_user(&account.account_name)
            .and_then(|()| relaunch_with_cred(&root, &account.account_name));
        return Err(match recovery { Ok(()) => format!("{error}；已重新启动 Steam"),
            Err(recovery) => format!("{error}；恢复 Steam 也失败：{recovery}") });
    }
    steam_progress(&app, generation, "applying", "正在应用 Steam 配置", Some(&sid64));
    let mut report = apply_steam_presets(&root, &sid3, include_global);
    steam_progress(&app, generation, "starting", "配置已写入，正在启动 Steam", Some(&sid64));
    let launched = registry_set_autologin_user(&account.account_name)
        .and_then(|()| relaunch_with_cred(&root, &account.account_name));
    report.auto_restarted = launched.is_ok();
    if let Err(error) = launched {
        report.total += 1;
        report.failed.push(CoverResult { rel: "恢复 Steam".into(), ok: false, err: Some(error.clone()) });
        log("steam", "配置已写入，Steam 启动失败", &error, false);
    }
    Ok(report)
}

// LOL 守护
/// 守护运行句柄（全局唯一，OnceCell 持有；None = 未在运行）
struct GuardHandle {
    stop_flag: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// 守护启动时刻（Unix 毫秒），stop 时计算 elapsed_ms
    start_ms: u64,
    /// 累计覆盖轮次（与线程共享）
    count: Arc<AtomicU64>,
}

static GUARD: OnceLock<Mutex<Option<GuardHandle>>> = OnceLock::new();

/// 守护统计（"guard-stats" 事件 payload / stop 返回值）
#[derive(Serialize, Clone)]
pub struct GuardSummary {
    pub count: u64,
    pub elapsed_ms: u64,
}

/// 守护主循环（独立线程内运行）
fn run_loop(
    app: tauri::AppHandle,
    lol_root: String,
    start_ms: u64,
    stop_flag: Arc<AtomicBool>,
    count: Arc<AtomicU64>,
) {
    let mut fail_streak: u32 = 0; // 连续"全部文件失败"的轮数
    let mut last_emit: Option<Instant> = None;
    let mut current_root = lol_root;          // 热更：路径模态改目录后守护跟着切
    let mut last_root_check = Instant::now();

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break; // 正常停止路径，结束日志由 stop() 写
        }

        // 每秒重读设置：LOL 根目录被修改则切换覆盖目标（写盘仍保持 200ms 节奏）
        if last_root_check.elapsed() >= Duration::from_secs(1) {
            last_root_check = Instant::now();
            if let Some(nr) = load_settings().lol_root {
                if !nr.trim().is_empty() && nr != current_root {
                    current_root = nr;
                }
            }
        }

        let results = write_all(Path::new(&current_root), PackScope::Lol, None);
        // count 语义 = 累计覆盖轮次（UI 显示"累计覆盖次数"），一轮 +1
        count.fetch_add(1, Ordering::SeqCst);

        let all_fail = !results.is_empty() && results.iter().all(|r| !r.ok);
        fail_streak = if all_fail { fail_streak + 1 } else { 0 };

        // 连续 5 轮全部文件失败：写失败日志 + 推 guard-error + 自停
        if fail_streak >= 5 {
            let sample = results
                .iter()
                .find(|r| !r.ok)
                .and_then(|r| r.err.clone())
                .unwrap_or_else(|| "未知错误".to_string());
            log(
                "lol",
                "LOL 守护连续失败自动停止",
                &format!(
                    "连续 {} 轮（每轮 200ms）全部文件覆盖失败，已自动停止守护。示例错误：{}",
                    fail_streak, sample
                ),
                false,
            );
            let _ = app.emit(
                "guard-error",
                serde_json::json!({
                    "message": format!("LOL 守护连续 {} 轮覆盖全部失败，已自动停止。示例错误：{}", fail_streak, sample)
                }),
            );
            // 清空全局句柄，允许再次 start。不 join 自己（线程即将退出，
            // JoinHandle 随 GuardHandle drop 即 detach，无泄漏风险）。
            if let Some(m) = GUARD.get() {
                let mut g = m.lock().unwrap();
                if g.as_ref().is_some_and(|h| Arc::ptr_eq(&h.stop_flag, &stop_flag)) {
                    g.take();
                    set_guard_running(false);
                }
            }
            return;
        }

        // 每 1s 推一次统计（首轮立即推，让前端尽快有数）
        if last_emit.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            let stats = GuardSummary {
                count: count.load(Ordering::SeqCst),
                elapsed_ms: now_ms().saturating_sub(start_ms),
            };
            let _ = app.emit("guard-stats", stats);
            last_emit = Some(Instant::now());
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 当前 Unix 毫秒（时钟异常时返回 0，仅用于展示统计）
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 时长展示（日志用）："3分05秒" 风格简化为 "3分5秒"
fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{}分{}秒", s / 60, s % 60)
    } else {
        format!("{}秒", s)
    }
}

// Steam 账号管理
/// CREATE_NO_WINDOW：taskkill / steam.exe 等子进程一律隐藏控制台窗口，避免闪黑框
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// SteamID64 → SteamID3 换算基数（实测：userdata 目录名 = SteamID64 − 76561197960265728）
const SID64_BASE: u64 = 76_561_197_960_265_728;
/// Steam 操作互斥锁：cover / switch / delete / login_new 全程持锁串行执行。
/// 防止并发杀/启 Steam、并发写 loginusers.vdf 互相踩（覆盖结果被回写、双启、vdf 交错）。
pub(crate) static STEAM_OP: Mutex<()> = Mutex::new(());
static STEAM_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 账号条目（对外合同结构，前端渲染用）
#[derive(Serialize, Clone)]
pub struct SteamAccount {
    pub steam_id64: String,
    pub account_name: String,
    pub persona_name: String,
    pub auto_login: bool,
    pub timestamp: i64,
}

// ---------------- 基础路径与注册表 ----------------

/// 从设置读取 Steam 根目录；未设置返回 Err
fn steam_root_or_err() -> Result<String, String> {
    checked_settings()?
        .steam_root
        .filter(|s| !s.is_empty() && steam_root_ok(Path::new(s)))
        .ok_or_else(|| "Steam 根目录未设置或无效，请先在设置中配置路径".to_string())
}

/// <steam_root>\config\loginusers.vdf
fn vdf_path(root: &str) -> PathBuf {
    Path::new(root).join("config").join("loginusers.vdf")
}

/// 读注册表 HKCU\Software\Valve\Steam 的 AutoLoginUser（REG_SZ 账号名），失败返回 None
fn registry_autologin_user() -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey("Software\\Valve\\Steam").ok()?;
    key.get_value::<String, _>("AutoLoginUser").ok()
}

/// 活跃账号必须属于指定 Steam 安装目录下仍在运行的客户端。
fn active_steam_sid3(root: &str) -> Option<String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Valve\Steam\ActiveProcess").ok()?;
    let pid: u32 = key.get_value("pid").ok()?;
    let sid3: u32 = key.get_value("ActiveUser").ok()?;
    if pid == 0 || sid3 == 0 { return None; }
    let mut sys = System::new();
    let process_id = sysinfo::Pid::from_u32(pid);
    if !sys.refresh_process_specifics(process_id,
        ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::OnlyIfNotSet)) { return None; }
    let process = sys.process(process_id)?;
    if !process.name().eq_ignore_ascii_case("steam.exe") { return None; }
    let actual = fs::canonicalize(process.exe()?).ok()?;
    let expected = fs::canonicalize(Path::new(root).join("steam.exe")).ok()?;
    if !actual.to_string_lossy().eq_ignore_ascii_case(&expected.to_string_lossy())
        || key.get_value::<u32, _>("pid").ok()? != pid
        || key.get_value::<u32, _>("ActiveUser").ok()? != sid3 { return None; }
    Some(sid3.to_string())
}

fn connection_log(root: &str) -> PathBuf { Path::new(root).join("logs").join("connection_log.txt") }

fn login_success_in_log(text: &str, sid3: &str, since: i64) -> Option<i64> {
    use chrono::TimeZone;
    let marker = format!("RecvMsgClientLogOnResponse() : [U:1:{sid3}] 'OK'");
    text.lines().rev().find_map(|line| {
        if !line.contains(&marker) { return None; }
        let timestamp = chrono::NaiveDateTime::parse_from_str(line.get(1..20)?, "%Y-%m-%d %H:%M:%S").ok()?;
        let timestamp = chrono::Local.from_local_datetime(&timestamp).single()?.timestamp();
        (timestamp >= since).then_some(timestamp)
    })
}

fn read_login_success(root: &str, sid3: &str, offset: u64, since: i64) -> Option<i64> {
    let mut file = File::open(connection_log(root)).ok()?;
    let len = file.metadata().ok()?.len();
    // 只读取本次启动后的新增内容；日志轮换后要求时间晚于启动。
    let rotated = len < offset;
    file.seek(SeekFrom::Start(if rotated { 0 } else { offset }.max(len.saturating_sub(128 * 1024)))).ok()?;
    let mut text = String::new();
    file.take(128 * 1024).read_to_string(&mut text).ok()?;
    login_success_in_log(&text, sid3, since + i64::from(rotated))
}

fn session_account(root: &str, sid64: &str, history: &[RawAccount]) -> Result<Option<SteamAccount>, String> {
    let matches: Vec<_> = history.iter().filter(|a| a.sid64 == sid64).collect();
    if matches.len() > 1 { return Err("Steam 账号记录重复，请检查 loginusers.vdf 后重试".into()); }
    if let Some(account) = matches.first() { return Ok(Some(to_account(account))); }
    let text = fs::read_to_string(Path::new(root).join("config").join("config.vdf"))
        .map_err(|e| format!("读取 Steam 活动账号映射失败：{e}"))?;
    let Some(name) = vdf::account_for_id(&text, sid64)? else { return Ok(None); };
    Ok(Some(SteamAccount { steam_id64: sid64.into(), account_name: name, persona_name: String::new(),
        auto_login: false, timestamp: sid64_to_sid3(sid64).and_then(|sid| read_login_success(root, &sid, 0, 0)).unwrap_or(0) }))
}

fn accounts_for_root(root: &str) -> Result<Vec<SteamAccount>, String> {
    let history = login_history(root)?;
    let mut accounts: Vec<_> = history.iter().map(to_account).collect();
    if let Some(sid64) = active_steam_sid3(root).and_then(|sid| sid3_to_id64(&sid)) {
        if !accounts.iter().any(|a| a.steam_id64 == sid64) {
            if let Some(account) = session_account(root, &sid64, &history)? { accounts.push(account); }
        }
    }
    Ok(accounts)
}

/// 仅在已确认会话且 Steam 完全退出后补齐缺失历史，保留其他账号与未知字段。
fn retain_session_account(root: &str, account: &SteamAccount) -> Result<Vec<RawAccount>, String> {
    let mut history = login_history(root)?;
    if !history.iter().any(|a| a.sid64 == account.steam_id64) {
        history.push(RawAccount { sid64: account.steam_id64.clone(), fields: vec![
            ("AccountName".into(), account.account_name.clone()), ("PersonaName".into(), account.persona_name.clone()),
            ("Timestamp".into(), account.timestamp.to_string()),
        ] });
    }
    select_login_account(root, &mut history, &account.steam_id64)?;
    Ok(history)
}

/// 写注册表 HKCU\Software\Valve\Steam 的 AutoLoginUser = account（REG_SZ）
fn registry_set_autologin_user(account: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags("Software\\Valve\\Steam", KEY_SET_VALUE)
        .map_err(|e| format!("打开注册表 Valve\\Steam 失败: {e}"))?;
    key.set_value("AutoLoginUser", &account)
        .map_err(|e| format!("写入注册表 AutoLoginUser 失败: {e}"))
}

// ---------------- vdf 解析与重建 ----------------

/// 从原始账号块取字段值
fn field<'a>(a: &'a RawAccount, name: &str) -> Option<&'a str> {
    a.fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// 原始账号块 → 对外 SteamAccount
fn to_account(a: &RawAccount) -> SteamAccount {
    let account_name = field(a, "AccountName").unwrap_or("").to_string();
    let persona_name = field(a, "PersonaName").unwrap_or("").to_string();
    let auto_login = field(a, "AutoLogin") == Some("1");
    let timestamp = field(a, "Timestamp")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    SteamAccount {
        steam_id64: a.sid64.clone(),
        account_name,
        persona_name,
        auto_login,
        timestamp,
    }
}

/// Read and validate account records in one parse; reject corrupt input before any write.
fn read_vdf(root: &str) -> Result<Vec<RawAccount>, String> {
    let bytes = fs::read(vdf_path(root)).map_err(|e| format!("读取 loginusers.vdf 失败: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| "loginusers.vdf 不是 UTF-8 文本，原文件已保留".to_string())?;
    vdf::parse_users(&text)
}

/// Steam must be fully stopped before replacing loginusers.vdf.
fn write_vdf(root: &str, accounts: &[RawAccount]) -> Result<(), String> {
    atomic_write(&vdf_path(root), vdf::render_users(accounts).as_bytes(), false)
        .map_err(|e| format!("写入 loginusers.vdf 失败: {e}，原文件已保留"))
}

/// 显示名格式（合同）：PersonaName(AccountName)；昵称缺失时退化为 AccountName(AccountName)
fn display_name(persona: &str, account: &str) -> String {
    let p = if persona.is_empty() { account } else { persona };
    format!("{}({})", p, account)
}

/// SteamID64 → SteamID3 字符串（非数字或越界返回 None）
fn sid64_to_sid3(sid64: &str) -> Option<String> {
    sid64
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_sub(SID64_BASE))
        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
        .map(|n| n.to_string())
}

// ---------------- 进程控制 ----------------

/// 只刷新进程基本信息，判断 steam.exe 是否运行（不含 steamwebhelper）。
pub fn is_steam_running() -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessRefreshKind::new());
    sys.processes()
        .values()
        .any(|p| p.name().eq_ignore_ascii_case("steam.exe"))
}

/// 强杀 Steam（steam.exe + steamwebhelper.exe，CREATE_NO_WINDOW）。
/// 合同 pub API（R17）：供 cover 模块复用。返回是否确实发出了 kill（未运行返回 false）。
pub fn kill_steam_if_running() -> bool {
    if !is_steam_running() {
        return false;
    }
    for exe in ["steam.exe", "steamwebhelper.exe"] {
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", exe])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    true
}

/// 复用进程快照，每 100ms 等待 Steam 退出，最多 timeout_secs 秒。
pub fn wait_steam_exit(timeout_secs: u32) -> bool {
    use sysinfo::ProcessRefreshKind;
    let mut sys = System::new();
    let alive = |sys: &System| sys.processes().values().any(|p| p.name().eq_ignore_ascii_case("steam.exe"));
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.into());
    loop {
        sys.refresh_processes_specifics(ProcessRefreshKind::new());
        if !alive(&sys) {
            return true;
        }
        if Instant::now() >= deadline { return false; }
        thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// 优雅退出 Steam，8 秒未退则强杀；仍未退出时返回错误。
fn kill_and_wait() -> Result<(), String> {
    if !is_steam_running() {
        return Ok(());
    }
    // ① 官方优雅关闭
    if let Ok(root) = steam_root_or_err() {
        let exe = Path::new(&root).join("steam.exe");
        let _ = Command::new(&exe)
            .arg("-shutdown")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    if wait_steam_exit(8) {
        return Ok(());
    }
    // ② 兜底强杀
    kill_steam_if_running();
    if wait_steam_exit(10) {
        Ok(())
    } else {
        Err("Steam 未能在 10 秒内完全退出，请手动退出后重试".to_string())
    }
}

/// 后台启动 Steam，隐藏控制台，不等待进程退出。
pub fn launch_steam(root: &str, args: &[&str]) -> Result<(), String> {
    let exe = Path::new(root).join("steam.exe");
    Command::new(&exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 steam.exe 失败: {e}"))
}

/// 优先用保存的密码重启，缺少密码时无参启动；启动错误由调用方反馈。
fn relaunch_with_cred(root: &str, account: &str) -> Result<(), String> {
    let pass = if account.is_empty() { None } else { get_cred(account) };
    match &pass {
        Some(p) => launch_steam(root, &["-login", account, p.as_str()]),
        None => launch_steam(root, &[]),
    }
}

// ==================== 凭据存储（2026-09-20 用户拍板：自用工具，明文存 Data\accounts.json） ====================
// 列表登录与覆盖/删除后的重启优先使用已保存凭据，并由 Steam 登录状态确认结果。
// 实测诊断（全文件唯一完整版，他处一句话引用）：
// ① -login 参数登录不保存 Steam 端免密凭据，vdf 里 RememberPassword 停留 0；
// ② Steam 缓存凭据可能失效或需要 Steam Guard，不能只凭预选项认定登录成功。

fn creds_path() -> PathBuf {
    data_dir().join("accounts.json")
}

fn read_creds(path: &Path) -> Result<std::collections::BTreeMap<String, String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| "Data\\accounts.json 格式损坏，原文件已保留，请修复后重试".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("读取本地账号文件失败：{e}")),
    }
}

/// 键 = 账号名（登录发起前即已知，无需等 vdf 出现 sid64）
pub fn save_cred_by_name(account: &str, password: &str) -> Result<(), String> {
    let mut m = read_creds(&creds_path())?;
    replace_credential(&mut m, account, password);
    backup::login_attempt_started(account).map_err(|e| e.message)?;
    atomic_json(&creds_path(), &m)?;
    backup::credentials_changed().map_err(|e| e.message)
}

fn replace_credential(m: &mut std::collections::BTreeMap<String, String>, account: &str, password: &str) {
    m.retain(|k, _| !k.eq_ignore_ascii_case(account));
    m.insert(account.to_ascii_lowercase(), password.into());
}

pub fn remove_cred_by_name(account: &str) -> Result<(), String> {
    let mut m = read_creds(&creds_path())?;
    m.retain(|k, _| !k.eq_ignore_ascii_case(account));
    backup::login_attempt_started(account).map_err(|e| e.message)?;
    atomic_json(&creds_path(), &m)?;
    backup::credentials_changed().map_err(|e| e.message)
}

fn get_cred(account: &str) -> Option<String> {
    // 忽略大小写匹配（用户输入的账号名与 vdf 回读的 AccountName 可能大小写不同）
    let creds = match read_creds(&creds_path()) {
        Ok(creds) => creds,
        Err(e) => { log("acct", "读取本地登录凭据失败", &e, false); return None; }
    };
    let matches: Vec<_> = creds.iter().filter(|(k, _)| k.eq_ignore_ascii_case(account)).collect();
    if matches.len() > 1 {
        log("acct", "本地账号有重复凭据", "请在新增登录栏重新输入最新密码，保存后将自动合并大小写重复项", false);
        return None;
    }
    matches.first().map(|(_, v)| (*v).clone())
}

// ---------------- 核心功能（合同 pub API） ----------------

/// 解析 <steam_root>\config\loginusers.vdf 输出账号列表；根目录未设置/文件不存在返回空列表
pub fn accounts_list_v() -> Vec<SteamAccount> {
    let root = match steam_root_or_err() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    accounts_for_root(&root).unwrap_or_default()
}

/// AutoLoginUser 只是预选项；活动账号必须属于该安装目录的真实 Steam 进程。
pub fn current_account_sid3() -> Option<String> { active_steam_sid3(&steam_root_or_err().ok()?) }

#[derive(Serialize, Deserialize, Clone)]
pub struct SteamProgress {
    pub generation: u64,
    pub phase: String,
    pub message: String,
    pub sid64: Option<String>,
}
static STEAM_PROGRESS: Mutex<Option<SteamProgress>> = Mutex::new(None);

fn steam_progress(app: &AppHandle, generation: u64, phase: &str, message: &str, sid64: Option<&str>) {
    if STEAM_GENERATION.load(Ordering::SeqCst) != generation { return; }
    let progress = SteamProgress { generation, phase: phase.into(), message: message.into(), sid64: sid64.map(str::to_string) };
    *STEAM_PROGRESS.lock().unwrap() = Some(progress.clone());
    let _ = app.emit("steam-operation", progress);
}

/// 一个登录尝试携带固定路径、凭据快照与已应用结果；等待时不占用操作锁。
struct LoginAttempt {
    generation: u64,
    root: String,
    account: String,
    password: Option<String>,
    sid64: Option<String>,
    baseline: i64,
    history: Vec<(String, i64)>,
    log_offset: u64,
    log_since: i64,
    include_global: bool,
    cover: Option<CoverReport>,
    restoring: bool,
    started: Instant,
}

fn find_login_target(accounts: &[RawAccount], sid64: Option<&str>, name: &str) -> Result<Option<SteamAccount>, String> {
    let matches: Vec<_> = accounts.iter().filter(|a| match sid64 {
        Some(sid64) => a.sid64 == sid64,
        None => field(a, "AccountName").is_some_and(|n| n.eq_ignore_ascii_case(name)),
    }).collect();
    if matches.len() > 1 { return Err("Steam 账号记录重复，请检查 loginusers.vdf 后重试".into()); }
    let account = matches.first().map(|a| to_account(a));
    if sid64.is_some() && account.is_none() { return Err("账号不存在，请刷新账号列表".into()); }
    if account.as_ref().is_some_and(|a| a.account_name.is_empty() || sid64_to_sid3(&a.steam_id64).is_none()) {
        return Err("Steam 账号记录不完整，原文件已保留".into());
    }
    Ok(account)
}

fn login_history(root: &str) -> Result<Vec<RawAccount>, String> {
    if !vdf_path(root).exists() { return Ok(Vec::new()); }
    read_vdf(root)
}

fn select_login_account(root: &str, accounts: &mut [RawAccount], sid64: &str) -> Result<(), String> {
    for account in accounts.iter_mut() {
        let selected = account.sid64 == sid64;
        account.set("AutoLogin", if selected { "1" } else { "0" });
        account.set("MostRecent", if selected { "1" } else { "0" });
        if selected { account.set("RememberPassword", "1"); }
    }
    write_vdf(root, accounts)
}

impl LoginAttempt {
    fn current(&self) -> bool {
        let saved = self.password.as_ref().and_then(|_| get_cred(&self.account));
        pending_login_matches(self.generation, STEAM_GENERATION.load(Ordering::SeqCst),
            self.password.as_deref(), saved.as_deref())
            && load_settings().steam_root.as_deref() == Some(self.root.as_str())
    }
    fn launch(&mut self) -> Result<(), String> {
        self.log_offset = fs::metadata(connection_log(&self.root)).map(|m| m.len()).unwrap_or(0);
        self.log_since = chrono::Utc::now().timestamp();
        registry_set_autologin_user(&self.account)?;
        match self.password.as_deref() {
            Some(password) => launch_steam(&self.root, &["-login", &self.account, password]),
            None => launch_steam(&self.root, &[]),
        }
    }
    fn observed(&self, accounts: &[SteamAccount], active_sid64: Option<&str>) -> Result<Option<SteamAccount>, String> {
        // 退出后的记录作基线；其他账号出现新的登录记录时，不再将后来的登录归因于旧密码。
        if accounts.iter().any(|a| !a.account_name.eq_ignore_ascii_case(&self.account)
            && a.timestamp > self.history.iter().find(|(sid, _)| sid == &a.steam_id64).map(|(_, ts)| *ts).unwrap_or(0)) {
            return Err("检测到 Steam 已切换其他账号，本次自动登录确认已停止，请重新选择目标账号".into());
        }
        Ok(accounts.iter().find(|a| login_observed(a, active_sid64, &self.account, self.baseline)
            && self.sid64.as_ref().is_none_or(|target| target == &a.steam_id64)).cloned())
    }
    fn confirmed(&self) -> Result<Option<SteamAccount>, String> {
        let Some(sid3) = active_steam_sid3(&self.root) else { return Ok(None); };
        let sid64 = sid3_to_id64(&sid3);
        let Ok(records) = login_history(&self.root) else { return Ok(None); };
        if let Some(account) = self.observed(&records.iter().map(to_account).collect::<Vec<_>>(), sid64.as_deref())? {
            return Ok(Some(account));
        }
        // 首次 -login 会话可能尚无历史记录，以本次服务器 OK 和真实 SteamID 映射确认。
        let Some(sid64) = sid64 else { return Ok(None); };
        let Ok(Some(account)) = session_account(&self.root, &sid64, &records) else { return Ok(None); };
        if active_steam_sid3(&self.root).as_deref() != Some(&sid3) { return Ok(None); }
        Ok(self.observed_connection(account, read_login_success(&self.root, &sid3, self.log_offset, self.log_since)))
    }
    fn observed_connection(&self, mut account: SteamAccount, timestamp: Option<i64>) -> Option<SteamAccount> {
        if !account.account_name.eq_ignore_ascii_case(&self.account)
            || self.sid64.as_ref().is_some_and(|target| target != &account.steam_id64) { return None; }
        account.timestamp = timestamp?;
        Some(account)
    }
    fn pending(&self) -> LoginResult {
        let message = if self.restoring { "配置已写入，正在等待 Steam 恢复登录；如有验证提示，请在 Steam 完成" }
            else { "Steam 已启动，正在等待登录确认；如有验证码或手机确认，请在 Steam 完成，程序会自动继续" };
        LoginResult { generation: self.generation, pending: true, name: self.account.clone(),
            message: format!("{}：{message}", self.account), ok: false }
    }
}

/// 停止→应用已知账号配置→启动严格串行；启动后立即交还界面并在后台确认。
fn start_login(sid64: Option<String>, account: String, password: Option<String>, app: AppHandle) -> Result<LoginResult, String> {
    let _steam_op = STEAM_OP.try_lock().map_err(|_| "Steam 正在应用操作，请稍候重试")?;
    let generation = STEAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    steam_progress(&app, generation, "starting", "正在准备 Steam 登录", sid64.as_deref());
    let result = (|| {
        let root = steam_root_or_err()?;
        let history = login_history(&root)?;
        let known = find_login_target(&history, sid64.as_deref(), account.trim())?;
        let account = known.as_ref().map(|a| a.account_name.clone()).unwrap_or_else(|| account.trim().into());
        if account.is_empty() || account.contains(['\r', '\n', '\0'])
            || password.as_ref().is_some_and(|p| p.is_empty() || p.contains(['\r', '\n', '\0'])) {
            return Err("账号和密码不能为空或含换行".to_string());
        }
        let password = match password {
            Some(password) => { save_cred_by_name(&account, &password)?; Some(password) },
            None => {
                let saved = get_cred(&account);
                if saved.is_some() { backup::login_attempt_started(&account).map_err(|e| e.message)?; }
                saved
            },
        };
        let mut attempt = LoginAttempt { generation, root, account, password,
            sid64: known.map(|a| a.steam_id64), baseline: 0, history: Vec::new(), log_offset: 0, log_since: 0,
            include_global: load_settings().global_cover_on,
            cover: None, restoring: false, started: Instant::now() };
        let previous = active_steam_sid3(&attempt.root).and_then(|sid| sid3_to_id64(&sid))
            .and_then(|sid| history.iter().find(|a| a.sid64 == sid).map(to_account));
        let was_running = is_steam_running();
        steam_progress(&app, generation, "stopping", "正在退出旧 Steam，等待保存配置", attempt.sid64.as_deref());
        kill_and_wait()?;
        let prepared = (|| {
            // 优雅退出后重新读取，保留 Steam 刚刚回写的字段和时间戳。
            let mut history = login_history(&attempt.root)?;
            attempt.history = history.iter().map(to_account).map(|a| (a.steam_id64, a.timestamp)).collect();
            if let Some(target) = find_login_target(&history, attempt.sid64.as_deref(), &attempt.account)? {
                attempt.baseline = target.timestamp;
                attempt.sid64 = Some(target.steam_id64.clone());
                select_login_account(&attempt.root, &mut history, &target.steam_id64)?;
                steam_progress(&app, generation, "applying", "正在启动前应用账号配置", attempt.sid64.as_deref());
                attempt.cover = Some(apply_steam_presets(&attempt.root, &sid64_to_sid3(&target.steam_id64).unwrap(), attempt.include_global));
            }
            steam_progress(&app, generation, "starting", "正在启动 Steam 登录目标账号", attempt.sid64.as_deref());
            attempt.launch()
        })();
        if let Err(error) = prepared {
            if was_running {
                let recovery = (|| {
                    let name = if let Some(previous) = &previous {
                        select_login_account(&attempt.root, &mut login_history(&attempt.root)?, &previous.steam_id64)?;
                        previous.account_name.as_str()
                    } else { "" };
                    registry_set_autologin_user(name)?;
                    relaunch_with_cred(&attempt.root, name)
                })();
                if let Err(recovery) = recovery { return Err(format!("{error}；恢复原 Steam 也失败：{recovery}")); }
            }
            return Err(error);
        }
        if attempt.password.is_none() { backup::missing_password(); }
        let pending = attempt.pending();
        steam_progress(&app, generation, "waiting", &pending.message, attempt.sid64.as_deref());
        watch_login(attempt, app.clone())?;
        Ok(pending)
    })();
    if let Err(error) = &result {
        steam_progress(&app, generation, "idle", "", None);
        log("acct", "Steam 登录未完成", error, false);
    }
    result
}

/// ActiveUser 必须来自目标安装的活动进程，且本次登录记录更新；预选账号不算登录。
fn login_observed(account: &SteamAccount, active_sid64: Option<&str>, target: &str, baseline: i64) -> bool {
    !target.is_empty() && account.account_name.eq_ignore_ascii_case(target)
        && active_sid64 == Some(account.steam_id64.as_str()) && account.timestamp > baseline
}

fn pending_login_matches(expected_generation: u64, generation: u64, password: Option<&str>, saved: Option<&str>) -> bool {
    expected_generation == generation && password.is_none_or(|password| saved == Some(password))
}

fn watch_login(mut attempt: LoginAttempt, app: AppHandle) -> Result<(), String> {
    thread::Builder::new().name("steam-login-confirm".into()).spawn(move || {
        let mut stage_started = Instant::now();
        let mut deadline = stage_started + Duration::from_secs(600);
        let outcome = (|| -> Result<Option<SteamAccount>, String> {
            loop {
                if STEAM_GENERATION.load(Ordering::SeqCst) != attempt.generation { return Ok(None); }
                if !attempt.current() { return Err("登录期间密码或路径已改变，本次确认已停止，请重新登录".into()); }
                if attempt.confirmed()?.is_some() {
                    if let Ok(_steam_op) = STEAM_OP.try_lock() {
                        if STEAM_GENERATION.load(Ordering::SeqCst) != attempt.generation { return Ok(None); }
                        if !attempt.current() { return Err("登录期间密码或路径已改变，本次确认已停止，请重新登录".into()); }
                        // 取得锁后再次确认，避免外部 Steam 切号使第一次观察过时。
                        if let Some(account) = attempt.confirmed()? {
                            if !attempt.restoring {
                                if let Some(password) = &attempt.password { backup::confirmed_login(&attempt.account, password, "Steam 登录确认"); }
                            }
                            if attempt.cover.is_some() { return Ok(Some(account)); }
                            // 真正首次新账号只有这里需要一次补配置恢复。
                            let sid3 = active_steam_sid3(&attempt.root).ok_or("当前 Steam 会话已退出")?;
                            validate_cover_account(&sid3, Some(&account.steam_id64))?;
                            attempt.sid64 = Some(account.steam_id64.clone());
                            steam_progress(&app, attempt.generation, "stopping", "首次账号已登录，正在退出 Steam 以应用账号配置", attempt.sid64.as_deref());
                            kill_and_wait()?;
                            let prepared = (|| -> Result<(), String> {
                                let history = retain_session_account(&attempt.root, &account)?;
                                attempt.history = history.iter().map(to_account).map(|a| (a.steam_id64, a.timestamp)).collect();
                                attempt.baseline = history.iter().find(|a| a.sid64 == account.steam_id64)
                                    .map(to_account).ok_or("Steam 账号记录不可用")?.timestamp;
                                steam_progress(&app, attempt.generation, "applying", "正在应用首次账号配置", attempt.sid64.as_deref());
                                attempt.cover = Some(apply_steam_presets(&attempt.root, &sid3, attempt.include_global));
                                Ok(())
                            })();
                            attempt.restoring = true;
                            steam_progress(&app, attempt.generation, "restoring", "正在恢复 Steam 登录", attempt.sid64.as_deref());
                            // 即使停机后的读取失败也尝试启动一次，原错误仍如实反馈。
                            let launched = attempt.launch();
                            if let Err(error) = prepared {
                                return Err(match launched { Ok(()) => format!("配置未完成：{error}；已重新启动 Steam"),
                                    Err(recovery) => format!("配置未完成：{error}；恢复 Steam 也失败：{recovery}") });
                            }
                            launched.map_err(|error| format!("配置已写入，但 Steam 启动失败：{error}"))?;
                            stage_started = Instant::now();
                            deadline = stage_started + Duration::from_secs(600);
                            let pending = attempt.pending();
                            steam_progress(&app, attempt.generation, "waiting", &pending.message, attempt.sid64.as_deref());
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Err(if attempt.restoring { "配置已写入，但仍未确认 Steam 恢复登录，请在 Steam 检查登录状态".into() }
                        else { "10 分钟内仍未确认登录，请检查 Steam 的验证码、网络或账号密码后重试".into() });
                }
                let interval = if stage_started.elapsed() < Duration::from_secs(20) { Duration::from_millis(250) } else { Duration::from_secs(1) };
                thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
            }
        })();
        match outcome {
            Ok(Some(account)) => {
                let report = attempt.cover.take().expect("confirmed login has a prepared report");
                let name = display_name(&account.persona_name, &account.account_name);
                // 首次账号恢复期间 NAS 可能已完成；读取此刻状态，不重复上传。
                let result = LoginResult::from_cover(name, Ok(report))
                    .with_backup_note(backup::login_backup_note(&attempt.account, attempt.password.as_deref()));
                finish_login(&attempt, result, &app);
            },
            Ok(None) => (), // 新操作拥有状态，旧尝试不能清除它。
            Err(error) => {
                let mut result = attempt.pending();
                result.pending = false;
                result.message = format!("{}：{error}", attempt.account);
                finish_login(&attempt, result, &app);
            },
        }
    }).map(|_| ()).map_err(|e| format!("Steam 已启动，但后台登录确认未能启动：{e}"))
}

fn validate_cover_account(sid3: &str, expected_sid64: Option<&str>) -> Result<(), String> {
    if expected_sid64.is_some_and(|expected| sid3_to_id64(sid3).as_deref() != Some(expected)) {
        return Err("当前 Steam 账号已变化，已取消本次自动覆盖，请重新登录目标账号".into());
    }
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct LoginResult {
    pub name: String, pub message: String, pub ok: bool, pub pending: bool, pub generation: u64,
}

impl LoginResult {
    fn from_cover(name: String, cover: Result<CoverReport, String>) -> Self {
        let (ok, note) = match cover {
            Ok(report) if report.failed.is_empty() && report.ok == report.total =>
                (true, format!("配置覆盖完成（{}/{}）", report.ok, report.total)),
            Ok(report) => (false, format!("配置覆盖未完成（{}/{}，详见日志）", report.ok, report.total)),
            Err(error) => (false, format!("配置覆盖未完成：{error}")),
        };
        Self { message: format!("已登录 {name}，{note}"), name, ok, pending: false, generation: 0 }
    }

    fn with_backup_note(mut self, note: Option<String>) -> Self {
        if let Some(note) = note { self.message.push_str(&format!("；{note}")); }
        self
    }
}

fn finish_login(attempt: &LoginAttempt, mut result: LoginResult, app: &AppHandle) {
    if STEAM_GENERATION.load(Ordering::SeqCst) != attempt.generation { return; }
    result.generation = attempt.generation;
    log("acct", "Steam 登录与配置", &format!("{}（用时 {}）", result.message, fmt_duration(attempt.started.elapsed().as_millis() as u64)), result.ok);
    steam_progress(app, attempt.generation, "idle", "", attempt.sid64.as_deref());
    let _ = app.emit("steam-accounts-changed", ());
    let _ = app.emit("steam-login-finished", result);
}

/// 只清理本机账号与 userdata；强杀避免退出回写复活账号，必要时恢复原会话。
fn delete_locked(sid64: &str) -> Result<String, String> {
    let root = steam_root_or_err()?;
    // 杀 Steam 前锁定当前账号名（Steam 退出过程会临时清掉注册表 AutoLoginUser），
    // 重启用它凭据直登恢复会话；被删的若是它则凭据已删，自然退化为无参启动
    let autologin_before = registry_autologin_user().unwrap_or_default();
    let mut accounts = read_vdf(&root)?;
    let idx = accounts
        .iter()
        .position(|a| a.sid64 == sid64)
        .ok_or_else(|| "账号不存在，请刷新账号列表".to_string())?;
    let removed = accounts.remove(idx);
    let account_name = field(&removed, "AccountName").unwrap_or("").to_string();
    let persona_name = field(&removed, "PersonaName").unwrap_or("").to_string();
    if account_name.is_empty() {
        return Err("该账号缺少 AccountName 字段，loginusers.vdf 可能已损坏".to_string());
    }
    let was_running = kill_steam_if_running();
    if was_running && !wait_steam_exit(10) {
        return Err("Steam 未能在 10 秒内完全退出，请手动退出后再删除账号".to_string());
    }
    if let Err(e) = write_vdf(&root, &accounts) {
        if was_running {
            if let Err(recovery) = relaunch_with_cred(&root, &autologin_before) { log("steam", "恢复 Steam 失败", &recovery, false); }
        }
        return Err(e);
    }
    // 同步删除保存的登录凭据（完全清理的一部分）
    let mut cleanup_errors = Vec::new();
    if let Err(e) = remove_cred_by_name(&account_name) { cleanup_errors.push(e); }
    // 删除该账号的 userdata 目录（含 localconfig.vdf 等）；不存在或删除失败不阻断流程
    if let Some(sid3) = sid64_to_sid3(sid64) {
        let ud = Path::new(&root).join("userdata").join(&sid3);
        if let Err(e) = fs::remove_dir_all(ud) {
            if e.kind() != std::io::ErrorKind::NotFound { cleanup_errors.push(format!("账号数据目录删除失败：{e}")); }
        }
    }
    // 删除前 Steam 在运行 → 自动重启：被删的正是当前账号则无参启动（回选择页属预期），
    // 否则凭据直登恢复原会话
    if was_running {
        let target = if account_name.eq_ignore_ascii_case(&autologin_before) {
            ""
        } else {
            autologin_before.as_str()
        };
        if let Err(error) = relaunch_with_cred(&root, target) { cleanup_errors.push(format!("恢复 Steam 失败：{error}")); }
    }
    if !cleanup_errors.is_empty() {
        return Err(format!("账号已从 Steam 列表移除，但部分清理未完成：{}", cleanup_errors.join("；")));
    }
    Ok(display_name(&persona_name, &account_name))
}

// ---------------- Tauri 命令：统一在文件尾 mod commands（成功写日志 + 推 "steam-accounts-changed"） ----------------

// WebView2 安装引导

/// EdgeUpdate Clients 的 WebView2 固定 GUID 子键（检测读它的 pv 值）。
const CLIENTS_SUBKEY: &str = concat!(
    r"Microsoft\EdgeUpdate\Clients\",
    r"{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
);

/// 手动模式打开的微软官方引导安装器下载页（fwlink 永久短链，点开立即下载，约 1.8MB）。
const MANUAL_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// 自动模式下载到 %TEMP% 的离线包文件名。
const SETUP_TEMP_NAME: &str = "GameReady-WebView2Setup.exe";

/// 检查四处注册表，回退检查 WebView2 安装目录。
pub fn has_webview2() -> bool {
    let hklm_wow64 = format!(r"SOFTWARE\WOW6432Node\{}", CLIENTS_SUBKEY);
    let hklm_native = format!(r"SOFTWARE\{}", CLIENTS_SUBKEY);
    let hkcu_native = format!(r"Software\{}", CLIENTS_SUBKEY);
    let hkcu_wow64 = format!(r"Software\WOW6432Node\{}", CLIENTS_SUBKEY);

    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    reg_has_pv(&hklm, &hklm_wow64)
        || reg_has_pv(&hklm, &hklm_native)
        || reg_has_pv(&hkcu, &hkcu_native)
        || reg_has_pv(&hkcu, &hkcu_wow64)
        || webview_dir_exists()
}

/// 保持原生引导窗口，安装完成后继续；放弃安装则退出。
pub fn run_guide_window() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GameReady 安装引导")
            .with_inner_size([420.0, 260.0])
            .with_min_inner_size([420.0, 260.0])
            .with_resizable(false),
        ..Default::default()
    };

    // 窗口正常返回（安装成功关窗 / 用户确认退出前 exit / Alt+F4 等）后兜底复查：
    // 未装好即无法进入 tauri 主界面，直接退出进程。
    let _ = eframe::run_native(
        "GameReady 安装引导",
        options,
        Box::new(|cc| {
            let app: Box<dyn eframe::App> = Box::new(GuideApp::new(cc));
            Ok(app)
        }),
    );
    if !has_webview2() {
        std::process::exit(0);
    }
}

// 检测实现

/// 在指定预定义根键下查子键是否有非空 pv 值（REG_SZ 字符串）。
fn reg_has_pv(root: &winreg::RegKey, path: &str) -> bool {
    use winreg::enums::KEY_READ;
    root.open_subkey_with_flags(path, KEY_READ)
        .ok()
        .and_then(|k| k.get_value::<String, _>("pv").ok())
        .map(|v: String| !v.is_empty())
        .unwrap_or(false)
}

/// 兜底：检查固定安装目录是否存在（覆盖注册表信息缺失但文件在的边缘情况）。
fn webview_dir_exists() -> bool {
    // 首选 %ProgramFiles(x86)%（标准 64 位系统上的固定安装位置）
    let base = std::env::var_os("ProgramFiles(x86)")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("ProgramFiles").map(std::path::PathBuf::from));
    if let Some(b) = base {
        if b.join(r"Microsoft\EdgeWebView\Application").is_dir() {
            return true;
        }
    }
    // 环境变量异常时的硬编码兜底
    std::path::Path::new(r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application").is_dir()
}

/// 优先动态查询 IsWow64Process2，旧系统回退环境变量。
fn native_arch() -> &'static str {
    // Resolve dynamically so older Windows versions can use the environment fallback.
    type IsWow64Process2 = unsafe extern "system" fn(HANDLE, *mut u16, *mut u16) -> BOOL;
    if let Ok(module) = unsafe { GetModuleHandleW(w!("kernel32.dll")) } {
        if let Some(proc) = unsafe { GetProcAddress(module, s!("IsWow64Process2")) } {
            let query: IsWow64Process2 = unsafe { std::mem::transmute(proc) };
            let (mut process, mut native) = (0, 0);
            if unsafe { query(GetCurrentProcess(), &mut process, &mut native) }.as_bool() {
                return match native { 0xAA64 => "arm64", 332 => "x86", _ => "x64" };
            }
        }
    }
    // ② 环境变量兜底：WOW64/模拟环境下 PROCESSOR_ARCHITEW6432 给出原生架构
    if let Ok(native) = std::env::var("PROCESSOR_ARCHITEW6432") {
        return match native.trim().to_ascii_uppercase().as_str() {
            "ARM64" => "arm64",
            "X86" => "x86",
            _ => "x64", // "AMD64" 及其它
        };
    }
    // ③ 非模拟环境：本进程架构即原生架构
    match std::env::var("PROCESSOR_ARCHITECTURE") {
        Ok(a) if a.eq_ignore_ascii_case("ARM64") => "arm64",
        Ok(a) if a.eq_ignore_ascii_case("x86") => "x86",
        _ => "x64",
    }
}

/// 按架构返回微软官方离线包直链（fwlink 永久短链，点开立即下载）。
fn offline_url(arch: &str) -> &'static str {
    match arch {
        "arm64" => "https://go.microsoft.com/fwlink/p/?LinkId=2099616",
        "x86" => "https://go.microsoft.com/fwlink/p/?LinkId=2099617",
        _ => "https://go.microsoft.com/fwlink/p/?LinkId=2124701", // x64
    }
}

// 后台线程 <-> UI 的消息

enum BgMsg {
    /// 自动流程已判明架构，开始下载
    Started(&'static str),
    /// 下载进度（已下载字节 / 总字节，总字节为 0 表示服务器未给 Content-Length）
    DownloadProgress { done: u64, total: u64 },
    /// 下载完成，即将弹出 UAC 授权
    UacPrompt,
    /// 安装器已启动，正在静默安装
    Installing,
    /// 复查注册表已检测到 WebView2
    Success,
    /// 流程失败（附原因）
    Failed(String),
}

/// 后台线程发消息后立刻请求 UI 重绘（egui 只在有输入/请求时重绘）。
fn notify(tx: &Sender<BgMsg>, ctx: &egui::Context, msg: BgMsg) {
    let _ = tx.send(msg);
    ctx.request_repaint();
}

// 自动安装流程（在后台线程运行）

/// 自动流程线程入口：任何 Err 都转成 Failed 消息推给 UI。
fn worker_auto(tx: Sender<BgMsg>, ctx: egui::Context) {
    if let Err(e) = auto_install_flow(&tx, &ctx) {
        notify(&tx, &ctx, BgMsg::Failed(e));
    }
}

fn auto_install_flow(tx: &Sender<BgMsg>, ctx: &egui::Context) -> Result<(), String> {
    // ① 判真实架构，选对应离线包
    let arch = native_arch();
    let url = offline_url(arch);
    notify(tx, ctx, BgMsg::Started(arch));

    // ② 流式下载到 %TEMP%（阶段一：真实百分比进度）
    // 注：不设总超时（大文件）；只设连接超时。reqwest 0.12 全系 blocking 均支持。
    let client = reqwest::blocking::Client::builder()
        .user_agent("GameReady/0.1 (WebView2-Bootstrap)")
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("初始化下载器失败：{e}"))?;

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("连接微软服务器失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("服务器返回错误：HTTP {}", resp.status()));
    }
    let total: u64 = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let dest = std::env::temp_dir().join(SETUP_TEMP_NAME);
    let mut file = std::fs::File::create(&dest)
        .map_err(|e| format!("无法创建临时文件（{}）：{e}", dest.display()))?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    let mut last_push: Option<Instant> = None; // 进度推送节流（约 120ms 一次）
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入临时文件失败：{e}"))?;
        done += n as u64;
        let due = last_push.is_none_or(|t| t.elapsed() >= Duration::from_millis(120));
        if due {
            last_push = Some(Instant::now());
            notify(tx, ctx, BgMsg::DownloadProgress { done, total });
        }
    }
    drop(file);
    if total > 0 && done < total {
        return Err("下载不完整，文件可能被安全软件拦截".to_string());
    }
    notify(tx, ctx, BgMsg::DownloadProgress { done, total });

    // ③ 预告 UAC（给用户约 1 秒阅读时间），再以 runas 静默启动安装器
    notify(tx, ctx, BgMsg::UacPrompt);
    std::thread::sleep(Duration::from_millis(900));

    let file_w: Vec<u16> = dest.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"), lpFile: PCWSTR(file_w.as_ptr()), lpParameters: w!("/silent /install"),
        nShow: SW_SHOW.0, ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|error| {
        let os_err = std::io::Error::from_raw_os_error(error.code().0 & 0xffff);
        format!("启动安装程序失败（{os_err}）。若刚才弹出了系统授权窗口并选择了【否】，可点击重试")
    })?;
    notify(tx, ctx, BgMsg::Installing);
    if !info.hProcess.is_invalid() {
        unsafe {
            let _ = WaitForSingleObject(info.hProcess, INFINITE);
            let _ = CloseHandle(info.hProcess);
        }
    }

    // ⑤ 复查注册表：安装器刚退出可能尚未写完，最多再轮询 30s
    for _ in 0..60 {
        if has_webview2() {
            let _ = std::fs::remove_file(&dest); // 清理临时安装包（失败无所谓）
            notify(tx, ctx, BgMsg::Success);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("安装程序已结束，但未检测到 WebView2，请改用手动下载".to_string())
}

// 手动下载流程（在后台线程运行）

/// 手动流程线程入口：打开官方下载页 + 每 1s 轮询注册表，装好自动继续。
fn worker_manual(tx: Sender<BgMsg>, ctx: egui::Context) {
    use std::os::windows::process::CommandExt;
    // cmd /c start 打开默认浏览器（CREATE_NO_WINDOW 避免闪一下黑色控制台窗口）
    let opened = std::process::Command::new("cmd")
        .args(["/c", "start", "", MANUAL_URL])
        .creation_flags(0x0800_0000)
        .status();
    if let Err(e) = opened {
        notify(&tx, &ctx, BgMsg::Failed(format!("无法打开浏览器：{e}")));
        return;
    }
    // 1s 轮询注册表，直到装好（用户可随时点 X 二次确认退出）
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if has_webview2() {
            notify(&tx, &ctx, BgMsg::Success);
            return;
        }
    }
}

// egui 引导小窗

/// 小窗当前阶段。
#[derive(Clone)]
enum Phase {
    /// 初始选择界面
    Ask,
    /// 自动：正在下载离线包（阶段一，真实百分比）
    Downloading,
    /// 自动：下载完成，预告即将弹出 UAC 授权
    UacPrompt,
    /// 自动：安装器运行中（阶段二，不确定进度）
    Installing,
    /// 手动：已打开浏览器，等待用户完成安装
    ManualWaiting,
    /// 安装成功（记录时间，短暂展示后关窗）
    Done(Instant),
    /// 失败（附原因），UI 提供重试 / 改用手动
    Failed(String),
}

struct GuideApp {
    tx: Sender<BgMsg>,
    rx: Receiver<BgMsg>,
    phase: Phase,
    /// 后台报告的架构名（用于下载文案）
    arch: String,
    /// 下载进度（字节）
    dl_done: u64,
    dl_total: u64,
    /// 不确定进度动画累计时间（秒）
    anim_t: f32,
    /// 是否显示退出二次确认弹窗
    ask_exit: bool,
}

impl GuideApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::setup_cjk_fonts(&cc.egui_ctx); // egui 默认字体无中文字形，必须先补
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            phase: Phase::Ask,
            arch: String::new(),
            dl_done: 0,
            dl_total: 0,
            anim_t: 0.0,
            ask_exit: false,
        }
    }

    /// 加载系统中文字体（微软雅黑 -> 黑体 -> 宋体 依次尝试），
    /// 追加到各字体族末尾作为回退：拉丁字符仍用默认字体，中文回落到系统中文字体。
    fn setup_cjk_fonts(ctx: &egui::Context) {
        let candidates = [
            r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑（Win7+，index 0 为常规体）
            r"C:\Windows\Fonts\simhei.ttf", // 黑体
            r"C:\Windows\Fonts\simsun.ttc", // 宋体
        ];
        for path in candidates {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&family) {
                    list.push("cjk".to_owned());
                }
            }
            ctx.set_fonts(fonts);
            return;
        }
        // 找不到任何中文字体时保持默认字体（界面文字将无法显示中文，但流程仍可用）
    }

    /// 点击 [自动安装（推荐）] / [重试]：切换到下载阶段并启动后台自动流程。
    fn start_auto(&mut self, ctx: &egui::Context) {
        self.phase = Phase::Downloading;
        self.arch.clear();
        self.dl_done = 0;
        self.dl_total = 0;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || worker_auto(tx, ctx2));
    }

    /// 点击 [手动下载] / [改用手动下载]：打开官方下载页并启动轮询。
    fn start_manual(&mut self, ctx: &egui::Context) {
        self.phase = Phase::ManualWaiting;
        let tx = self.tx.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || worker_manual(tx, ctx2));
    }

    /// 应用一条后台消息（只在 UI 线程调用）。
    fn apply(&mut self, msg: BgMsg) {
        match msg {
            BgMsg::Started(arch) => self.arch = arch.to_string(),
            BgMsg::DownloadProgress { done, total } => {
                self.dl_done = done;
                self.dl_total = total;
            }
            BgMsg::UacPrompt => self.phase = Phase::UacPrompt,
            BgMsg::Installing => self.phase = Phase::Installing,
            BgMsg::Success => self.phase = Phase::Done(Instant::now()),
            BgMsg::Failed(e) => self.phase = Phase::Failed(e),
        }
    }

    /// 是否需要动画/计时驱动的持续重绘。
    fn needs_animation(&self) -> bool {
        matches!(
            self.phase,
            Phase::UacPrompt | Phase::Installing | Phase::ManualWaiting | Phase::Done(_)
        ) || (matches!(self.phase, Phase::Downloading) && self.dl_total == 0)
    }

    /// 不确定进度的来回滚动值（0..=1 三角波，周期 1.6s）。
    fn sweep(&self) -> f32 {
        let cycle = 1.6f32;
        let p = (self.anim_t % cycle) / cycle;
        if p < 0.5 {
            p * 2.0
        } else {
            2.0 - p * 2.0
        }
    }

    /// 主界面（按阶段绘制）。
    fn draw_main(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.heading("需要安装 WebView2 运行环境");
        ui.add_space(8.0);

        // 先克隆阶段，避免分支内调用 &mut self 方法时与 match 的借用冲突
        let phase = self.phase.clone();
        match phase {
            Phase::Ask => {
                ui.label("本程序界面依赖微软 WebView2，当前电脑未检测到。");
                ui.label("请选择安装方式：");
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let auto =
                        ui.add_sized([168.0, 32.0], egui::Button::new("自动安装（推荐）"));
                    if auto.clicked() {
                        self.start_auto(ui.ctx());
                    }
                    let manual = ui.add_sized([120.0, 32.0], egui::Button::new("手动下载"));
                    if manual.clicked() {
                        self.start_manual(ui.ctx());
                    }
                });
                ui.add_space(10.0);
                ui.small("自动：下载对应架构的官方离线包并静默安装（约 100 MB+）");
                ui.small("手动：打开微软官方下载页，装好后自动继续");
            }
            Phase::Downloading => {
                let title = if self.arch.is_empty() {
                    "正在检测系统架构并准备下载…".to_string()
                } else {
                    format!("正在下载 {} 离线安装包…", self.arch)
                };
                ui.label(title);
                ui.add_space(14.0);
                if self.dl_total > 0 {
                    let frac = (self.dl_done as f32 / self.dl_total as f32).clamp(0.0, 1.0);
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    ui.small(format!(
                        "{:.1} / {:.1} MB",
                        self.dl_done as f64 / 1e6,
                        self.dl_total as f64 / 1e6
                    ));
                } else {
                    // 服务器未给 Content-Length：只显示已下载量 + 滚动动画
                    ui.add(egui::ProgressBar::new(self.sweep()));
                    ui.small(format!("已下载 {:.1} MB", self.dl_done as f64 / 1e6));
                }
            }
            Phase::UacPrompt => {
                ui.label("下载完成，即将启动安装程序。");
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 80),
                    "系统会弹出一次授权窗口（UAC），请点击【是】",
                );
                ui.add_space(14.0);
                ui.add(egui::ProgressBar::new(self.sweep()));
            }
            Phase::Installing => {
                ui.label("正在静默安装 WebView2，请稍候…");
                ui.add_space(6.0);
                ui.small("安装期间可能需要一两分钟，请勿关闭本窗口");
                ui.add_space(14.0);
                ui.add(egui::ProgressBar::new(self.sweep()));
            }
            Phase::ManualWaiting => {
                ui.label("已用浏览器打开微软官方下载页。");
                ui.add_space(6.0);
                ui.label("请在浏览器完成下载并运行安装程序，");
                ui.label("安装完成后这里会自动继续。");
                ui.add_space(14.0);
                ui.add(egui::ProgressBar::new(self.sweep()));
                ui.small("正在等待安装完成（每秒自动检测）…");
            }
            Phase::Done(_) => {
                ui.label("WebView2 安装成功！");
                ui.add_space(6.0);
                ui.label("正在启动本程序…");
                ui.add_space(14.0);
                ui.add(egui::ProgressBar::new(1.0));
            }
            Phase::Failed(err) => {
                ui.colored_label(egui::Color32::from_rgb(255, 120, 120), "安装未能完成");
                ui.add_space(4.0);
                ui.small(err);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("重试").clicked() {
                        self.start_auto(ui.ctx());
                    }
                    if ui.button("改用手动下载").clicked() {
                        self.start_manual(ui.ctx());
                    }
                });
            }
        }
    }

    /// 未完成安装时点 X 的二次确认弹窗。
    fn draw_exit_dialog(&mut self, ctx: &egui::Context) {
        egui::Window::new("退出确认")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label("还没有完成 WebView2 安装。");
                ui.label("没有 WebView2，本程序无法运行。确定退出吗？");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add_sized([96.0, 28.0], egui::Button::new("退出程序")).clicked() {
                        std::process::exit(0);
                    }
                    if ui.add_sized([96.0, 28.0], egui::Button::new("继续安装")).clicked() {
                        self.ask_exit = false;
                    }
                });
            });
    }
}

impl eframe::App for GuideApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 动画时间推进（每帧增量）
        self.anim_t += ctx.input(|i| i.unstable_dt);

        // ① 读取后台消息（只在此处改状态，update 本身不阻塞）
        while let Ok(msg) = self.rx.try_recv() {
            self.apply(msg);
        }

        // ② 拦截窗口关闭：未完成安装时先弹二次确认，确认退出才结束进程
        //    （eframe 0.29 机制：close_requested 后同帧回 CancelClose 可取消本次关闭）
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && !matches!(self.phase, Phase::Done(_)) {
            self.ask_exit = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        // ③ 绘制主界面
        egui::CentralPanel::default().show(ctx, |ui| self.draw_main(ui));

        // ④ 退出确认弹窗
        if self.ask_exit {
            self.draw_exit_dialog(ctx);
        }

        // ⑤ 动画/计时阶段请求持续重绘
        if self.needs_animation() {
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        // ⑥ 安装成功：短暂展示后关窗，run_native 返回、同进程进入 tauri
        if let Phase::Done(since) = self.phase {
            if since.elapsed() >= Duration::from_millis(400) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

// ============================ commands（Tauri 命令层；独立 mod 以便 generate_handler 路径引用） ============================

mod commands {
    use super::*;

#[tauri::command]
pub fn backup_get() -> Result<backup::Snapshot, backup::Error> { backup::get() }

#[tauri::command]
pub fn backup_save(config: backup::Config) -> Result<backup::Snapshot, backup::Error> { backup::save(config) }

#[tauri::command]
pub async fn backup_test(config: backup::Config) -> Result<backup::TestReport, backup::Error> { backup::test(config).await }

#[tauri::command]
pub async fn backup_sync(account: Option<String>) -> Result<backup::Snapshot, backup::Error> {
    backup::sync(true, account.as_deref()).await
}

#[tauri::command]
pub fn backup_save_password(account: String, password: String) -> Result<backup::Snapshot, backup::Error> {
    backup::save_password(&account, &password)
}

#[tauri::command]
pub fn get_status() -> AppStatus {
    let s = load_settings();
    let lol_root = s.lol_root.clone().unwrap_or_default();
    let steam_root = s.steam_root.clone().unwrap_or_default();
    let steam_running = is_steam_running();
    AppStatus {
        paths_valid: validate_roots(&lol_root, &steam_root),
        lol_guard_on: GUARD_RUNNING.load(Ordering::Relaxed),
        global_cover_on: s.global_cover_on,
        steam_running,
        current_steam_id64: if steam_running {
            current_account_sid3().and_then(|sid3| sid3_to_id64(&sid3))
        } else {
            None
        },
        steam_operation: STEAM_PROGRESS.lock().unwrap().clone(),
        lol_root,
        steam_root,
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    fn login_record(sid64: &str, name: &str, timestamp: i64) -> RawAccount {
        RawAccount { sid64: sid64.into(), fields: vec![("AccountName".into(), name.into()),
            ("Timestamp".into(), timestamp.to_string()), ("AutoLogin".into(), "1".into())] }
    }

    fn login_attempt_fixture() -> LoginAttempt {
        LoginAttempt { generation: 7, root: "fixture-steam".into(), account: "player".into(),
            password: Some("fixture-password".into()), sid64: Some("76561197960265729".into()),
            baseline: 100, history: vec![("76561197960265729".into(), 100), ("76561197960265730".into(), 80)],
              log_offset: 0, log_since: 0, include_global: true, cover: None, restoring: false, started: Instant::now() }
    }

    #[test]
    fn new_session_without_login_history_can_be_confirmed_retained_and_configured() {
        use chrono::TimeZone;
        let dir = std::env::temp_dir().join(format!("gameready-session-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("logs")).unwrap();
        let root = dir.to_str().unwrap();
        let config = r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "Accounts" {
            "player" { "SteamID" "76561197960265729" }
            "other" { "SteamID" "76561197960265730" }
        } } } } }"#;
        fs::write(dir.join("config/config.vdf"), config).unwrap();
        let mut old = login_record("76561197960265730", "other", 80);
        old.set("Unrelated", "keep");
        write_vdf(root, &[old]).unwrap();
        let history = login_history(root).unwrap();
        let account = session_account(root, "76561197960265729", &history).unwrap().unwrap();
        assert_eq!(account.account_name, "player");
        assert!(session_account(root, "76561197960265731", &history).unwrap().is_none());
        let mut attempt = login_attempt_fixture();
        attempt.root = root.into(); attempt.sid64 = None;
        attempt.log_since = chrono::Local.with_ymd_and_hms(2026, 10, 17, 18, 45, 0).single().unwrap().timestamp();
        let old_line = "[2026-10-17 18:44:59] [Logging On, 4, 7] [U:1:1] RecvMsgClientLogOnResponse() : [U:1:1] 'OK'\n";
        fs::write(connection_log(root), old_line).unwrap();
        attempt.log_offset = old_line.len() as u64;
        assert!(attempt.observed_connection(account.clone(), read_login_success(root, "1", attempt.log_offset, attempt.log_since)).is_none());
        let lines = "[2026-10-17 18:45:01] [Logging On, 4, 7] [U:1:2] RecvMsgClientLogOnResponse() : [U:1:2] 'OK'\n\
            [2026-10-17 18:45:01] [Logging On, 4, 7] [U:1:1] RecvMsgClientLogOnResponse() : [U:1:1] 'InvalidPassword'\n";
        OpenOptions::new().append(true).open(connection_log(root)).unwrap().write_all(lines.as_bytes()).unwrap();
        assert!(read_login_success(root, "1", attempt.log_offset, attempt.log_since).is_none());
        let success = "[2026-10-17 18:45:02] [Logging On, 4, 7] [U:1:1] RecvMsgClientLogOnResponse() : [U:1:1] 'OK'\n";
        OpenOptions::new().append(true).open(connection_log(root)).unwrap().write_all(success.as_bytes()).unwrap();
        let timestamp = read_login_success(root, "1", attempt.log_offset, attempt.log_since);
        assert!(attempt.observed_connection(session_account(root, "76561197960265730", &history).unwrap().unwrap(), timestamp).is_none());
        let confirmed = attempt.observed_connection(account, timestamp).unwrap();
        let retained = retain_session_account(root, &confirmed).unwrap();
        assert_eq!(retained.len(), 2);
        assert_eq!(field(&retained[0], "Unrelated"), Some("keep"));
        assert_eq!(field(&retained[1], "RememberPassword"), Some("1"));
        assert_eq!(retain_session_account(root, &confirmed).unwrap().len(), 2);
        let results = write_all(&dir, PackScope::SteamAccount, Some("1"));
        assert_eq!(results.len(), 2); assert!(results.iter().all(|r| r.ok));
        let local = fs::read_to_string(dir.join("userdata/1/config/localconfig.vdf")).unwrap();
        assert!(local.contains("\"NotifyAvailableGames\"\t\t\"0\""));
        let roaming = fs::read_to_string(dir.join("userdata/1/7/remote/sharedconfig.vdf")).unwrap();
        assert!(roaming.contains("\"SteamDefaultDialog\"\t\t\"#app_games\""));
        // 恢复阶段必须读取重新启动之后的新成功，旧阶段的 OK 不能复用。
        attempt.log_offset = fs::metadata(connection_log(root)).unwrap().len();
        attempt.sid64 = Some(confirmed.steam_id64.clone());
        assert!(read_login_success(root, "1", attempt.log_offset, attempt.log_since).is_none());
        fs::write(connection_log(root), old_line).unwrap();
        assert!(read_login_success(root, "1", attempt.log_offset, attempt.log_since).is_none());
        fs::write(connection_log(root), success).unwrap();
        assert!(read_login_success(root, "1", attempt.log_offset, attempt.log_since).is_some());
        fs::write(vdf_path(root), b"{broken").unwrap();
        assert!(retain_session_account(root, &confirmed).is_err());
        assert_eq!(fs::read(vdf_path(root)).unwrap(), b"{broken");
        let resolved = fs::canonicalize(&dir).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn session_mapping_rejects_duplicate_ids_and_malformed_configs() {
        let duplicate = r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "Accounts" {
            "player" { "SteamID" "76561197960265729" } "other" { "SteamID" "76561197960265729" }
        } } } } }"#;
        assert!(vdf::account_for_id(duplicate, "76561197960265729").is_err());
        assert!(vdf::account_for_id("{broken", "76561197960265729").is_err());
        assert!(vdf::account_for_id("\"InstallConfigStore\" {}", "76561197960265729").unwrap().is_none());
    }

    #[test]
    #[ignore = "只读核验真实 Steam 会话，需要 GAMEREADY_STEAM_TEST_ROOT"]
    fn live_steam_session_read_only_diagnostic() {
        let root = std::env::var("GAMEREADY_STEAM_TEST_ROOT").unwrap();
        let sid3 = active_steam_sid3(&root).expect("Steam 活动进程不可用");
        let sid64 = sid3_to_id64(&sid3).unwrap();
        let history = login_history(&root).unwrap();
        let account = session_account(&root, &sid64, &history).unwrap().expect("SteamID 账号映射不可用");
        assert!(!account.account_name.is_empty());
        assert!(read_login_success(&root, &sid3, 0, 0).is_some());
        let accounts = accounts_for_root(&root).unwrap();
        assert_eq!(accounts.iter().filter(|a| a.steam_id64 == sid64).count(), 1);
        println!("Live Steam identified; history contains active account: {}; displayed accounts: {}",
            history.iter().any(|a| a.sid64 == sid64), accounts.len());
    }

    #[test]
    fn login_target_matches_history_and_rejects_missing_duplicate_or_invalid_records() {
        let sid = "76561197960265729";
        let records = vec![login_record(sid, "Player", 100)];
        assert_eq!(find_login_target(&records, None, "PLAYER").unwrap().unwrap().steam_id64, sid);
        assert_eq!(find_login_target(&records, Some(sid), "ignored").unwrap().unwrap().account_name, "Player");
        assert!(find_login_target(&records, None, "new_player").unwrap().is_none());
        assert!(find_login_target(&records, Some("76561197960265730"), "Player").is_err());
        assert!(find_login_target(&[login_record(sid, "Player", 100), login_record(sid, "Player", 101)], Some(sid), "").is_err());
        assert!(find_login_target(&[login_record(sid, "Player", 100), login_record("76561197960265730", "PLAYER", 101)], None, "player").is_err());
        assert!(find_login_target(&[login_record(sid, "", 100)], Some(sid), "").is_err());
        for invalid in ["invalid".to_string(), (SID64_BASE - 1).to_string(), SID64_BASE.to_string(),
            (SID64_BASE + u64::from(u32::MAX) + 1).to_string()] {
            assert!(find_login_target(&[login_record(&invalid, "player", 100)], Some(&invalid), "").is_err(),
                "Invalid individual Steam ID accepted: {invalid}");
        }
        let highest = (SID64_BASE + u64::from(u32::MAX)).to_string();
        assert!(find_login_target(&[login_record(&highest, "player", 100)], Some(&highest), "").unwrap().is_some());
    }

    #[test]
    fn login_observation_ignores_old_history_and_preselection_until_target_is_active() {
        let attempt = login_attempt_fixture();
        let sid = attempt.sid64.as_deref().unwrap();
        let mut accounts = vec![to_account(&login_record(sid, "PLAYER", 100)),
            to_account(&login_record("76561197960265730", "other", 80))];
        for active in [None, Some(sid), Some("76561197960265730")] {
            assert!(attempt.observed(&accounts, active).unwrap().is_none());
        }
        accounts[0].timestamp = 101;
        accounts[0].auto_login = false; // ActiveUser, not our AutoLogin preselection, proves the target.
        assert!(attempt.observed(&accounts, None).unwrap().is_none());
        assert!(attempt.observed(&accounts, Some("76561197960265730")).unwrap().is_none());
        assert_eq!(attempt.observed(&accounts, Some(sid)).unwrap().unwrap().steam_id64, sid);
    }

    #[test]
    fn other_accounts_new_login_cancels_even_after_switching_back_to_target() {
        let attempt = login_attempt_fixture();
        let sid = attempt.sid64.as_deref().unwrap();
        let mut accounts = vec![to_account(&login_record(sid, "player", 101)),
            to_account(&login_record("76561197960265730", "other", 81))];
        assert!(attempt.observed(&accounts, Some("76561197960265730")).is_err());
        assert!(attempt.observed(&accounts, Some(sid)).is_err());
        accounts[1] = to_account(&login_record("76561197960265731", "new_other", 1));
        assert!(attempt.observed(&accounts, Some(sid)).is_err()); // Other account absent from the stopped snapshot.
    }

    #[test]
    fn first_login_discovers_sid_but_restore_requires_that_sid_and_a_new_confirmation() {
        let sid = "76561197960265729";
        let mut attempt = login_attempt_fixture();
        attempt.sid64 = None;
        attempt.baseline = 0;
        attempt.history.retain(|(record_sid, _)| record_sid != sid);
        let mut account = to_account(&login_record(sid, "Player", 100));
        let confirmed = attempt.observed(std::slice::from_ref(&account), Some(sid)).unwrap().unwrap();
        attempt.sid64 = Some(confirmed.steam_id64.clone());
        attempt.baseline = confirmed.timestamp;
        attempt.history.push((confirmed.steam_id64, confirmed.timestamp));
        attempt.restoring = true;
        assert!(attempt.observed(std::slice::from_ref(&account), Some(sid)).unwrap().is_none());
        let duplicate_name = to_account(&login_record("76561197960265731", "PLAYER", 101));
        assert!(attempt.observed(std::slice::from_ref(&duplicate_name), Some(&duplicate_name.steam_id64)).unwrap().is_none());
        account.timestamp = 101;
        assert!(attempt.observed(std::slice::from_ref(&account), None).unwrap().is_none());
        assert_eq!(attempt.observed(&[account], Some(sid)).unwrap().unwrap().steam_id64, sid);
    }

    #[test]
    fn prepared_configuration_is_not_reported_as_login_success_while_waiting() {
        let mut attempt = login_attempt_fixture();
        attempt.cover = Some(CoverReport { total: 2, ok: 2, failed: Vec::new(), skipped_reason: None, auto_restarted: false });
        for restoring in [false, true] {
            attempt.restoring = restoring;
            let pending = attempt.pending();
            assert!(pending.pending && !pending.ok);
            assert_eq!(pending.generation, attempt.generation);
            assert_eq!(pending.name, attempt.account);
            assert!(!pending.message.contains(attempt.password.as_deref().unwrap()));
            assert_eq!(pending.message.contains("恢复登录"), restoring);
        }
    }

    #[test]
    fn login_result_reports_cover_success_partial_failure_and_start_failure() {
        let mut report = CoverReport { total: 2, ok: 2, failed: Vec::new(), skipped_reason: None, auto_restarted: true };
        let complete = LoginResult::from_cover("player".into(), Ok(report.clone()));
        assert!(complete.ok && complete.message.contains("配置覆盖完成（2/2）"));
        report.ok = 1;
        report.failed.push(CoverResult { rel: "localconfig.vdf".into(), ok: false, err: Some("access denied".into()) });
        let partial = LoginResult::from_cover("player".into(), Ok(report));
        assert!(!partial.ok && partial.message.contains("已登录 player，配置覆盖未完成（1/2"));
        let failed = LoginResult::from_cover("player".into(), Err("当前账号已变化".into()));
        assert!(!failed.ok && failed.message.contains("当前账号已变化"));
    }

    #[test]
    fn final_login_feedback_includes_nas_state_without_changing_steam_outcome() {
        let report = CoverReport { total: 2, ok: 2, failed: Vec::new(), skipped_reason: None, auto_restarted: false };
        let completed = LoginResult::from_cover("player".into(), Ok(report));
        for note in ["NAS 已同步", "NAS 正在同步", "NAS 已排队", "NAS 同步失败：认证失败", "本地无密码，未自动同步 NAS"] {
            let result = completed.clone().with_backup_note(Some(note.into()));
            assert!(result.ok && !result.pending, "NAS state must not turn a completed Steam login into a failure");
            assert_eq!(result.name, completed.name);
            assert!(result.message.starts_with(&completed.message));
            assert!(result.message.ends_with(note));
        }
        assert_eq!(completed.clone().with_backup_note(None).message, completed.message);
        let failed_cover = LoginResult::from_cover("player".into(), Err("账号配置写入失败".into()))
            .with_backup_note(Some("NAS 已同步".into()));
        assert!(!failed_cover.ok && !failed_cover.pending);
        assert!(failed_cover.message.contains("账号配置写入失败") && failed_cover.message.ends_with("NAS 已同步"));
    }

    #[test]
    fn automatic_cover_rejects_a_different_account_before_writing() {
        assert!(validate_cover_account("1", Some("76561197960265729")).is_ok());
        assert!(validate_cover_account("2", Some("76561197960265729")).is_err());
        assert!(validate_cover_account("invalid", Some("76561197960265729")).is_err());
        assert!(validate_cover_account("2", None).is_ok()); // 手动覆盖使用当前账号
    }

    #[test]
    fn late_login_is_cancelled_by_new_operations_or_replaced_passwords() {
        assert!(pending_login_matches(7, 7, Some("saved"), Some("saved")));
        assert!(!pending_login_matches(7, 8, Some("saved"), Some("saved")));
        assert!(!pending_login_matches(7, 7, Some("saved"), Some("changed")));
        assert!(!pending_login_matches(7, 7, Some("saved"), None));
        assert!(pending_login_matches(7, 7, None, None)); // Steam 缓存登录可自动覆盖
        assert!(pending_login_matches(7, 7, None, Some("unverified"))); // 但不验证或同步新补的密码
        assert!(!pending_login_matches(7, 8, None, None));
    }

    #[test]
    fn embedded_steam_templates_enable_overlay_and_disable_restore_tabs_without_losing_other_settings() {
        for pack in FILES.iter().filter(|p| p.scope != PackScope::Lol && !p.rel.ends_with("sharedconfig.vdf")) {
            // 同时覆盖旧版账号根名修复和全局配置；额外字段不能被模板清掉。
            let original = br#""InstallConfigStore" {
                "system" { "EnableGameOverlay" "0" "InGameOverlayRestoreBrowserTabs" "1" "Unrelated" "keep" }
                "news" { "NotifyAvailableGames" "1" "OtherNewsSetting" "keep" }
                "Software" { "Valve" { "Steam" { "OtherPreference" "keep" } } }
            }"#;
            let merged = vdf::merge(original, pack.bytes).unwrap();
            let text = std::str::from_utf8(&merged).unwrap();
            let root = if pack.scope == PackScope::SteamAccount { "UserLocalConfigStore" } else { "InstallConfigStore" };
            assert!(text.starts_with(&format!("\"{root}\"\n")));
            for (key, value, opposite) in [("EnableGameOverlay", "1", "0"), ("InGameOverlayRestoreBrowserTabs", "0", "1")] {
                assert_eq!(text.matches(&format!("\"{key}\"\t\t\"{value}\"")).count(), 1);
                assert!(!text.contains(&format!("\"{key}\"\t\t\"{opposite}\"")));
            }
            assert!(text.contains("\"Unrelated\"\t\t\"keep\""));
            assert!(text.contains("\"OtherPreference\"\t\t\"keep\""));
            assert!(text.contains("\"NotifyAvailableGames\"\t\t\"0\""));
            assert!(!text.contains("\"NotifyAvailableGames\"\t\t\"1\""));
            assert!(text.contains("\"OtherNewsSetting\"\t\t\"keep\""));
            assert_eq!(vdf::merge(&merged, pack.bytes).unwrap(), merged);
        }
    }

    #[test]
    fn roaming_template_selects_library_and_never_toasts_preserving_other_settings() {
        let pack = FILES.iter().find(|p| p.rel.ends_with("sharedconfig.vdf")).unwrap();
        assert_eq!(pack.scope, PackScope::SteamAccount);
        let original = br##""UserRoamingConfigStore" {
            "DisableAllToasts" "0" "DisableToastsInGame" "0" "OtherSetting" "keep"
            "Software" { "Valve" { "Steam" { "SteamDefaultDialog" "#app_store" "OtherPreference" "keep" } } }
        }"##;
        let merged = vdf::merge(original, pack.bytes).unwrap();
        let text = std::str::from_utf8(&merged).unwrap();
        for (key, value) in [("DisableAllToasts", "1"), ("DisableToastsInGame", "1"), ("SteamDefaultDialog", "#app_games"),
            ("OtherSetting", "keep"), ("OtherPreference", "keep")] {
            assert!(text.contains(&format!("\"{key}\"\t\t\"{value}\"")));
        }
        assert!(!text.contains("#app_store"));
        assert_eq!(vdf::merge(&merged, pack.bytes).unwrap(), merged);
    }

    #[test]
    fn unchanged_steam_templates_preserve_bom_comments_and_crlf() {
        for pack in FILES.iter().filter(|p| p.scope != PackScope::Lol) {
            let template = std::str::from_utf8(pack.bytes).unwrap().replace("\r\n", "\n").replace('\n', "\r\n");
            let original = format!("\u{feff}// keep this comment\r\n{template}");
            assert_eq!(vdf::merge(original.as_bytes(), pack.bytes).unwrap(), original.as_bytes());
        }
    }

    #[test]
    fn legacy_account_root_is_written_even_when_all_preferences_match() {
        let pack = FILES.iter().find(|p| p.scope == PackScope::SteamAccount).unwrap();
        let original = std::str::from_utf8(pack.bytes).unwrap().replacen("UserLocalConfigStore", "InstallConfigStore", 1);
        let dir = std::env::temp_dir().join(format!("gameready-steam-root-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join(pack.rel.replace("{SID3}", "1"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, original.as_bytes()).unwrap();
        let report = write_all(&dir, PackScope::SteamAccount, Some("1"));
        assert_eq!(report.len(), 2);
        assert!(report.iter().all(|r| r.ok && r.err.is_none()));
        let updated = fs::read(&path).unwrap();
        assert_ne!(updated, original.as_bytes());
        assert!(std::str::from_utf8(&updated).unwrap().starts_with("\"UserLocalConfigStore\""));
        assert_eq!(vdf::merge(&updated, pack.bytes).unwrap(), updated);
        let resolved = fs::canonicalize(&dir).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
        assert!(resolved.file_name().unwrap().to_string_lossy().starts_with("gameready-steam-root-test-"));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn steam_noop_preserves_timestamp_and_changed_locked_files_report_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let dir = std::env::temp_dir().join(format!("gameready-steam-noop-test-{}", uuid::Uuid::new_v4()));
        for pack in FILES.iter().filter(|p| p.scope != PackScope::Lol) {
            let path = dir.join(pack.rel.replace("{SID3}", "1"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            for changed in [false, true] {
                let template = std::str::from_utf8(pack.bytes).unwrap();
                let key = if pack.rel.ends_with("sharedconfig.vdf") { "DisableAllToasts" } else { "EnableGameOverlay" };
                let original = if changed { template.replacen(&format!("\"{key}\"\t\t\"1\""), &format!("\"{key}\"\t\t\"0\""), 1) }
                    else { template.to_string() };
                assert_eq!(original != template, changed);
                fs::write(&path, original.as_bytes()).unwrap();
                let modified = std::time::UNIX_EPOCH + Duration::from_secs(1_234_567_890);
                File::options().write(true).open(&path).unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
                let before = fs::metadata(&path).unwrap().modified().unwrap();
                let guard = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(&path).unwrap();
                let report = write_all(&dir, pack.scope, Some("1"));
                let result = report.iter().find(|r| r.rel == pack.rel.replace("{SID3}", "1")).unwrap();
                assert_eq!(result.ok, !changed);
                if changed { assert!(result.err.as_deref().unwrap().contains("写入失败")); }
                else { assert!(result.err.is_none()); }
                assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
                assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
                assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
                drop(guard);
            }
        }
        let resolved = fs::canonicalize(&dir).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
        assert!(resolved.file_name().unwrap().to_string_lossy().starts_with("gameready-steam-noop-test-"));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn overlay_scaling_is_enabled_in_registry_and_failures_are_reported() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = format!(r"Software\GameReady-Test-{}", uuid::Uuid::new_v4());
        let (fixture, _) = hkcu.create_subkey(&path).unwrap();
        let missing = enable_overlay_scaling(&fixture);
        assert!(!missing.ok && missing.err.is_some());
        let (steam, _) = fixture.create_subkey(r"Software\Valve\Steam").unwrap();
        steam.set_value("OverlayScaleInterface", &0u32).unwrap();
        steam.set_value("Unrelated", &"keep").unwrap();
        for _ in 0..2 {
            let result = enable_overlay_scaling(&fixture);
            assert!(result.ok && result.err.is_none());
            assert_eq!(steam.get_value::<u32, _>("OverlayScaleInterface").unwrap(), 1);
            assert_eq!(steam.get_value::<String, _>("Unrelated").unwrap(), "keep");
        }
        drop(steam); drop(fixture);
        hkcu.delete_subkey_all(path).unwrap();
    }

    #[test]
    fn failed_atomic_replace_keeps_original_and_cleans_temporary_files() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let dir = std::env::temp_dir().join(format!("gameready-write-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("preserved.txt");
        fs::write(&path, b"original").unwrap();
        let guard = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(&path).unwrap();
        for durable in [false, true] {
            assert!(atomic_write(&path, b"replacement", durable).is_err());
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        }
        drop(guard);
        atomic_write(&path, b"complete", false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"complete");
        fs::remove_file(path).unwrap(); fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn embedded_lol_templates_keep_paths_contents_and_all_timestamps() {
        use chrono::TimeZone;
        let dir = std::env::temp_dir().join(format!("gameready-packs-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let result = write_all(&dir, PackScope::Lol, None);
        assert_eq!(result.len(), 5);
        assert!(result.iter().all(|r| r.ok && r.err.is_none()));
        let expected = chrono::Local.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).single().unwrap().timestamp();
        for pack in FILES.iter().filter(|p| p.scope == PackScope::Lol) {
            let path = dir.join(pack.rel);
            let meta = fs::metadata(&path).unwrap();
            for stamp in [meta.created().unwrap(), meta.modified().unwrap(), meta.accessed().unwrap()] {
                assert_eq!(stamp.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(), expected as u64);
            }
            assert_eq!(fs::read(&path).unwrap(), pack.bytes);
        }
        let resolved = fs::canonicalize(&dir).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
        assert!(resolved.file_name().unwrap().to_string_lossy().starts_with("gameready-packs-test-"));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn local_duplicates_are_replaced_with_latest_password() {
        let mut creds = std::collections::BTreeMap::from([
            ("Player".into(), "old".into()), ("PLAYER".into(), "older".into()), ("other".into(), "keep".into())]);
        replace_credential(&mut creds, "player", " latest password ");
        assert_eq!(creds.len(), 2);
        assert_eq!(creds["player"], " latest password ");
        assert_eq!(creds["other"], "keep");
    }

    #[test]
    fn atomic_storage_round_trip_and_corrupt_credentials() {
        let dir = std::env::temp_dir().join(format!("gameready-store-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("accounts.json");
        let mut creds = std::collections::BTreeMap::new();
        replace_credential(&mut creds, "player", "one");
        atomic_json(&path, &creds).unwrap();
        replace_credential(&mut creds, "PLAYER", "two");
        atomic_json(&path, &creds).unwrap();
        assert_eq!(read_creds(&path).unwrap()["player"], "two");
        fs::write(&path, b"{broken").unwrap();
        assert!(read_creds(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(path).unwrap(); fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn older_settings_have_usable_defaults_and_vdf_preserves_fields() {
        let settings: Settings = serde_json::from_str(r#"{"global_cover_on":true}"#).unwrap();
        assert!(settings.global_cover_on); assert_eq!(settings.close_behavior, "tray");
        assert_eq!(settings.accent, "#00e5ff");
        let text = "\u{feff}\"users\"\r\n{\r\n\"76561197960265729\"\r\n{\r\n\"AccountName\" \"fixture\"\r\n\"Timestamp\" \"100\"\r\n\"FutureField\" \"keep\"\r\n}\r\n}\r\n";
        let accounts = vdf::parse_users(text).unwrap();
        assert_eq!(accounts.len(), 1); assert_eq!(field(&accounts[0], "Timestamp"), Some("100"));
        let reparsed = vdf::parse_users(&vdf::render_users(&accounts)).unwrap();
        assert_eq!(field(&reparsed[0], "FutureField"), Some("keep"));
        assert_eq!(sid64_to_sid3(&accounts[0].sid64).as_deref(), Some("1"));
        assert!(sid64_to_sid3("invalid").is_none());
    }

    #[test]
    fn login_confirmation_requires_new_timestamp_and_active_matching_account() {
        let mut account = SteamAccount { steam_id64: "76561197960265729".into(), account_name: "player".into(),
            persona_name: String::new(), auto_login: true, timestamp: 100 };
        assert!(!login_observed(&account, Some(&account.steam_id64), "player", 100));
        account.timestamp = 101;
        assert!(login_observed(&account, Some(&account.steam_id64), "PLAYER", 100));
        assert!(!login_observed(&account, None, "player", 100));
        assert!(!login_observed(&account, Some("76561197960265730"), "player", 100));
        assert!(!login_observed(&account, None, "player", 100));
        assert!(!login_observed(&account, Some(&account.steam_id64), "other", 100));
    }
}

#[tauri::command]
pub fn set_paths(lol_root: String, steam_root: String) -> Result<AppStatus, String> {
    let _steam_op = STEAM_OP.try_lock().map_err(|_| "Steam 正在应用操作，请稍候再修改路径")?;
    let _settings = SETTINGS_OP.lock().unwrap();
    let lol = lol_root.trim().to_string();
    let steam = steam_root.trim().to_string();
    let valid = validate_roots(&lol, &steam);
    if !valid.lol && !valid.steam {
        return Err(format!(
            "两个路径下均未找到 LeagueClient.exe / steam.exe（LOL：{lol}，Steam：{steam}），请重新选择"
        ));
    }
    let mut s = checked_settings()?;
    // 只保存各自校验有效的项：无效路径不落盘（避免重启后路径条指向无效目录、
    // 覆盖/守护在错误根目录下 create_dir_all 长出垃圾文件树）
    if !lol.is_empty() && valid.lol {
        s.lol_root = Some(lol);
    }
    let root_changed = !steam.is_empty() && valid.steam && s.steam_root.as_deref() != Some(steam.as_str());
    if !steam.is_empty() && valid.steam { s.steam_root = Some(steam); }
    save_settings(&s)?;
    if root_changed {
        let generation = STEAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        *STEAM_PROGRESS.lock().unwrap() = Some(SteamProgress { generation, phase: "idle".into(),
            message: "Steam 路径已更新，上次登录确认已取消".into(), sid64: None });
    }
    let mut status = get_status();
    status.paths_valid = valid;
    Ok(status)
}

#[tauri::command]
pub fn get_settings() -> Result<Settings, String> {
    checked_settings()
}

#[tauri::command(rename_all = "snake_case")]
pub fn set_settings(mut settings: Settings, app: tauri::AppHandle) -> Result<(), String> {
    let _settings = SETTINGS_OP.lock().unwrap();
    let cur = checked_settings()?;
    if !matches!(settings.close_behavior.as_str(), "tray" | "exit") {
        return Err("关闭行为必须为 tray 或 exit".into());
    }
    if settings.accent.len() != 7 || !settings.accent.starts_with('#') || !settings.accent[1..].bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("强调色必须为 #RRGGBB".into());
    }
    settings.lol_root = cur.lol_root.clone();
    settings.steam_root = cur.steam_root.clone();

    // 开机自启：同步到系统启动项
    #[cfg(target_os = "windows")]
    {
        use tauri_plugin_autostart::ManagerExt;
        let auto = app.autolaunch();   // 插件 v2 API 名为 autolaunch（v3 计划改名 autostart）
        let want = settings.autostart;
        let is_enabled = auto.is_enabled().unwrap_or(false);
        let r = if want && !is_enabled {
            auto.enable()
        } else if !want && is_enabled {
            auto.disable()
        } else {
            Ok(())
        };
        if let Err(e) = r {
            return Err(format!("设置开机自启动失败：{e}"));
        }
    }
    if let Err(e) = save_settings(&settings) {
        use tauri_plugin_autostart::ManagerExt;
        let auto = app.autolaunch();
        let _ = if cur.autostart { auto.enable() } else { auto.disable() };
        return Err(e);
    }
    Ok(())
}

#[tauri::command]
pub fn open_data_dir() -> Result<(), String> {
    let dir = data_dir();
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开数据目录失败：{e}"))
}

#[tauri::command(rename_all = "snake_case")]
pub fn list_logs(filter: String) -> Vec<LogEntry> {
    let mut out: Vec<LogEntry> = Vec::new();
    if let Ok(f) = File::open(logs_path()) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            // 单行损坏（如断电截断）跳过，不影响其余条目
            if let Ok(e) = serde_json::from_str::<LogEntry>(&line) {
                if filter == "all" || filter.is_empty() || e.kind == filter {
                    out.push(e);
                }
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.ts_ms));
    out
}

#[tauri::command]
pub async fn steam_cover(include_global: bool, app: tauri::AppHandle) -> Result<CoverReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _steam_op = STEAM_OP.try_lock().map_err(|_| "Steam 正在应用操作，请稍候重试")?;
        let generation = STEAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        let result = cover_locked(include_global, app.clone(), generation);
        steam_progress(&app, generation, "idle", "", None);
        result
    })
    .await
    .map_err(|e| format!("后台任务失败: {e}"))?
}

#[tauri::command]
pub fn start(app: tauri::AppHandle) -> Result<(), String> {
    let cell = GUARD.get_or_init(|| Mutex::new(None));
    let mut g = cell.lock().unwrap();
    if g.is_some() {
        return Ok(()); // 幂等：已在跑直接成功
    }

    let settings = load_settings();
    // 路径有效性前置校验：防止守护在无效根目录下 create_dir_all 长出垃圾文件树
    if let Some(lr) = settings.lol_root.as_deref() {
        if !lr.trim().is_empty() && !validate_roots(lr, "").lol {
            return Err("LOL 根目录无效（未找到 LeagueClient.exe），请先在路径设置中修正".to_string());
        }
    }
    let lol_root = settings
        .lol_root
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "未配置 LOL 根目录，请先在路径设置中指定".to_string())?;

    // 启动日志
    log(
        "lol",
        "LOL 守护启动",
        &format!("开始每 200ms 循环覆盖内置配置到 {}", lol_root),
        true,
    );

    let stop_flag = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let start_ms = now_ms();

    // 后台守护线程（AppHandle 可跨线程：tauri 2 AppHandle 为 Send + Sync）
    let thread = {
        let lol_root = lol_root.clone();
        let stop_flag2 = stop_flag.clone();
        let count2 = count.clone();
        let app2 = app.clone();
        std::thread::spawn(move || run_loop(app2, lol_root, start_ms, stop_flag2, count2))
    };

    *g = Some(GuardHandle { stop_flag, thread, start_ms, count });
    // 同步全局守护开关（AppStatus.lol_guard_on 的数据源，前端 get_status 依赖）
    set_guard_running(true);
    Ok(())
}

#[tauri::command]
pub fn stop(app: tauri::AppHandle) -> Result<GuardSummary, String> {
    let cell = GUARD.get_or_init(|| Mutex::new(None));
    let handle = {
        let mut g = cell.lock().unwrap();
        match g.take() {
            Some(h) => {
                h.stop_flag.store(true, Ordering::SeqCst);
                set_guard_running(false);
                h
            },
            None => return Err("LOL 守护未在运行".to_string()),
        }
    }; // 锁在此释放：线程自停时要拿同一把锁清理，join 前绝不能持锁（防死锁）

    let _ = handle.thread.join();

    let summary = GuardSummary {
        count: handle.count.load(Ordering::SeqCst),
        elapsed_ms: now_ms().saturating_sub(handle.start_ms),
    };

    // 结束日志（kind="lol"，含轮次/时长/结果）
    log(
        "lol",
        "LOL 守护停止",
        &format!(
            "累计覆盖 {} 轮，累计运行 {}。",
            summary.count,
            fmt_duration(summary.elapsed_ms)
        ),
        true,
    );

    // 推送最终统计，前端定格显示
    let _ = app.emit("guard-stats", summary.clone());

    Ok(summary)
}

/// 账号列表（前端账号模态渲染）
#[tauri::command]
pub fn accounts_list() -> Result<Vec<SteamAccount>, String> {
    match steam_root_or_err() { Ok(root) => accounts_for_root(&root), Err(_) => Ok(Vec::new()) }
}

/// 登录准备在后台线程执行；验证码和服务器等待由可取消的确认线程处理。
#[tauri::command]
pub async fn switch_account(sid64: String, app: AppHandle) -> Result<LoginResult, String> {
    tauri::async_runtime::spawn_blocking(move || start_login(Some(sid64), String::new(), None, app))
        .await.map_err(|e| format!("后台任务失败: {e}"))?
}

#[tauri::command]
pub async fn delete_account(sid64: String, app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _steam_op = STEAM_OP.try_lock().map_err(|_| "Steam 正在应用操作，请稍候重试")?;
        let generation = STEAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        steam_progress(&app, generation, "stopping", "正在清理本机 Steam 账号", Some(&sid64));
        let result = delete_locked(&sid64);
        steam_progress(&app, generation, "idle", "", None);
        match &result {
            Ok(name) => log("acct", "删除 Steam 账号", &format!("已删除 {name}"), true),
            Err(error) => log("acct", "删除 Steam 账号未完成", error, false),
        }
        let _ = app.emit("steam-accounts-changed", ());
        result
    }).await.map_err(|e| format!("后台任务失败: {e}"))?
}

#[tauri::command]
pub async fn login_new(account: String, password: String, app: AppHandle) -> Result<LoginResult, String> {
    tauri::async_runtime::spawn_blocking(move || start_login(None, account, Some(password), app))
        .await.map_err(|e| format!("后台任务失败: {e}"))?
}
}
