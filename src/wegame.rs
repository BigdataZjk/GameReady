//! 通过当前运行的 WeGame 登录窗口执行填写；不启动其他安装、不登录。
//! 受保护密码通过 FakerInput 虚拟 HID 键盘输入；缺组件时由界面引导安装。
//! 不读取密码控件、不使用剪贴板、不发送回车、不自动安装驱动。
use crate::wegame_hid::{self, Keyboard};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::System;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_TIMEOUT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Ime::{ImmGetContext, ImmGetOpenStatus, ImmReleaseContext};
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, String>;
static FILL_LOCK: Mutex<()> = Mutex::new(());
const LOST_TARGET: &str = "WeGame 窗口或焦点已变化，已停止填入；请检查当前内容后重试";
const REJECTED: &str = "WeGame 未正常接收密码输入，已停止填入；请先按 WeGame 提示恢复密码输入后重试";
const MAX_CHARS: usize = 128;
const KEY_HOLD: Duration = Duration::from_millis(4);
const KEY_GAP: Duration = Duration::from_millis(4);

struct Budget(Instant);
impl Budget {
    fn new() -> Self { Self(Instant::now() + Duration::from_secs(5)) }
    fn check(&self) -> Result<()> {
        if Instant::now() >= self.0 { return Err("填入超时，已停止；请检查 WeGame 当前内容后重试".into()); }
        if unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) } < 0 { return Err("已取消填入，请检查 WeGame 当前内容".into()); }
        Ok(())
    }
}

#[derive(Clone)]
struct ProcessInfo { parent: Option<u32>, name: String }
fn family_root(processes: &BTreeMap<u32, ProcessInfo>, pid: u32) -> Option<u32> {
    let mut current = Some(pid);
    let mut visited = BTreeSet::new();
    let mut root = None;
    while let Some(id) = current {
        if !visited.insert(id) { return None; }
        let Some(process) = processes.get(&id) else { break; };
        if process.name.eq_ignore_ascii_case("wegame.exe") { root = Some(id); }
        current = process.parent;
    }
    root
}
struct ProcessGuard { pid: u32, handle: HANDLE }
impl ProcessGuard {
    fn open(pid: u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(|_| "无法访问当前 WeGame，请确认两程序使用相同权限运行".to_string())?;
        Ok(Self { pid, handle })
    }
    fn alive(&self) -> bool { unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT } }
}
impl Drop for ProcessGuard { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.handle); } } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds { left: i32, top: i32, right: i32, bottom: i32 }
impl From<RECT> for Bounds {
    fn from(r: RECT) -> Self { Self { left: r.left, top: r.top, right: r.right, bottom: r.bottom } }
}
impl Bounds {
    fn width(self) -> i32 { self.right - self.left }
    fn height(self) -> i32 { self.bottom - self.top }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field { Account, Password }
#[derive(Clone, Copy, Debug)]
struct Layout { width: i32, height: i32 }
impl Layout {
    fn new(width: i32, height: i32) -> Option<Self> {
        if !(640..=2880).contains(&width) || !(360..=1620).contains(&height)
            || (i64::from(width) * 540 - i64::from(height) * 960).abs() > i64::from(width) * 3 { return None; }
        Some(Self { width, height })
    }
    fn x(self, n: i32) -> i32 { (i64::from(n) * i64::from(self.width) / 960) as i32 }
    fn y(self, n: i32) -> i32 { (i64::from(n) * i64::from(self.height) / 540) as i32 }
    fn point(self, field: Field) -> POINT { POINT { x: self.x(480), y: self.y(if field == Field::Account { 332 } else { 373 }) } }
    fn password(self) -> Bounds { Bounds { left: self.x(410), top: self.y(361), right: self.x(550), bottom: self.y(385) } }
    fn client(self) -> Bounds { Bounds { left: 0, top: 0, right: self.width, bottom: self.height } }
    fn matches(self, actual: Bounds, expected: Bounds) -> bool {
        let d = self.x(5).max(3);
        (actual.left - expected.left).abs() <= d && (actual.top - expected.top).abs() <= d
            && (actual.right - expected.right).abs() <= d && (actual.bottom - expected.bottom).abs() <= d
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0_u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..len])
}
fn pid(hwnd: HWND) -> u32 {
    let mut id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut id)); }
    id
}
fn client_bounds(hwnd: HWND) -> Result<Bounds> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.map_err(|_| LOST_TARGET.to_string())?;
    Ok(rect.into())
}
fn child_bounds(root: HWND, child: HWND) -> Result<Bounds> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(child, &mut rect) }.map_err(|_| LOST_TARGET.to_string())?;
    let mut start = POINT { x: rect.left, y: rect.top };
    let mut end = POINT { x: rect.right, y: rect.bottom };
    if !unsafe { ScreenToClient(root, &mut start).as_bool() && ScreenToClient(root, &mut end).as_bool() } { return Err(LOST_TARGET.into()); }
    Ok(Bounds { left: start.x, top: start.y, right: end.x, bottom: end.y })
}
unsafe extern "system" fn collect_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    (&mut *(parameter.0 as *mut Vec<HWND>)).push(hwnd);
    BOOL(1)
}
fn children(hwnd: HWND) -> Vec<HWND> {
    let mut result = Vec::new();
    unsafe { let _ = EnumChildWindows(hwnd, Some(collect_window), LPARAM(&mut result as *mut Vec<HWND> as isize)); }
    result
}
fn current_focus() -> Result<HWND> {
    let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    unsafe { GetGUIThreadInfo(0, &mut gui) }.map_err(|_| LOST_TARGET.to_string())?;
    Ok(gui.hwndFocus)
}

