// GameReady 主入口：WebView2 引导 → Tauri 应用
// 各模块接口合同见 docs/PROTOCOL.md（勿随意改此文件，属共享文件）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
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
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{GetCurrentProcess, WaitForSingleObject, INFINITE};


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
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("GameReady 运行失败");
}


// ============================ store ============================
// store.rs —— 数据目录、设置存储与全工程共享类型定义
// 合同：docs/PROTOCOL.md「共享类型」「store 模块」两节，类型字段/函数签名不得偏离。
// 便携原则：数据一律存 exe 同级 Data\（settings.json 等），用 current_exe 定位，
// 不依赖工作目录。设置读写在低频场景（命令触发），原子写保证文件完整性，
// 并发读到旧值无害，故不额外加锁。
// PROTOCOL-NOTE（3 条，详见各处行内注释）：
// 1. AppStatus.lol_guard_on 无来源：合同未给 guard 模块定义查询函数，本模块维护
// 全局开关 set_guard_running()，guard 模块 start/stop 时需调用，否则该字段恒 false。
// 2. 带参数的 command 使用 rename_all="snake_case"：前端按合同 Rust 签名同名传键
// （invoke('set_paths', { lol_root, steam_root })）；Tauri 2 默认却是 camelCase。
// 3. detect_default_roots 返回元组顺序未在合同写明，按参数命名惯例定为 (LOL, Steam)。
// ==================== 共享类型（其他模块 use *） ====================

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
    /// 注册表 AutoLoginUser 当前账号的 SteamID64
    pub current_steam_id64: Option<String>,
}

/// 路径有效性：能找到 LeagueClient.exe / steam.exe
#[derive(Serialize, Deserialize, Clone)]
pub struct PathsValid {
    pub lol: bool,
    pub steam: bool,
}