struct Target {
    root: HWND, root_pid: u32, account: HWND, password: HWND, auxiliary: HWND,
    password_pid: u32, processes: Vec<ProcessGuard>, layout: Layout,
}
struct Candidate {
    root: HWND, root_pid: u32, edits: Vec<HWND>, account: HWND, processes: Vec<ProcessGuard>,
}

// 只读选择唯一进程/窗口结构。驱动、布局和全部字符通过预检后才恢复。
fn discover(budget: &Budget) -> Result<Candidate> {
    let mut system = System::new();
    system.refresh_processes();
    let processes: BTreeMap<_, _> = system.processes().iter().map(|(id, process)| (id.as_u32(), ProcessInfo {
        parent: process.parent().map(|value| value.as_u32()), name: process.name().into(),
    })).collect();
    if !processes.values().any(|process| process.name.eq_ignore_ascii_case("wegame.exe")) {
        return Err("请先打开 WeGame 的账号密码登录页面".into());
    }
    let mut windows = Vec::new();
    unsafe { EnumWindows(Some(collect_window), LPARAM(&mut windows as *mut Vec<HWND> as isize)) }
        .map_err(|_| "无法查找 WeGame 登录窗口".to_string())?;
    let mut candidates = Vec::new();
    for root in windows {
        budget.check()?;
        let root_pid = pid(root);
        if !processes.get(&root_pid).map(|p| p.name.eq_ignore_ascii_case("wegame.exe")).unwrap_or(false)
            || !class_name(root).eq_ignore_ascii_case("TWINCONTROL") { continue; }
        let Some(family) = family_root(&processes, root_pid) else { continue; };
        let descendants = children(root);
        let edits: Vec<_> = descendants.iter().copied().filter(|hwnd| {
            let id = pid(*hwnd);
            processes.get(&id).map(|p| p.name.eq_ignore_ascii_case("TASLogin.exe")).unwrap_or(false)
                && family_root(&processes, id) == Some(family) && class_name(*hwnd).eq_ignore_ascii_case("Edit")
        }).collect();
        let accounts: Vec<_> = descendants.iter().copied().filter(|hwnd| pid(*hwnd) == root_pid
            && class_name(*hwnd).eq_ignore_ascii_case("TWINCONTROL")).collect();
        if edits.len() == 2 && accounts.len() == 1 { candidates.push((root, root_pid, edits, accounts[0])); }
    }
    if candidates.len() != 1 { return Err(if candidates.is_empty() {
        "未识别到受支持的 WeGame 登录页，请切换到账号密码登录后重试"
    } else { "发现多个 WeGame 登录窗口，请只保留需要填入的那个窗口" }.into()); }
    let (root, root_pid, edits, account) = candidates.remove(0);
    let ids: BTreeSet<_> = [root_pid, pid(edits[0]), pid(edits[1])].into_iter().collect();
    let processes: Vec<_> = ids.into_iter().map(ProcessGuard::open).collect::<Result<_>>()?;
    let candidate = Candidate { root, root_pid, edits, account, processes };
    candidate.validate()?;
    Ok(candidate)
}
impl Candidate {
    fn validate(&self) -> Result<()> {
        if self.processes.iter().any(|p| !p.alive()) || pid(self.root) != self.root_pid
            || pid(self.account) != self.root_pid
            || !class_name(self.root).eq_ignore_ascii_case("TWINCONTROL")
            || !class_name(self.account).eq_ignore_ascii_case("TWINCONTROL")
            || unsafe { GetAncestor(self.account, GA_ROOT) } != self.root
            || self.edits.iter().any(|hwnd| !class_name(*hwnd).eq_ignore_ascii_case("Edit")
                || unsafe { GetAncestor(*hwnd, GA_ROOT) } != self.root
                || !self.processes.iter().any(|p| p.pid == pid(*hwnd))) { return Err(LOST_TARGET.into()); }
        Ok(())
    }
    fn layouts(&self) -> Result<(HKL, HKL)> {
        self.validate()?;
        let account = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(self.account, None)) };
        let password = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(self.edits[0], None)) };
        if self.edits.iter().any(|hwnd| unsafe { GetKeyboardLayout(GetWindowThreadProcessId(*hwnd, None)) } != password) {
            return Err("密码控件键盘布局不一致，未修改输入框".into());
        }
        Ok((account, password))
    }
    fn activate(self, budget: &Budget) -> Result<Target> {
        self.validate()?;
        restore(self.root, self.root_pid, &self.processes, budget)?;
        self.validate()?;
        let bounds = client_bounds(self.root)?;
        let layout = Layout::new(bounds.width(), bounds.height()).ok_or("当前 WeGame 登录布局不受支持，未修改账号密码")?;
        let passwords: Vec<_> = self.edits.iter().copied().filter(|hwnd| child_bounds(self.root, *hwnd)
            .map(|r| layout.matches(r, layout.password())).unwrap_or(false)).collect();
        if passwords.len() != 1 || !layout.matches(child_bounds(self.root, self.account)?, layout.client()) {
            return Err("无法可靠区分 WeGame 账号与密码输入框，未修改内容".into());
        }
        let password = passwords[0];
        let auxiliary = self.edits.into_iter().find(|hwnd| *hwnd != password).ok_or(LOST_TARGET)?;
        let target = Target { root: self.root, root_pid: self.root_pid, account: self.account, password, auxiliary,
            password_pid: pid(password), processes: self.processes, layout };
        target.foreground()?;
        Ok(target)
    }
}

fn wait_for_window(root: HWND, root_pid: u32, processes: &[ProcessGuard], budget: &Budget, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    loop {
        budget.check()?;
        if pid(root) != root_pid || processes.iter().any(|p| !p.alive()) { return Err(LOST_TARGET.into()); }
        if ready()? { return Ok(()); }
        sleep(Duration::from_millis(10));
    }
}
fn restore(root: HWND, root_pid: u32, processes: &[ProcessGuard], budget: &Budget) -> Result<()> {
    budget.check()?;
    if pid(root) != root_pid || processes.iter().any(|p| !p.alive()) { return Err(LOST_TARGET.into()); }
    if !unsafe { ShowWindowAsync(root, if IsIconic(root).as_bool() { SW_RESTORE } else { SW_SHOW }).as_bool() } {
        return Err("无法请求恢复 WeGame 窗口，未修改账号密码".into());
    }
    wait_for_window(root, root_pid, processes, budget, || {
        let bounds = client_bounds(root)?;
        Ok(unsafe { IsWindowVisible(root).as_bool() && !IsIconic(root).as_bool() }
            && bounds.width() > 0 && bounds.height() > 0)
    })?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(root, &mut rect) }.map_err(|_| LOST_TARGET.to_string())?;
    let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if unsafe { GetMonitorInfoW(MonitorFromWindow(root, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool() } {
        let work = Bounds::from(monitor.rcWork);
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width > work.width() || height > work.height() { return Err("WeGame 窗口超出显示器范围，请调整显示缩放后重试".into()); }
        let x = rect.left.clamp(work.left, work.right - width);
        let y = rect.top.clamp(work.top, work.bottom - height);
        if x != rect.left || y != rect.top {
            unsafe { SetWindowPos(root, HWND::default(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS) }
                .map_err(|_| "无法恢复 WeGame 窗口位置".to_string())?;
            wait_for_window(root, root_pid, processes, budget, || {
                let mut current = RECT::default();
                unsafe { GetWindowRect(root, &mut current) }.map_err(|_| LOST_TARGET.to_string())?;
                Ok(current.left == x && current.top == y)
            })?;
        }
    }
    unsafe { let _ = SetForegroundWindow(root); }
    for _ in 0..20 {
        budget.check()?;
        if unsafe { GetForegroundWindow() } == root { return Ok(()); }
        sleep(Duration::from_millis(10));
    }
    Err("无法将 WeGame 切到前台，请先点击 WeGame 窗口后重试".into())
}

impl Target {
    fn validate(&self) -> Result<()> {
        if self.processes.iter().any(|p| !p.alive()) || pid(self.root) != self.root_pid
            || pid(self.account) != self.root_pid || pid(self.password) != self.password_pid
            || pid(self.auxiliary) != self.password_pid
            || !self.processes.iter().any(|p| p.pid == self.password_pid)
            || unsafe { GetAncestor(self.account, GA_ROOT) } != self.root
            || unsafe { GetAncestor(self.password, GA_ROOT) } != self.root
            || unsafe { GetAncestor(self.auxiliary, GA_ROOT) } != self.root
            || !class_name(self.account).eq_ignore_ascii_case("TWINCONTROL")
            || !class_name(self.password).eq_ignore_ascii_case("Edit")
            || client_bounds(self.root)? != self.layout.client()
            || !self.layout.matches(child_bounds(self.root, self.account)?, self.layout.client())
            || !self.layout.matches(child_bounds(self.root, self.password)?, self.layout.password()) { return Err(LOST_TARGET.into()); }
        Ok(())
    }
    fn foreground(&self) -> Result<()> {
        self.validate()?;
        if unsafe { GetForegroundWindow() } != self.root { return Err(LOST_TARGET.into()); }
        Ok(())
    }
    fn element(&self, field: Field) -> HWND { if field == Field::Account { self.account } else { self.password } }
    fn screen_point(&self, field: Field) -> Result<POINT> {
        let mut point = self.layout.point(field);
        if !unsafe { ClientToScreen(self.root, &mut point).as_bool() } { return Err(LOST_TARGET.into()); }
        Ok(point)
    }
    fn guard(&self, field: Field, layout: HKL, budget: &Budget) -> Result<()> {
        budget.check()?;
        self.foreground()?;
        let focused = current_focus()?;
        if focused == self.auxiliary { return Err(REJECTED.into()); }
        if focused != self.element(field) { return Err(LOST_TARGET.into()); }
        let thread = unsafe { GetWindowThreadProcessId(focused, None) };
        if unsafe { GetKeyboardLayout(thread) } != layout { return Err("输入法布局已变化，已停止填入，请检查内容后重试".into()); }
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor) }.map_err(|_| LOST_TARGET.to_string())?;
        let expected = self.screen_point(field)?;
        if (cursor.x - expected.x).abs() > 3 || (cursor.y - expected.y).abs() > 3 {
            return Err("检测到鼠标或窗口移动，已停止填入，请检查内容后重试".into());
        }
        Ok(())
    }
    fn activation_stable(&self, budget: &Budget) -> Result<()> {
        let start = Instant::now();
        let mut last_focus = current_focus()?;
        let mut changed = start;
        loop {
            budget.check()?;
            self.foreground()?;
            let focus = current_focus()?;
            if !focus.is_invalid() && unsafe { GetAncestor(focus, GA_ROOT) } != self.root { return Err(LOST_TARGET.into()); }
            if focus != last_focus { changed = Instant::now(); last_focus = focus; }
            // 激活约 330ms 后客户端可能把焦点交给 CEF，先等此过程结束。
            if start.elapsed() >= Duration::from_millis(380) && changed.elapsed() >= Duration::from_millis(40) { return Ok(()); }
            if start.elapsed() >= Duration::from_millis(900) { return Err("WeGame 登录页焦点尚未稳定，请稍后重试".into()); }
            sleep(Duration::from_millis(10));
        }
    }
    fn click(&self, field: Field, layout: HKL, budget: &Budget) -> Result<()> {
        budget.check()?;
        self.foreground()?;
        modifiers_released()?;
        // 辅助 Edit 是已观察到的拒绝状态，不通过重新聚焦继续尝试。
        if current_focus()? == self.auxiliary { return Err(REJECTED.into()); }
        let point = self.screen_point(field)?;
        if unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) } != self.root { return Err("WeGame 输入框被其他窗口遮挡，请关闭遮挡后重试".into()); }
        let x = normalize(point.x, unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) }, unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) });
        let y = normalize(point.y, unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) }, unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) });
        let (x, y) = x.zip(y).ok_or("WeGame 输入框不在可操作的屏幕范围内")?;
        let events = [mouse(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
            mouse(0, 0, MOUSEEVENTF_LEFTDOWN), mouse(0, 0, MOUSEEVENTF_LEFTUP)];
        let inserted = unsafe { SendInput(&events, size_of::<INPUT>() as i32) };
        if inserted != events.len() as u32 {
            if inserted >= 2 { unsafe { SendInput(&[mouse(0, 0, MOUSEEVENTF_LEFTUP)], size_of::<INPUT>() as i32); } }
            return Err("无法完整点击 WeGame 输入框，已停止填入".into());
        }
        for _ in 0..15 {
            budget.check()?;
            self.foreground()?;
            let focus = current_focus()?;
            if focus == self.auxiliary { return Err(REJECTED.into()); }
            if focus == self.element(field) { return self.guard(field, layout, budget); }
            sleep(Duration::from_millis(10));
        }
        Err("无法确认 WeGame 输入框焦点，已停止填入".into())
    }
    fn observe(&self, field: Field, plan: &HidPlan, budget: &Budget, duration: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            self.guard(field, plan.layout, budget)?;
            if (unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0) != plan.caps {
                return Err("CapsLock 状态已变化，已停止填入，请检查当前内容".into());
            }
            ime_closed(self.element(field))?;
            if start.elapsed() >= duration { return Ok(()); }
            sleep(Duration::from_millis(4).min(duration.saturating_sub(start.elapsed())));
        }
    }
}