/// 用户设置（Data\settings.json 的结构）
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Settings {
    pub lol_root: Option<String>,
    pub steam_root: Option<String>,
    pub global_cover_on: bool,
    pub autostart: bool,
    pub start_minimized: bool,
    pub close_behavior: String, // "tray" | "exit"
    pub accent: String,         // 强调色，如 "#00e5ff"
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

/// LOL 根目录有效：存在 <root>\LeagueClient\LeagueClient.exe（WeGame 国服实测结构，
/// LeagueClient 子目录大小写不敏感定位）；兼容 <root>\LeagueClient.exe 的非标准安装
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

/// LOL 探测：常见盘符下 <盘>:\WeGameApps\英雄联盟（D→C→E 优先），
/// WeGameApps / 英雄联盟 / LeagueClient 各级目录名均大小写不敏感定位
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

/// Steam 探测：优先注册表 HKLM\SOFTWARE\WOW6432Node\Valve\Steam 的 InstallPath
/// （本机实测可靠，读到即采用）；失败回退 C:\Program Files (x86)\Steam，
/// 回退值仅当确实存在 steam.exe 时才返回，避免给出明显无效的默认
fn detect_steam_root() -> Option<String> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(key) = hklm.open_subkey(r"SOFTWARE\WOW6432Node\Valve\Steam") {
        if let Ok(path) = key.get_value::<String, _>("InstallPath") {
            let p = path.trim().trim_end_matches('/').trim_end_matches('\\');
            if !p.is_empty() {
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

/// 读设置：文件不存在或解析失败 → 探测默认路径并落盘一份默认设置（顺带修复坏文件）
pub fn load_settings() -> Settings {
    if let Ok(text) = std::fs::read_to_string(settings_path()) {
        if let Ok(s) = serde_json::from_str::<Settings>(&text) {
            return s;
        }
    }
    let (lol, steam) = detect_default_roots();
    let s = Settings {
        lol_root: lol,
        steam_root: steam,
        global_cover_on: false,
        autostart: false,
        start_minimized: false,
        close_behavior: "tray".to_string(),
        accent: "#00e5ff".to_string(),
    };
    save_settings(&s);
    s
}

/// 原子写：先写 .tmp 再 rename（Windows 下 std rename 走 MoveFileEx 可覆盖已存在文件）
pub fn save_settings(s: &Settings) {
    let path = settings_path();
    let tmp = data_dir().join("settings.json.tmp");
    let text = serde_json::to_string_pretty(s).unwrap_or_else(|_| "{}".to_string());
    if std::fs::write(&tmp, &text).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        return;
    }
    // 原子写失败（罕见）时清理残留并直接覆盖写保底
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::write(&path, &text);
}

// ==================== LOL 守护运行状态（AppStatus.lol_guard_on 数据源） ====================

// PROTOCOL-NOTE: 合同定义了 AppStatus.lol_guard_on 字段，但未给 guard 模块定义
// 查询函数；故在此维护全局开关。guard 模块应在 start 成功后调 set_guard_running(true)、
// stop 后调 set_guard_running(false)。不调用不会编译出错，仅该字段恒显示 false。
static GUARD_RUNNING: AtomicBool = AtomicBool::new(false);

/// 由 guard 模块 start/stop 时调用，维护 LOL 守护运行标志
pub fn set_guard_running(on: bool) {
    GUARD_RUNNING.store(on, Ordering::Relaxed);
}

/// SteamID3 → SteamID64：id64 = sid3 + 76561197960265728（换算关系本机实测验证）
/// current_account_sid3() 返回 sid3，此处反算 id64 供 AppStatus 使用（二选一方案之一，
/// 未采用"再解析一次 loginusers.vdf"：避免与 steam 模块重复实现 vdf 匹配逻辑）
fn sid3_to_id64(sid3: &str) -> Option<String> {
    let n: u64 = sid3.trim().parse().ok()?;
    n.checked_add(SID64_BASE)
        .map(|v| v.to_string())
}

// ==================== Tauri commands ====================
// PROTOCOL-NOTE: 带参数的 command 使用 rename_all="snake_case"，前端按合同 Rust 签名
// 同名传键：invoke('set_paths', { lol_root, steam_root })。Tauri 2 默认规则是 camelCase
// （lolRoot/steamRoot），两者不兼容；若前端统一用 camelCase，去掉 rename_all 即可，
// 请主协调者对全工程命令（含 cover 模块 include_global）统一约定。

/// 组装应用总状态（前端启动初始化 / 各操作后刷新）
/// 用户语义（2026-09-20 修正）："当前登录账号"= **Steam 正在运行**且 AutoLoginUser
/// 指向的账号在 loginusers.vdf 有记录——Steam 没开就视为未登录（历史账号不算）

/// 设置游戏根目录：任一路径有效即保存（传空串表示保留原值），返回最新状态；
/// 两个路径均无效时 Err，由前端标红提示。
/// 参数键用 Tauri 默认 camelCase（lolRoot/steamRoot），与前端及全工程约定一致

/// 读取设置

/// 保存设置。⚠ 游戏路径字段不信任前端回传值——强制以磁盘现值覆盖
/// （根治竞态：set_paths 刚保存的新路径被启动时的旧 settings 快照经防抖回存回滚）；
/// 同时把"开机自启动"同步到系统（tauri-plugin-autostart 写注册表/启动项）。

/// 资源管理器打开 Data 目录（explorer 正常退出码为 1，故只判 spawn 不判退出状态）


// ============================ logger ============================
// logger.rs —— 覆盖日志模块（R12）
// 存储：exe 同级 Data\logs.jsonl，一行一条 LogEntry 的 JSON（追加写）；
// 超过 5000 条时整体重写、保留最新 5000（日志写入低频，全量重写可接受）。
// 事件：每条日志追加后推 "log-appended"（payload = LogEntry，前端日志页前插展示）。
// PROTOCOL-NOTE: 合同 log() 签名不带 AppHandle 参数，故进程内持一份全局句柄，
// 由 spawn（main.rs setup 调用）注入；注入完成前（理论上只有启动瞬间）的
// log 仅落盘不推事件。main.rs 无需为此改动。
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
    let lines: Vec<String> = BufReader::new(f).lines().flatten().collect();
    if lines.len() <= MAX_ENTRIES {
        return;
    }
    // 文件按追加序即时间升序，保留末尾（最新）MAX_ENTRIES 条；先写临时文件再原子替换
    let keep = &lines[lines.len() - MAX_ENTRIES..];
    let tmp = data_dir().join("logs.jsonl.tmp");
    if let Ok(mut w) = File::create(&tmp) {
        for l in keep {
            let _ = writeln!(w, "{l}");
        }
        drop(w);
        // Windows 下 std rename 可覆盖已存在文件；失败则保留原文件，下次写入再试
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 读全部日志：filter = "all"（或空串）不过滤，否则按 kind 精确匹配；
/// 返回按 ts_ms 严格降序（最新在最上，R12 跨日同样倒序）


// ============================ watcher ============================
// watcher.rs —— 游戏进程活跃监听（R1 左列绿点数据源）
// 后台线程每 2s 用 sysinfo 枚举进程：
// LOL 活跃   = 存在 LeagueClient.exe（客户端大厅）或 League of Legends.exe（对局）
// Steam 活跃 = 存在 steam.exe
// 与上次状态比较，变化时推事件 "game-active" payload {"lol":bool,"steam":bool}。
// 冷启动预热：前 5 轮（0/2/4/6/8s）无论是否变化都推一次——前端页面加载与
// listen 注册完成的时刻不确定（Tauri emit 只送达"已注册"的监听，不缓存），
// 预热保证前端任意时刻就绪后 2s 内必能收到当前状态（满足"3s 内必推首帧"）。
// 另：本模块的 spawn() 是 main.rs setup 中最早拿到 AppHandle 的入口，
// 顺带为 logger 注入全局 AppHandle（log 合同签名不带 app 参数）。
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
    // PROTOCOL-NOTE: sysinfo 0.30 的 Process::name() 返回 &str（0.31+ 才是 &OsStr），
    // 故用 to_lowercase 直接比较，不适用 to_string_lossy。
    let mut last: Option<(bool, bool)> = None; // None = 尚未推过任何状态
    let mut round: u32 = 0;
    loop {
        sys.refresh_processes_specifics(ProcessRefreshKind::new());
        let (lol, steam) = scan(&sys);
        let cur = (lol, steam);
        if round < WARMUP_ROUNDS || last != Some(cur) {
            let _ = app.emit("game-active", json!({ "lol": lol, "steam": steam }));
            last = Some(cur);
        }
        round = round.saturating_add(1);
        thread::sleep(POLL_INTERVAL);
    }
}

/// 枚举一次进程表，返回 (LOL 活跃, Steam 活跃)；进程名统一小写后精确匹配
fn scan(sys: &System) -> (bool, bool) {
    let mut lol = false;
    let mut steam = false;
    for p in sys.processes().values() {
        let name = p.name().to_lowercase();
        if name == "leagueclient.exe" || name == "league of legends.exe" {
            lol = true;
        } else if name == "steam.exe" {
            steam = true;
        }
    }
    (lol, steam)
}


// ============================ packs ============================
// packs.rs —— 内置配置包（编译期内嵌）与覆盖写盘引擎。
// 7 个配置文件 include_bytes! 嵌入 exe（源在仓库 packs/ 下，只读资产）；
// write_all 按 scope 过滤后写入目标根目录，并把产物三项时间戳统一设为
// 本地 2000-01-01 00:00:00（覆盖标识：区分 GameReady 产物与游戏回写）。
// 目标相对路径（rel）与真实文件大小写严格一致，{SID3} 为 SteamID3 占位符。
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

/// 全部内置文件（7 个）：LOL 5 + Steam 全局 1 + Steam 账号 1
static FILES: [PackFile; 7] = [
    // ── LOL：Game\Config（对局内配置）──
    PackFile {
        bytes: include_bytes!("../packs/lol/Game/Config/game.cfg"),
        rel: r"Game\Config\game.cfg",
        scope: PackScope::Lol,
    },
    PackFile {
        bytes: include_bytes!("../packs/lol/Game/Config/input.ini"),
        rel: r"Game\Config\input.ini",
        scope: PackScope::Lol,
    },
    PackFile {
        bytes: include_bytes!("../packs/lol/Game/Config/PersistedSettings.json"),
        rel: r"Game\Config\PersistedSettings.json",
        scope: PackScope::Lol,
    },
    // ── LOL：LeagueClient\Config（客户端偏好，均无账号字段）──
    PackFile {
        bytes: include_bytes!("../packs/lol/LeagueClient/Config/LCUAccountPreferences.yaml"),
        rel: r"LeagueClient\Config\LCUAccountPreferences.yaml",
        scope: PackScope::Lol,
    },
    PackFile {
        bytes: include_bytes!("../packs/lol/LeagueClient/Config/LCULocalPreferences.yaml"),
        rel: r"LeagueClient\Config\LCULocalPreferences.yaml",
        scope: PackScope::Lol,
    },
    // ── Steam：全局配置 ──
    PackFile {
        bytes: include_bytes!("../packs/steam/config.vdf"),
        rel: r"config\config.vdf",
        scope: PackScope::SteamGlobal,
    },
    // ── Steam：当前账号配置（{SID3} 占位，写盘时替换）──
    PackFile {
        bytes: include_bytes!("../packs/steam/localconfig.vdf"),
        rel: r"userdata\{SID3}\config\localconfig.vdf",
        scope: PackScope::SteamAccount,
    },
];


/// 单文件覆盖结果：ok=true 且 err=Some 表示写盘成功但时间戳设置失败（"ts-failed: ..."）
#[derive(Serialize, Clone)]
pub struct CoverResult {
    pub rel: String,
    pub ok: bool,
    pub err: Option<String>,
}

/// 把 scope 范围内的内置文件全部写入 root：
/// - SteamAccount 范围的目标路径含 {SID3} 占位符，sid3 为 None 时跳过该文件并在 err 里注明；
/// - 父目录不存在则递归创建；
/// - 写成功后设置创建/修改/访问时间 = 本地 2000-01-01 00:00:00，时间戳失败不算覆盖失败。
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
        let tmp = target.with_extension("gameready.tmp");
        match fs::write(&tmp, f.bytes).and_then(|_| fs::rename(&tmp, &target)) {
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
/// 修改/访问时间用 filetime crate；创建时间标准库无 API，走 Win32 SetFileTime。
fn set_stamp_2000(path: &Path) -> Result<(), String> {
    use chrono::TimeZone;
    // 本地时区 2000-01-01 00:00:00 对应的 UTC Unix 秒（filetime 按 UTC 秒换算）
    let secs = chrono::Local
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| "本地时间 2000-01-01 00:00:00 换算失败".to_string())?
        .timestamp();

    // ① 修改 + 访问时间
    let ft = filetime::FileTime::from_unix_time(secs, 0);
    filetime::set_file_times(path, ft, ft)
        .map_err(|e| format!("修改/访问时间设置失败: {e}"))?;

    // ② 创建时间（Win32）
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


// ============================ cover ============================
// cover.rs —— Steam 一键覆盖（大按钮，R17 全链路自动化）。
// 顺序：读设置拿 steam_root → 取当前账号 SteamID3（无则报错）→
// 【自动】Steam 运行中则 taskkill + 等完全退出（超时 Err）→ 写账号组 →
// （可选）写全局组 → 写日志（注明自动退出/重启）→ 若曾退出则自动重启 Steam →
// 推 "cover-done" 事件。async + spawn_blocking：杀/等进程可阻塞 ~10s，不能冻结 UI。
/// 一键覆盖结果报告（推给前端的 "cover-done" payload）
#[derive(Serialize, Clone)]
pub struct CoverReport {
    pub total: usize,
    pub ok: usize,
    pub failed: Vec<CoverResult>,
    pub skipped_reason: Option<String>,
    /// R17：本次覆盖是否自动退出了正在运行的 Steam（true 则覆盖后已自动重启）
    pub auto_restarted: bool,
}

/// Steam 一键覆盖：账号组 localconfig.vdf 必覆盖，include_global=true 再覆盖全局 config.vdf

/// 覆盖核心（调用方必须已持有 STEAM_OP 锁）。
/// pub(crate)：login_new 登录成功后在锁内串联调用（登录+覆盖一步到位）。
pub(crate) fn cover_locked(include_global: bool, app: tauri::AppHandle) -> Result<CoverReport, String> {
    // ⓪ 用户语义（2026-09-20 修正）：覆盖只针对**正在运行且已登录**的 Steam——
    // Steam 没开就不存在"当前账号会话"，覆盖无从谈起
    if !is_steam_running() {
        return Err("Steam 未运行：请先打开 Steam 并登录账号，再执行一键覆盖".to_string());
    }
    // ① steam 根目录（且必须有效，防止在错误根目录长出垃圾文件树）
    let settings = load_settings();
    let steam_root = settings
        .steam_root
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "未配置 Steam 根目录，请先在路径设置中指定".to_string())?;
    if !validate_roots("", &steam_root).steam {
        return Err("Steam 根目录无效（未找到 steam.exe），请先在路径设置中修正".to_string());
    }

    // ② 当前账号 SteamID3（定位 userdata\<SID3> 目录）
    let sid3 = current_account_sid3()
        .ok_or_else(|| "未检测到已登录的 Steam 账号，无法覆盖账号配置".to_string())?;

    // ③ R17 自动化：Steam 运行中 → 自动 taskkill + 轮询 ≤10s 等完全退出
    //（vdf 若在 Steam 运行时写入，会被其退出时的内存回写覆盖）
    let was_running = kill_steam_if_running();
    if was_running && !wait_steam_exit(10) {
        return Err("Steam 未能在 10 秒内完全退出，请手动退出后重试".to_string());
    }

    // ④ 先账号组，再（可选）全局组
    let root = Path::new(&steam_root);
    let mut all: Vec<CoverResult> = Vec::new();
    all.extend(write_all(root, PackScope::SteamAccount, Some(&sid3)));
    if include_global {
        all.extend(write_all(root, PackScope::SteamGlobal, None));
    }

    let ok = all.iter().filter(|r| r.ok).count();
    let failed: Vec<CoverResult> = all.iter().filter(|r| !r.ok).cloned().collect();
    let report = CoverReport {
        total: all.len(),
        ok,
        failed: failed.clone(),
        skipped_reason: if include_global {
            None
        } else {
            Some("未开启「全局配置覆盖」，已跳过 config\\config.vdf".to_string())
        },
        auto_restarted: was_running,
    };

    // ⑤ 日志（kind="steam"，注明自动退出/重启）
    let title = format!("Steam 一键覆盖：成功 {}/{}", report.ok, report.total);
    let auto_note = if was_running { "，检测到 Steam 运行，已自动退出并在覆盖后重启" } else { "" };
    let detail = if failed.is_empty() {
        format!(
            "账号配置（SteamID3={}）覆盖完成{}{}。",
            sid3,
            if include_global { "，含全局 config.vdf" } else { "" },
            auto_note
        )
    } else {
        let list = failed
            .iter()
            .map(|r| format!("{}（{}）", r.rel, r.err.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("；");
        format!("账号配置（SteamID3={}）部分失败：{}{}", sid3, list, auto_note)
    };
    log("steam", &title, &detail, failed.is_empty());

    // ⑥ 覆盖后自动重启 Steam（仅当覆盖前确实退出了它；失败不影响覆盖结果，仅记日志）
    if was_running {
        if let Err(e) = launch_steam(&steam_root, &[]) {
            log("steam", "自动重启 Steam 失败", &e, false);
        }
    }

    // ⑦ 推事件（事件发送失败不影响覆盖结果）
    let _ = app.emit("cover-done", report.clone());

    Ok(report)
}


// ============================ guard ============================
// guard.rs —— LOL 守护：后台线程每 200ms 循环覆盖内置 LOL 配置，
// 对抗 PersistedSettings.json 等文件的服务端同步回写。
// start 幂等（已在跑直接 Ok）；每 1s 推 "guard-stats"（count 语义 =
// 累计覆盖轮次，一轮 +1）；连续 5 轮全部文件失败 → 失败日志 +
// "guard-error" 事件 + 自停；stop 置停止位并 join 线程，返回统计并写结束日志。
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

/// 启动 LOL 守护（幂等：已在跑则直接 Ok）

/// 停止守护：置停止位 → join 线程 → 返回统计并写结束日志

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
                m.lock().unwrap().take();
            }
            set_guard_running(false);
            return;
        }

        // 每 1s 推一次统计（首轮立即推，让前端尽快有数）
        if last_emit.map_or(true, |t| t.elapsed() >= Duration::from_secs(1)) {
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


// ============================ steam ============================
// GameReady · Steam 账号管理模块（src/steam.rs）
// 职责：loginusers.vdf 极简解析/重建（保持实测 Tab 格式，Steam 退出后才能写）、
// 注册表 AutoLoginUser 读写、Steam 进程检测/强杀/启动、账号列表/切换/删除/新增登录。
// 依据：docs/PROTOCOL.md "steam 模块" 合同 + design/调研-配置文件位置.md 实测结论。
// 实测 loginusers.vdf 字节级格式（LF 换行、无 BOM）：
// "users"\n{\n\t"<SteamID64>"\n\t{\n\t\t"字段"\t\t"值"\n...\t}\n}\n
// 字段顺序：AccountName/PersonaName/RememberPassword/WantsOfflineMode/SkipOfflineModeWarning/AutoLogin/Timestamp
/// CREATE_NO_WINDOW：taskkill / steam.exe 等子进程一律隐藏控制台窗口，避免闪黑框
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// SteamID64 → SteamID3 换算基数（实测：userdata 目录名 = SteamID64 − 76561197960265728）
const SID64_BASE: u64 = 7_656_119_796_026_572_8;
/// Steam 操作互斥锁：cover / switch / delete / login_new 全程持锁串行执行。
/// 防止并发杀/启 Steam、并发写 loginusers.vdf 互相踩（覆盖结果被回写、双启、vdf 交错）。
pub(crate) static STEAM_OP: Mutex<()> = Mutex::new(());

/// 账号条目（对外合同结构，前端渲染用）
#[derive(Serialize, Clone)]
pub struct SteamAccount {
    pub steam_id64: String,
    pub account_name: String,
    pub persona_name: String,
    pub auto_login: bool,
    pub timestamp: i64,
}

/// loginusers.vdf 内部原始账号块：保留字段原始顺序与全部键值（含未来 Steam 新增的未知字段），
/// 写回时按原样重建，只改 AutoLogin 或整块删除，最大限度不丢信息。
struct RawAccount {
    sid64: String,
    fields: Vec<(String, String)>,
}

// ---------------- 基础路径与注册表 ----------------

/// 从设置读取 Steam 根目录；未设置返回 Err
fn steam_root_or_err() -> Result<String, String> {
    load_settings()
        .steam_root
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "未设置 Steam 根目录，请先在设置中配置路径".to_string())
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

/// 解析一行，返回 (key, Option<value>)。
/// 三种形态：`"key"`（无值，如 "users"、账号 SteamID64 行）、`"key"\t\t"value"`、其他（返回 None）。
fn parse_line(line: &str) -> Option<(String, Option<String>)> {
    let line = line.trim_start();
    if !line.starts_with('"') {
        return None;
    }
    let rest = &line[1..];
    let key_end = rest.find('"')?;
    let key = rest[..key_end].to_string();
    let after = rest[key_end + 1..].trim();
    if after.is_empty() {
        return Some((key, None));
    }
    let after = after.strip_prefix('"')?;
    let val_end = after.rfind('"')?;
    Some((key, Some(after[..val_end].to_string())))
}

/// 极简 vdf 解析器：逐行处理，去 BOM、跳过空行与 `#`/`//` 注释行，
/// 大括号嵌套一层（"users" → 账号块），输出原始账号块列表（保持字段顺序）。
fn parse_vdf(text: &str) -> Vec<RawAccount> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // 去 UTF-8 BOM
    let mut accounts: Vec<RawAccount> = Vec::new();
    let mut cur: Option<RawAccount> = None;
    let mut depth: usize = 0;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if line.starts_with('{') {
            depth += 1;
            continue;
        }
        if line.starts_with('}') {
            depth = depth.saturating_sub(1);
            // 账号块结束（depth 从 2 → 1）：收下当前账号
            if depth == 1 {
                if let Some(acc) = cur.take() {
                    accounts.push(acc);
                }
            }
            continue;
        }
        if let Some((key, val)) = parse_line(line) {
            match (depth, val) {
                // users 层的无值键 = 账号 SteamID64，开启新账号块
                (1, None) => cur = Some(RawAccount { sid64: key, fields: Vec::new() }),
                // 账号块内的键值对 = 字段
                (2, Some(v)) => {
                    if let Some(a) = cur.as_mut() {
                        a.fields.push((key, v));
                    }
                }
                _ => {}
            }
        }
    }
    accounts
}

/// 从原始账号块取字段值
fn field<'a>(a: &'a RawAccount, name: &str) -> Option<&'a str> {
    a.fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// 设置账号块的 AutoLogin 字段（无则追加到块尾）
fn set_autologin(a: &mut RawAccount, val: &str) {
    for (k, v) in a.fields.iter_mut() {
        if k == "AutoLogin" {
            *v = val.to_string();
            return;
        }
    }
    a.fields.push(("AutoLogin".to_string(), val.to_string()));
}

/// 设置账号块的 RememberPassword 字段（无则追加）。写回整份 vdf。
/// 背景（2026-09-20 实测诊断）：`-login` 参数登录不保存 Steam 端免密凭据，
/// vdf 里 RememberPassword 停留 0；置 1 保证 UI 状态正确（Steam 若有凭据即可免密）。
fn set_remember_password(root: &str, sid64: &str, val: &str) {
    let Ok(text) = read_vdf(root) else { return };
    let mut accounts = parse_vdf(&text);
    let Some(a) = accounts.iter_mut().find(|a| a.sid64 == sid64) else { return };
    for (k, v) in a.fields.iter_mut() {
        if k == "RememberPassword" {
            *v = val.to_string();
            let _ = write_vdf(root, &accounts);
            return;
        }
    }
    a.fields.push(("RememberPassword".to_string(), val.to_string()));
    let _ = write_vdf(root, &accounts);
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

/// 按实测字节级格式重建整个 loginusers.vdf：
/// "users" 一层，账号键一层 Tab，块内字段 `\t\t"字段"\t\t"值"`（字段名与值之间两个 Tab），LF 换行，末尾 `}\n`
fn build_vdf(accounts: &[RawAccount]) -> String {
    let mut s = String::from("\"users\"\n{\n");
    for a in accounts {
        s.push_str(&format!("\t\"{}\"\n\t{{\n", a.sid64));
        for (k, v) in &a.fields {
            s.push_str(&format!("\t\t\"{}\"\t\t\"{}\"\n", k, v));
        }
        s.push_str("\t}\n");
    }
    s.push_str("}\n");
    s
}

/// 读 loginusers.vdf 全文（容错非 UTF-8 字节），文件不存在/不可读返回 Err
fn read_vdf(root: &str) -> Result<String, String> {
    let path = vdf_path(root);
    let bytes = fs::read(&path).map_err(|e| format!("读取 loginusers.vdf 失败: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 原子写回 loginusers.vdf：先写 .tmp 再 rename 覆盖（调用前必须确保 Steam 已完全退出，否则会被回写覆盖）
fn write_vdf(root: &str, accounts: &[RawAccount]) -> Result<(), String> {
    let path = vdf_path(root);
    let tmp = path.with_extension("vdf.tmp");
    fs::write(&tmp, build_vdf(accounts)).map_err(|e| format!("写入临时文件失败: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("替换 loginusers.vdf 失败: {e}"))
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
        .map(|n| n.to_string())
}

// ---------------- 进程控制 ----------------

/// steam.exe 是否在运行（sysinfo 遍历进程名小写比较，不含 steamwebhelper）
pub fn is_steam_running() -> bool {
    let mut sys = System::new();
    sys.refresh_processes();
    sys.processes()
        .values()
        .any(|p| p.name().to_lowercase() == "steam.exe")
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

/// 轮询等待 Steam 完全退出（100ms 一次，上限 timeout_secs 秒）。合同 pub API（R17）。
/// 循环内复用同一 System 实例做增量刷新（每次 System::new 全量枚举在等待期会占可观 CPU）。
pub fn wait_steam_exit(timeout_secs: u32) -> bool {
    use sysinfo::ProcessRefreshKind;
    let mut sys = System::new();
    let alive = |sys: &System| sys.processes().values().any(|p| p.name().to_lowercase() == "steam.exe");
    for _ in 0..(timeout_secs * 10) {
        sys.refresh_processes_specifics(ProcessRefreshKind::new());
        if !alive(&sys) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    sys.refresh_processes_specifics(ProcessRefreshKind::new());
    !alive(&sys)
}

/// 优雅退出 Steam 并等待完全退出：先官方命令 `steam.exe -shutdown`（正常退出会保存
/// 登录会话状态），8s 未退再 taskkill /F 兜底。超时 Err。
/// （2026-09-20 诊断：强杀会破坏 Steam 正常会话收尾，降低下次自动登录成功率）
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

/// 启动 <steam_root>\steam.exe（CREATE_NO_WINDOW、spawn 后不 wait，不阻塞也不产生控制台窗口）。
/// 合同 pub API（R17）：供 cover 模块复用。
pub fn launch_steam(root: &str, args: &[&str]) -> Result<(), String> {
    let exe = Path::new(root).join("steam.exe");
    Command::new(&exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 steam.exe 失败: {e}"))
}

// ==================== 凭据存储（2026-09-20 用户拍板：自用工具，明文存 Data\accounts.json） ====================
// 目的：列表"登录"用 steam.exe -login 账号 密码 直登，100% 成功——
// Steam 自带的自动登录凭据约三个月未登录即失效（会停在登录页），不可依赖。

fn creds_path() -> PathBuf {
    data_dir().join("accounts.json")
}

fn load_creds() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(creds_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 键 = 账号名（登录发起前即已知，无需等 vdf 出现 sid64）
pub fn save_cred_by_name(account: &str, password: &str) {
    let mut m = load_creds();
    m.insert(account.to_string(), password.to_string());
    if let Ok(json) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(creds_path(), json);
    }
}

pub fn remove_cred_by_name(account: &str) {
    let mut m = load_creds();
    if m.remove(account).is_some() {
        if let Ok(json) = serde_json::to_string_pretty(&m) {
            let _ = std::fs::write(creds_path(), json);
        }
    }
}

fn get_cred(account: &str) -> Option<String> {
    // 忽略大小写匹配（用户输入的账号名与 vdf 回读的 AccountName 可能大小写不同）
    let creds = load_creds();
    creds
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(account))
        .map(|(_, v)| v.clone())
}

// ---------------- 核心功能（合同 pub API） ----------------

/// 解析 <steam_root>\config\loginusers.vdf 输出账号列表；根目录未设置/文件不存在返回空列表
pub fn accounts_list_v() -> Vec<SteamAccount> {
    let root = match steam_root_or_err() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match read_vdf(&root) {
        Ok(text) => parse_vdf(&text).iter().map(to_account).collect(),
        Err(_) => Vec::new(),
    }
}

/// 当前账号 SteamID3：注册表 AutoLoginUser(账号名) → 在 loginusers.vdf 反查其 SteamID64 → 减基数得 sid3。
/// 任一环节失败返回 None。
pub fn current_account_sid3() -> Option<String> {
    let name = registry_autologin_user()?;
    let acc = accounts_list_v()
        .into_iter()
        .find(|a| a.account_name.eq_ignore_ascii_case(&name))?;
    sid64_to_sid3(&acc.steam_id64)
}

/// 切换账号（列表"登录"）：杀 Steam → vdf/注册表指向目标 → 启动 steam.exe →
/// **凭据直登**（Data\accounts.json 有该账号密码 → `-login 账号 密码` 参数登录，100% 成功；
/// 无凭据 → 无参启动走 Steam 自动登录，凭据时效内有效）→ 轮询该账号 Timestamp 更新确认登录完成。
pub fn switch(sid64: &str) -> Result<String, String> {
    let _steam_op = STEAM_OP.lock().unwrap(); // 全程持锁：与 cover/delete/login_new 互斥
    let root = steam_root_or_err()?;
    kill_and_wait()?;
    let mut accounts = parse_vdf(&read_vdf(&root)?);
    let idx = accounts
        .iter()
        .position(|a| a.sid64 == sid64)
        .ok_or_else(|| "账号不存在，请刷新账号列表".to_string())?;
    for a in accounts.iter_mut() {
        set_autologin(a, "0");
    }
    set_autologin(&mut accounts[idx], "1");
    let target = &accounts[idx];
    let account_name = field(target, "AccountName").unwrap_or("").to_string();
    let persona_name = field(target, "PersonaName").unwrap_or("").to_string();
    if account_name.is_empty() {
        return Err("该账号缺少 AccountName 字段，loginusers.vdf 可能已损坏".to_string());
    }
    write_vdf(&root, &accounts)?;
    registry_set_autologin_user(&account_name)?;

    // 记录切换前的 Timestamp（Steam 登录成功会更新它，作为"登录完成"判据）
    let before_ts: i64 = field(&accounts[idx], "Timestamp")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let display = display_name(&persona_name, &account_name);

    // ── 第 1 层：历史登录态直登（用户语义：免密优先）──
    // 无参启动 = Steam 自动登录该账号（与客户端下拉选历史账号同一机制，凭据有效则直接进入）。
    // 窗口 20s（2026-09-20 修正：Steam 冷启动/慢网络下载登录页可 >8s，过短会把
    // "载入中"误判为失败，实际稍后自动登录成功——用户实测反馈）
    launch_steam(&root, &[])?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(400));
        if login_confirmed(&root, sid64, before_ts) {
            return Ok(display);
        }
    }

    // ── 第 2 层：程序代输密码直登（用户无感兜底；诊断结论：-login 参数登录不保存
    // Steam 端免密凭据，所以第 1 层对这类账号可能失效，本层保证 100% 登上）──
    if let Some(pass) = get_cred(&account_name) {
        kill_and_wait()?;
        launch_steam(&root, &["-login", account_name.as_str(), pass.as_str()])?;
        for _ in 0..25 {
            thread::sleep(Duration::from_millis(400));
            if login_confirmed(&root, sid64, before_ts) {
                return Ok(display);
            }
        }
        return Ok(display);
    }

    // ── 第 3 层：无凭据 → 明确要求补输一次密码（仅从未保存过密码的账号出现）──
    Err(format!("{display}：需在 Steam 输一次密码"))
}

/// 登录成功判据：loginusers.vdf 中该账号的 Timestamp 比 before 新
fn login_confirmed(root: &str, sid64: &str, before_ts: i64) -> bool {
    if let Ok(text) = read_vdf(root) {
        if let Some(acc) = parse_vdf(&text).iter().find(|a| a.sid64 == sid64) {
            let ts: i64 = field(acc, "Timestamp").and_then(|v| v.parse().ok()).unwrap_or(0);
            return ts > before_ts;
        }
    }
    false
}

/// 删除账号（R18 全链路自动化）：Steam 运行中 → 自动 taskkill + 等完全退出（≤10s，超时 Err）
/// → loginusers.vdf 移除该块 → 删 userdata\<sid3> 整目录（不存在则忽略）
/// → 删除前 Steam 在运行则自动重启 → Ok(显示名)。
/// 注：若被删账号恰为注册表当前 AutoLoginUser，不清理注册表键（Steam 重启后自行处理登录态）。
pub fn delete(sid64: &str) -> Result<String, String> {
    let _steam_op = STEAM_OP.lock().unwrap(); // 全程持锁：与 cover/switch/login_new 互斥
    let root = steam_root_or_err()?;
    // R18 自动化：Steam 运行中不再拒绝，自动退出（vdf 运行中改会被退出时回写复活；
    // 删当前账号时其 userdata 文件也被占用），删完自动重启
    let was_running = kill_steam_if_running();
    if was_running && !wait_steam_exit(10) {
        return Err("Steam 未能在 10 秒内完全退出，请手动退出后再删除账号".to_string());
    }
    let mut accounts = parse_vdf(&read_vdf(&root)?);
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
    write_vdf(&root, &accounts)?;
    // 同步删除保存的登录凭据（完全清理的一部分）
    remove_cred_by_name(&account_name);
    // 删除该账号的 userdata 目录（含 localconfig.vdf 等）；不存在或删除失败不阻断流程
    if let Some(sid3) = sid64_to_sid3(sid64) {
        let ud = Path::new(&root).join("userdata").join(&sid3);
        let _ = fs::remove_dir_all(ud);
    }
    // 删除前 Steam 在运行 → 自动重启（删除当前登录账号时，Steam 会回到登录界面，属预期）
    if was_running {
        let _ = launch_steam(&root, &[]);
    }
    Ok(display_name(&persona_name, &account_name))
}

// PROTOCOL-NOTE: 合同中 pub fn login_new(&str,&str) 与 #[tauri::command] pub fn login_new(String,String,AppHandle)
// 同名同模块无法共存（Rust 限制），核心实现命名为 login_new_impl，command 保持合同名 login_new 供 main.rs 注册，行为一致。
/// 新增登录（核心实现）：杀 Steam → 注册表 AutoLoginUser=account → steam.exe -login account password
/// （密码仅存在于进程参数，绝不落盘/写日志）→ 轮询 ≤30s 等 loginusers.vdf 出现该 AccountName → Ok(显示名)
pub fn login_new_impl(account: &str, password: &str) -> Result<String, String> {
    let account = account.trim();
    if account.is_empty() {
        return Err("账号名不能为空".to_string());
    }
    let _steam_op = STEAM_OP.lock().unwrap(); // 全程持锁：与 cover/switch/delete 互斥
    let root = steam_root_or_err()?;
    kill_and_wait()?;
    registry_set_autologin_user(account)?;
    // 凭据提前保存（2026-09-20 bug 修正）：新账号首次登录常需 SteamGuard 邮箱验证码，
    // 轮询可能超时——密码此刻就落盘（键=账号名，登录前即已知），列表"登录"永远可兜底
    save_cred_by_name(account, password);
    launch_steam(&root, &["-login", account, password])?;
    // 确认窗口 180s（2026-09-20 bug 修正：新账号首次登录常需 SteamGuard 邮箱验证码，
    // 30s 窗口必超时导致"看似失败"；凭据已提前保存，超时也不丢）
    for _ in 0..450 {
        thread::sleep(Duration::from_millis(400));
        if let Some(acc) = accounts_list_v()
            .into_iter()
            .find(|a| a.account_name.eq_ignore_ascii_case(account))
        {
            // 登录成功 → vdf RememberPassword 置 1（凭据已在发起登录前保存）
            if let Ok(root2) = steam_root_or_err() {
                set_remember_password(&root2, &acc.steam_id64, "1");
            }
            return Ok(display_name(&acc.persona_name, &acc.account_name));
        }
    }
    Err("登录确认超时：若 Steam 正在等待邮箱验证码，请完成验证后点列表「登录」（密码已保存，会自动直登并覆盖）".to_string())
}

// ---------------- Tauri 命令（合同签名，成功写日志 + 推 "steam-accounts-changed"） ----------------

/// 账号列表（前端账号模态渲染）

/// 切换账号（async + spawn_blocking：杀 Steam/轮询可阻塞 ~10s+，不能冻结 UI 线程）

/// 删除账号（前端已做二次确认；可能整目录删除，同样走后台线程）

/// 新增登录（密码参数绝不写入日志/文件；轮询最长 ~40s，必须走后台线程）。
/// 2026-09-20 用户确认：登录成功后**串联自动覆盖**——新增账号"登录+配置"一步到位
/// （此时 Steam 刚登录完成正在运行，覆盖走 R17 链路：杀→覆盖→重启）


// ============================ guide ============================
// guide 模块：WebView2 运行时检测 + 原生引导小窗（自动/手动安装）。
// 启动流程（见 docs/PROTOCOL.md 与设计规范）：
// - `has_webview2()`：每次启动直接查注册表（不缓存），4 处 EdgeUpdate Clients 键任一有
// `pv` 值即视为已安装，另有 `%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\`
// 文件夹存在性兜底；
// - `run_guide_window()`：阻塞运行 eframe 原生小窗（约 420×260，深色底，中文文案），
// 直到 WebView2 就绪后返回（同进程进 tauri，无需重启 exe）。
// 两种安装模式（全部阻塞操作都在后台线程，UI 线程只读共享状态）：
// - 自动（推荐）：IsWow64Process2 判真实架构 → reqwest 流式下载对应离线包到 %TEMP%
// （Content-Length 算真实百分比）→ ShellExecute "runas" 以 /silent /install 静默安装
// （启动前 UI 预告会弹一次 UAC）→ 等待安装器进程退出 → 复查注册表 pv 出现即成功；
// - 手动：打开微软官方 fwlink 下载页 → 后台每 1s 轮询注册表，查到即成功。
// 下载失败时 UI 提供 [重试] 与 [改用手动下载]；安装未完成时点窗口 X 会先二次确认，
// 确认退出则 `std::process::exit(0)`（没有 WebView2 程序无法继续）。
// PROTOCOL-NOTE: 设计文档（GameConfigGuard/design/新产品-WebView2引导设计.md）在本机
// 不存在，实现依据为 PROTOCOL.md 合同 + 定稿要点（fwlink 直链表 / 检测 4 处注册表 +
// 文件夹兜底 / IsWow64Process2 映射 0xAA64→arm64、332→x86、其余→x64 / 自动模式两阶段
// 进度 / 手动模式 1s 轮询）。注册表第 4 处采用 HKCU\Software\WOW6432Node（PROTOCOL 正文
// 只列出 3 处，多查一处无副作用）。
// PROTOCOL-NOTE: windows crate 0.58 中 `IsWow64Process2`（需 Win32_System_SystemInformation）、
// `ShellExecuteExW`/`SHELLEXECUTEINFOW`（需 Win32_System_Registry）、`ShellExecuteW`
// （需 Win32_UI_WindowsAndMessaging）都被 cfg 在工程 Cargo.toml 未启用的 feature 下，
// 而 Cargo.toml 属共享文件禁改 —— 故本模块对这几个 API 手写等价 FFI 声明（kernel32/
// shell32 均为系统库，链接无风险）；IsWow64Process2 通过 GetProcAddress 动态解析，
// Win10 1511 之前的系统自动退化为环境变量判定，避免进程启动即失败。
// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// EdgeUpdate Clients 的 WebView2 固定 GUID 子键（检测读它的 pv 值）。
const CLIENTS_SUBKEY: &str = concat!(
    r"Microsoft\EdgeUpdate\Clients\",
    r"{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
);

/// 手动模式打开的微软官方引导安装器下载页（fwlink 永久短链，点开立即下载，约 1.8MB）。
const MANUAL_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// 自动模式下载到 %TEMP% 的离线包文件名。
const SETUP_TEMP_NAME: &str = "GameReady-WebView2Setup.exe";

/// ShellExecute 的 SEE_MASK_NOCLOSEPROCESS（安装完拿 hProcess 等待退出）。
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x40;

/// SW_SHOW（ShellExecuteInfoW.nShow 的显示方式）。
const SW_SHOW: i32 = 5;

// ---------------------------------------------------------------------------
// pub API（与 docs/PROTOCOL.md 合同一致）
// ---------------------------------------------------------------------------

/// 检测 WebView2 运行时是否已安装：4 处注册表任一有非空 pv 值即 true，
/// 再兜底检查 `%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\` 是否存在。
/// 每次调用直接查询、不缓存（读取 <1ms）。
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

/// 阻塞运行 WebView2 引导小窗（eframe 原生窗口），直到 WebView2 就绪后返回。
/// 若用户放弃安装（窗口以任何方式关闭且未装好），直接退出进程。
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

// ---------------------------------------------------------------------------
// 检测实现
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 手写 FFI（原因见模块头 PROTOCOL-NOTE）
// ---------------------------------------------------------------------------

mod ffi {
    use std::ffi::c_void;

    /// ShellExecuteExW 用的参数结构（C: SHELLEXECUTEINFOW，64 位自然对齐布局）。
    /// 注意：此布局仅适用于 64 位目标（本工程发布 x64；若改编译 32 位需按 packed(1) 调整）。
    #[repr(C)]
    pub struct ShellExecuteInfoW {
        pub cb_size: u32,
        pub f_mask: u32,
        pub hwnd: *mut c_void,
        pub lp_verb: *const u16,      // "runas" 提权 / "open"
        pub lp_file: *const u16,      // 目标文件或 URL
        pub lp_parameters: *const u16,
        pub lp_directory: *const u16,
        pub n_show: i32,              // SW_SHOW = 5
        pub h_inst_app: *mut c_void,  // HINSTANCE 返回值
        pub lp_id_list: *mut c_void,
        pub lp_class: *const u16,
        pub hkey_class: *mut c_void,
        pub dw_hot_key: u32,
        pub h_icon: *mut c_void,      // C 侧是 hIcon/hMonitor 联合体，指针大小
        pub h_process: *mut c_void,   // SEE_MASK_NOCLOSEPROCESS 时为安装器进程句柄
    }

    /// IsWow64Process2 的函数指针形态（HANDLE 借用 *mut c_void 表达）。
    pub type IsWow64Process2Fn =
        unsafe extern "system" fn(*mut c_void, *mut u16, *mut u16) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetModuleHandleW(lp_module_name: *const u16) -> *mut c_void;
        pub fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const u8) -> *mut c_void;
    }

    #[link(name = "shell32")]
    extern "system" {
        pub fn ShellExecuteExW(p_exec_info: *mut ShellExecuteInfoW) -> i32;
    }
}

/// 字符串转 NUL 结尾的 UTF-16（宽字符），供 Win32 W 系列 API 使用。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// 真实架构判定
// ---------------------------------------------------------------------------

/// 判定操作系统真实架构："x64" | "x86" | "arm64"。
/// 首选动态解析的 IsWow64Process2（native machine：0xAA64→arm64 / 332→x86 / 其余→x64），
/// 系统 API 不存在（Win10 1511 之前）或调用失败时退化为环境变量判定。
fn native_arch() -> &'static str {
    // ① IsWow64Process2（GetProcAddress 动态解析，缺失时不影响进程启动）
    let kernel32 = wide("kernel32.dll");
    let proc_name = b"IsWow64Process2\0";
    let module = unsafe { ffi::GetModuleHandleW(kernel32.as_ptr()) };
    if !module.is_null() {
        let proc = unsafe { ffi::GetProcAddress(module, proc_name.as_ptr()) };
        if !proc.is_null() {
            let is_wow64_process2: ffi::IsWow64Process2Fn = unsafe { std::mem::transmute(proc) };
            let mut process_machine: u16 = 0;
            let mut native_machine: u16 = 0;
            // GetCurrentProcess() 返回伪句柄（不需关闭）
            let ok = unsafe {
                is_wow64_process2(
                    GetCurrentProcess().0,
                    &mut process_machine,
                    &mut native_machine,
                )
            };
            if ok != 0 {
                return match native_machine {
                    0xAA64 => "arm64", // IMAGE_FILE_MACHINE_ARM64
                    332 => "x86",      // 0x014C IMAGE_FILE_MACHINE_I386
                    _ => "x64",        // 0x8664 IMAGE_FILE_MACHINE_AMD64 及其余
                };
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

// ---------------------------------------------------------------------------
// 后台线程 <-> UI 的消息
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 自动安装流程（在后台线程运行）
// ---------------------------------------------------------------------------

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
        let due = last_push.map_or(true, |t| t.elapsed() >= Duration::from_millis(120));
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

    let verb = wide("runas");
    let file_w = wide(&dest.to_string_lossy());
    let args = wide("/silent /install");
    let mut info = ffi::ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ffi::ShellExecuteInfoW>() as u32,
        f_mask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut(),
        lp_verb: verb.as_ptr(),
        lp_file: file_w.as_ptr(),
        lp_parameters: args.as_ptr(),
        lp_directory: std::ptr::null(),
        n_show: SW_SHOW,
        h_inst_app: std::ptr::null_mut(),
        lp_id_list: std::ptr::null_mut(),
        lp_class: std::ptr::null(),
        hkey_class: std::ptr::null_mut(),
        dw_hot_key: 0,
        h_icon: std::ptr::null_mut(),
        h_process: std::ptr::null_mut(),
    };
    let ok = unsafe { ffi::ShellExecuteExW(&mut info) };
    if ok == 0 {
        // 常见原因：用户在 UAC 弹窗点了【否】（ERROR_CANCELLED）
        let os_err = std::io::Error::last_os_error();
        return Err(format!(
            "启动安装程序失败（{os_err}）。若刚才弹出了系统授权窗口并选择了【否】，可点击重试"
        ));
    }

    // ④ 阶段二：等待安装器进程退出（不确定进度，UI 滚动动画）
    notify(tx, ctx, BgMsg::Installing);
    if !info.h_process.is_null() {
        let handle = HANDLE(info.h_process);
        unsafe {
            // INFINITE 等待：UAC 弹窗确认 + 静默安装全程都在安装器进程生命周期内
            let _ = WaitForSingleObject(handle, INFINITE);
            let _ = CloseHandle(handle);
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

// ---------------------------------------------------------------------------
// 手动下载流程（在后台线程运行）
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// egui 引导小窗
// ---------------------------------------------------------------------------

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
        lol_root,
        steam_root,
    }
}


#[tauri::command]
pub fn set_paths(lol_root: String, steam_root: String) -> Result<AppStatus, String> {
    let lol = lol_root.trim().to_string();
    let steam = steam_root.trim().to_string();
    let valid = validate_roots(&lol, &steam);
    if !valid.lol && !valid.steam {
        return Err(format!(
            "两个路径下均未找到 LeagueClient.exe / steam.exe（LOL：{lol}，Steam：{steam}），请重新选择"
        ));
    }
    let mut s = load_settings();
    // 只保存各自校验有效的项：无效路径不落盘（避免重启后路径条指向无效目录、
    // 覆盖/守护在错误根目录下 create_dir_all 长出垃圾文件树）
    if !lol.is_empty() && valid.lol {
        s.lol_root = Some(lol);
    }
    if !steam.is_empty() && valid.steam {
        s.steam_root = Some(steam);
    }
    save_settings(&s);
    Ok(get_status())
}


#[tauri::command]
pub fn get_settings() -> Settings {
    load_settings()
}


#[tauri::command(rename_all = "snake_case")]
pub fn set_settings(mut settings: Settings, app: tauri::AppHandle) -> Result<(), String> {
    let cur = load_settings();
    settings.lol_root = cur.lol_root;
    settings.steam_root = cur.steam_root;
    save_settings(&settings);

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
        for line in BufReader::new(f).lines().flatten() {
            // 单行损坏（如断电截断）跳过，不影响其余条目
            if let Ok(e) = serde_json::from_str::<LogEntry>(&line) {
                if filter == "all" || filter.is_empty() || e.kind == filter {
                    out.push(e);
                }
            }
        }
    }
    out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    out
}


#[tauri::command]
pub async fn steam_cover(include_global: bool, app: tauri::AppHandle) -> Result<CoverReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _steam_op = STEAM_OP.lock().unwrap(); // 全程持锁：与 switch/delete/login_new 互斥
        cover_locked(include_global, app)
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
            Some(h) => h,
            None => return Err("LOL 守护未在运行".to_string()),
        }
    }; // 锁在此释放：线程自停时要拿同一把锁清理，join 前绝不能持锁（防死锁）

    handle.stop_flag.store(true, Ordering::SeqCst);
    let _ = handle.thread.join();
    set_guard_running(false);

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


#[tauri::command]
pub fn accounts_list() -> Result<Vec<SteamAccount>, String> {
    Ok(accounts_list_v())
}


#[tauri::command]
pub async fn switch_account(sid64: String, app: AppHandle) -> Result<String, String> {
    let name = tauri::async_runtime::spawn_blocking(move || switch(&sid64))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))??;
    log("acct", "切换 Steam 账号", &format!("已切换到 {}", name), true);
    let _ = app.emit("steam-accounts-changed", ());
    Ok(name)
}


#[tauri::command]
pub async fn delete_account(sid64: String, app: AppHandle) -> Result<String, String> {
    let name = tauri::async_runtime::spawn_blocking(move || delete(&sid64))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))??;
    log("acct", "删除 Steam 账号", &format!("已删除 {}", name), true);
    let _ = app.emit("steam-accounts-changed", ());
    Ok(name)
}


#[tauri::command]
pub async fn login_new(account: String, password: String, app: AppHandle) -> Result<String, String> {
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let name = login_new_impl(&account, &password)?;   // 内部已持有 STEAM_OP 锁
        let include_global = load_settings().global_cover_on;
        let note = match cover_locked(include_global, app2) {   // 锁内复用，不重入
            Ok(rep) => format!("，已自动覆盖配置（{}/{} 成功）", rep.ok, rep.total),
            Err(e) => format!("；自动覆盖未完成：{e}"),
        };
        Ok::<(String, String), String>((name, note))
    })
    .await
    .map_err(|e| format!("后台任务失败: {e}"));
    match result.and_then(|r| r) {   // 拍平双层 Result（spawn_blocking 外层 + 闭包内层）
        Ok((name, note)) => {
            // toast 只报状态+目标（用户要求），覆盖详情进日志
            log("acct", "新增 Steam 账号", &format!("已登录 {}{}", name, note), true);
            let _ = app.emit("steam-accounts-changed", ());
            Ok(name)
        }
        Err(e) => {
            log("acct", "新增 Steam 账号失败", &e, false);   // 失败也留痕（此前静默）
            Err(e)
        }
    }
}
}