fn valid_text(value: &str) -> bool { !value.is_empty() && value.len() <= MAX_CHARS && value.bytes().all(|b| (0x20..=0x7e).contains(&b)) }
fn modifiers_released() -> Result<()> {
    if [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN].iter().any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0) {
        Err("请松开 Ctrl、Shift、Alt、Win 键后重新填入".into())
    } else { Ok(()) }
}
fn normalize(value: i32, origin: i32, extent: i32) -> Option<i32> {
    if extent <= 1 || value < origin || i64::from(value) >= i64::from(origin) + i64::from(extent) { return None; }
    Some(((i64::from(value) - i64::from(origin)) * 65535 / i64::from(extent - 1)) as i32)
}
fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: x, dy: y, dwFlags: flags, ..Default::default() } } }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key { scan: u16, extended: bool }
fn scan(vk: VIRTUAL_KEY, layout: HKL) -> Result<Key> {
    let code = unsafe { MapVirtualKeyExW(u32::from(vk.0), MAPVK_VK_TO_VSC_EX, layout) };
    if code == 0 || code & 0xff00 == 0xe100 { return Err("当前键盘布局不支持自动填入，请切换英文输入法后重试".into()); }
    Ok(Key { scan: (code & 0xff) as u16, extended: code & 0xff00 == 0xe000 })
}
fn shift_required(vk: u16, required: bool, caps: bool) -> bool { required ^ (caps && (0x41..=0x5a).contains(&vk)) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HidKey { modifiers: u8, usage: u8 }
struct HidPlan { layout: HKL, caps: bool, keys: Vec<HidKey>, clear: [HidKey; 2] }

fn prepare(value: &str, layout: HKL) -> Result<HidPlan> {
    if !valid_text(value) || layout.0.is_null() { return Err("字符或键盘布局不受支持，未修改输入框".into()); }
    let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0;
    let mut keys = Vec::new();
    for ch in value.bytes() {
        let mapping = unsafe { VkKeyScanExW(u16::from(ch), layout) };
        if mapping == -1 || (mapping as u16 >> 8) & !1 != 0 { return Err("当前键盘布局无法安全输入账号密码，请切换英文输入法后重试".into()); }
        let vk = mapping as u16 & 0xff;
        let key = scan(VIRTUAL_KEY(vk), layout)?;
        let shifted = shift_required(vk, mapping as u16 & 0x100 != 0, caps);
        let mut state = [0_u8; 256];
        state[VK_CAPITAL.0 as usize] = u8::from(caps);
        if shifted { state[VK_SHIFT.0 as usize] = 0x80; }
        let mut output = [0_u16; 8];
        // Bit 2 prevents ToUnicodeEx from changing the keyboard dead-key state.
        let count = unsafe { ToUnicodeEx(u32::from(vk), u32::from(key.scan), &state, &mut output, 4, layout) };
        if count != 1 || output[0] != u16::from(ch) { return Err("字符无法在目标键盘布局中原样输入，未修改输入框".into()); }
        keys.push(HidKey { modifiers: if shifted { 2 } else { 0 }, usage: wegame_hid::usage(key.scan, key.extended)? });
    }
    let a = scan(VK_A, layout)?;
    let back = scan(VK_BACK, layout)?;
    let a = wegame_hid::usage(a.scan, a.extended)?;
    let back = wegame_hid::usage(back.scan, back.extended)?;
    if a != 0x04 || back != 0x2a { return Err("当前键盘布局的清空组合尚未适配，未修改输入框".into()); }
    Ok(HidPlan { layout, caps, keys, clear: [HidKey { modifiers: 1, usage: a }, HidKey { modifiers: 0, usage: back }] })
}

fn ime_closed(hwnd: HWND) -> Result<()> {
    let context = unsafe { ImmGetContext(hwnd) };
    if context.0.is_null() { return Ok(()); }
    let open = unsafe { ImmGetOpenStatus(context).as_bool() };
    unsafe { let _ = ImmReleaseContext(hwnd, context); }
    if open { Err("目标输入法处于组合输入模式，请切到英文后重新填入".into()) } else { Ok(()) }
}

trait Sink { fn state(&mut self, modifiers: u8, key: u8) -> Result<()>; }
impl Sink for Keyboard { fn state(&mut self, modifiers: u8, key: u8) -> Result<()> { self.state(modifiers, key) } }
fn press_guarded(sink: &mut impl Sink, key: HidKey, mut wait: impl FnMut(Duration) -> Result<()>) -> Result<()> {
    wait(Duration::ZERO)?;
    let down = sink.state(key.modifiers, key.usage);
    let held = down.and_then(|_| wait(KEY_HOLD));
    // Even failed writes may have reached the device. Always release, including
    // after cancellation/focus loss, before propagating the original error.
    sink.state(0, 0).map_err(|error| format!("{error}；松键未确认，请停止填入并检查键盘状态"))?;
    held?;
    wait(KEY_GAP)
}
fn fill_field(device: &mut Keyboard, target: &Target, field: Field, plan: &HidPlan, budget: &Budget) -> Result<()> {
    target.click(field, plan.layout, budget)?;
    let mut wait = |time| target.observe(field, plan, budget, time);
    wait(Duration::from_millis(20))?;
    for key in plan.clear.iter().chain(&plan.keys) {
        modifiers_released()?;
        press_guarded(device, *key, &mut wait)?;
    }
    wait(Duration::from_millis(if field == Field::Password { 100 } else { 40 }))
}
fn with_keyboard<K>(open: impl FnOnce() -> Result<Option<K>>, work: impl FnOnce(K) -> Result<FillOutcome>) -> Result<FillOutcome> {
    match open()? {
        Some(device) => work(device),
        None => Ok(FillOutcome::DriverRequired),
    }
}
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillOutcome { InputSent, DriverRequired }

fn valid_account(account: &str) -> bool { (5..=12).contains(&account.len()) && account.bytes().all(|b| b.is_ascii_digit()) }

/// Blocking worker only. The user must check the client and initiate login.
pub fn fill_credentials(account: &str, password: &str) -> Result<FillOutcome> {
    let _lock = FILL_LOCK.try_lock().map_err(|_| "正在填入 WeGame，请稍候".to_string())?;
    let _component_lock = crate::wegame_component::lock_operation()?;
    if !valid_account(account) { return Err("当前已适配 WeGame 的 QQ 数字账号登录页，需要 5–12 位数字账号；未修改输入框".into()); }
    if !valid_text(password) { return Err("自动填入支持 128 个以内的可打印英文、数字和符号密码，未修改输入框".into()); }
    let budget = Budget::new();
    budget.check()?;
    // Missing/busy drivers fail before discovering or restoring any window.
    with_keyboard(Keyboard::open, |mut device| {
        modifiers_released()?;
        let candidate = discover(&budget)?;
        let (account_layout, password_layout) = candidate.layouts()?;
        let account_plan = prepare(account, account_layout)?;
        let password_plan = prepare(password, password_layout)?;
        ime_closed(candidate.account)?;
        for edit in &candidate.edits { ime_closed(*edit)?; }
        if candidate.layouts()? != (account_layout, password_layout) { return Err("键盘布局已变化，未修改输入框".into()); }
        let target = candidate.activate(&budget)?;
        target.activation_stable(&budget)?;
        fill_field(&mut device, &target, Field::Account, &account_plan, &budget)?;
        fill_field(&mut device, &target, Field::Password, &password_plan, &budget)?;
        device.release()?;
        Ok(FillOutcome::InputSent)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn process(name: &str, parent: Option<u32>) -> ProcessInfo { ProcessInfo { parent, name: name.into() } }
    #[test]
    fn separate_clients_and_cyclic_process_graphs_never_merge() {
        let processes = BTreeMap::from([(1, process("WeGame.exe", None)), (2, process("helper.exe", Some(1))),
            (3, process("TASLogin.exe", Some(2))), (10, process("wegame.exe", None)), (11, process("TASLogin.exe", Some(10))),
            (20, process("wegame.exe", Some(21))), (21, process("helper.exe", Some(20)))]);
        assert_eq!(family_root(&processes, 3), Some(1));
        assert_eq!(family_root(&processes, 11), Some(10));
        assert_eq!(family_root(&processes, 20), None);
        assert_eq!(family_root(&processes, 99), None);
    }
    #[test]
    fn login_profile_scales_and_rejects_unrelated_or_auxiliary_rectangles() {
        let normal = Layout::new(960, 540).unwrap();
        let scaled = Layout::new(1200, 675).unwrap();
        assert_eq!(scaled.point(Field::Account).x, 600);
        assert_eq!(scaled.point(Field::Account).y, 415);
        assert_eq!(scaled.point(Field::Password).y, 466);
        assert!(Layout::new(1200, 800).is_none());
        assert!(Layout::new(0, 0).is_none());
        assert!(!normal.matches(Bounds { left: 410, top: 102, right: 550, bottom: 102 }, normal.password()));
    }
    #[test]
    fn control_characters_and_oversized_credentials_are_rejected_before_input() {
        assert!(valid_text("Example!24"));
        for invalid in ["", "demo\n", "demo\r", "demo\t", "demo\u{1b}", "demo\0", "示例"] { assert!(!valid_text(invalid)); }
        assert!(!valid_text(&"a".repeat(MAX_CHARS + 1)));
    }
    #[test]
    fn unsupported_accounts_fail_before_discovery_or_input() {
        assert!(valid_account("1234567890"));
        for invalid in ["", "1234", "1234567890123", "dummy", "12345a", "12345\n"] { assert!(!valid_account(invalid)); }
    }
    #[test]
    fn caps_lock_and_virtual_desktop_mapping_keep_literal_input() {
        assert!(shift_required(0x41, false, true));
        assert!(!shift_required(0x41, true, true));
        assert!(shift_required(0x31, true, true));
        assert_eq!(normalize(-1920, -1920, 3840), Some(0));
        assert_eq!(normalize(1919, -1920, 3840), Some(65535));
        assert_eq!(normalize(1920, -1920, 3840), None);
    }
    #[test]
    fn missing_keyboard_never_reaches_target_or_clear() {
        let mut touched = false;
        let result = with_keyboard(|| Ok(None::<()>), |_| { touched = true; Ok(FillOutcome::InputSent) });
        assert_eq!(result.unwrap(), FillOutcome::DriverRequired);
        assert!(!touched);
        let result = with_keyboard(|| Err::<Option<()>, _>("fixture busy device".into()), |_| { touched = true; Ok(FillOutcome::InputSent) });
        assert!(result.is_err());
        assert!(!touched);
    }
    #[derive(Default)]
    struct FakeSink { reports: Vec<(u8, u8)>, fail_down: bool, fail_release: bool }
    impl Sink for FakeSink {
        fn state(&mut self, modifiers: u8, key: u8) -> Result<()> {
            self.reports.push((modifiers, key));
            if (key != 0 && self.fail_down) || (key == 0 && self.fail_release) { Err("fixture I/O error".into()) } else { Ok(()) }
        }
    }
    #[test]
    fn cancellation_and_focus_loss_always_release_pressed_keys() {
        let mut sink = FakeSink::default();
        let result = press_guarded(&mut sink, HidKey { modifiers: 2, usage: 4 }, |time|
            if time == KEY_HOLD { Err("fixture focus lost".into()) } else { Ok(()) });
        assert!(result.is_err());
        assert_eq!(sink.reports, [(2, 4), (0, 0)]);
    }
    #[test]
    fn uncertain_down_writes_release_and_release_errors_are_not_success() {
        for (fail_down, fail_release) in [(true, false), (false, true)] {
            let mut sink = FakeSink { fail_down, fail_release, ..Default::default() };
            assert!(press_guarded(&mut sink, HidKey { modifiers: 0, usage: 4 }, |_| Ok(())).is_err());
            assert_eq!(sink.reports, [(0, 4), (0, 0)]);
        }
    }
    #[test]
    fn guard_failure_before_press_sends_nothing() {
        let mut sink = FakeSink::default();
        assert!(press_guarded(&mut sink, HidKey { modifiers: 0, usage: 4 }, |_| Err("fixture cancelled".into())).is_err());
        assert!(sink.reports.is_empty());
    }
    #[test]
    fn printable_fixture_plans_are_complete_before_input() {
        let layout = unsafe { GetKeyboardLayout(0) };
        for text in ["1234567890", "a7B!", "GrTest!24", "GrMix!24A?"] {
            let plan = prepare(text, layout).unwrap();
            assert_eq!(plan.keys.len(), text.len());
            assert_eq!(plan.clear, [HidKey { modifiers: 1, usage: 4 }, HidKey { modifiers: 0, usage: 0x2a }]);
        }
        for text in ["", "test\r", "test\n", "test\t", "测试"] { assert!(prepare(text, layout).is_err()); }
    }
}
