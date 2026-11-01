//! 通过当前运行的 WeGame 登录窗口执行填写；不启动其他安装、不登录。
//! 密码仅发送给已验证的 Edit，不使用剪贴板或全局键盘输入。
//! Ok 表示操作结束且观察期未发现拒绝，并不证明安全组件内部密码完整。
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::System;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, POINT, RECT, WAIT_TIMEOUT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, String>;
static FILL_LOCK: Mutex<()> = Mutex::new(());
const LOST_TARGET: &str = "WeGame 窗口或焦点已变化，已停止填入；请检查当前内容后重试";
const REJECTED: &str = "WeGame 未正常接收密码输入，已停止填入；请先按 WeGame 提示恢复密码输入后重试";
const MAX_CHARS: usize = 128;

struct Budget(Instant);
impl Budget {
    fn new() -> Self { Self(Instant::now() + Duration::from_secs(5)) }
    fn check(&self) -> Result<()> {
        if Instant::now() >= self.0 { return Err("填入超时，已停止；请检查 WeGame 当前内容后重试".into()); }
        if unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) } < 0 { return Err("已取消填入，请检查 WeGame 当前内容".into()); }
        Ok(())
    }
    fn message_ms(&self) -> Result<u32> {
        self.check()?;
        Ok(self.0.saturating_duration_since(Instant::now()).as_millis().clamp(1, 150) as u32)
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

// 先选唯一进程/窗口结构，再恢复和检查几何。最小化时不依赖失真的客户区矩形。
fn discover(budget: &Budget) -> Result<Target> {
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
        if edits.len() == 2 && !accounts.is_empty() { candidates.push((root, root_pid, edits, accounts)); }
    }
    if candidates.len() != 1 { return Err(if candidates.is_empty() {
        "未识别到受支持的 WeGame 登录页，请切换到账号密码登录后重试"
    } else { "发现多个 WeGame 登录窗口，请只保留需要填入的那个窗口" }.into()); }
    let (root, root_pid, edits, accounts) = candidates.remove(0);
    let ids: BTreeSet<_> = [root_pid, pid(edits[0]), pid(edits[1])].into_iter().collect();
    let guards: Vec<_> = ids.into_iter().map(ProcessGuard::open).collect::<Result<_>>()?;
    restore(root, root_pid, &guards, budget)?;
    let bounds = client_bounds(root)?;
    let layout = Layout::new(bounds.width(), bounds.height()).ok_or("当前 WeGame 登录布局不受支持，未修改账号密码")?;
    let passwords: Vec<_> = edits.iter().copied().filter(|hwnd| child_bounds(root, *hwnd)
        .map(|r| layout.matches(r, layout.password())).unwrap_or(false)).collect();
    let accounts: Vec<_> = accounts.into_iter().filter(|hwnd| child_bounds(root, *hwnd)
        .map(|r| layout.matches(r, layout.client())).unwrap_or(false)).collect();
    if passwords.len() != 1 || accounts.len() != 1 { return Err("无法可靠区分 WeGame 账号与密码输入框，未修改内容".into()); }
    let password = passwords[0];
    let auxiliary = edits.into_iter().find(|hwnd| *hwnd != password).ok_or(LOST_TARGET)?;
    let target = Target { root, root_pid, account: accounts[0], password, auxiliary,
        password_pid: pid(password), processes: guards, layout };
    target.foreground()?;
    Ok(target)
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
            || !self.processes.iter().any(|p| p.pid == self.password_pid)
            || unsafe { GetAncestor(self.account, GA_ROOT) } != self.root
            || unsafe { GetAncestor(self.password, GA_ROOT) } != self.root
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
            if start.elapsed() >= Duration::from_millis(350) && changed.elapsed() >= Duration::from_millis(100) { return Ok(()); }
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
    fn observe(&self, field: Field, layout: HKL, budget: &Budget, duration: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            self.guard(field, layout, budget)?;
            if start.elapsed() >= duration { return Ok(()); }
            sleep(Duration::from_millis(10));
        }
    }
    fn password_message(&self, layout: HKL, budget: &Budget, message: u32, w: WPARAM, l: LPARAM) -> Result<()> {
        self.guard(Field::Password, layout, budget)?;
        modifiers_released()?;
        let mut response = 0_usize;
        let completed = unsafe { SendMessageTimeoutW(self.password, message, w, l,
            SMTO_BLOCK | SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT, budget.message_ms()?, Some(&mut response)) };
        if completed.0 == 0 { return Err("WeGame 密码控件未响应，已停止填入；请检查当前内容".into()); }
        // 消息返回值不表示安全组件已接收密码，必须继续观察真实焦点状态。
        self.guard(Field::Password, layout, budget)
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
#[derive(Clone, Copy)]
struct KeyEvent { key: Key, up: bool }
fn scan(vk: VIRTUAL_KEY, layout: HKL) -> Result<Key> {
    let code = unsafe { MapVirtualKeyExW(u32::from(vk.0), MAPVK_VK_TO_VSC_EX, layout) };
    if code == 0 || code & 0xff00 == 0xe100 { return Err("当前键盘布局不支持自动填入，请切换英文输入法后重试".into()); }
    Ok(Key { scan: (code & 0xff) as u16, extended: code & 0xff00 == 0xe000 })
}
fn keyboard(event: KeyEvent) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if event.up { flags |= KEYEVENTF_KEYUP; }
    if event.key.extended { flags |= KEYEVENTF_EXTENDEDKEY; }
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wScan: event.key.scan, dwFlags: flags, ..Default::default() } } }
}
fn shift_required(vk: u16, required: bool, caps: bool) -> bool { required ^ (caps && (0x41..=0x5a).contains(&vk)) }
fn append_key(events: &mut Vec<KeyEvent>, key: Key) { events.push(KeyEvent { key, up: false }); events.push(KeyEvent { key, up: true }); }
fn account_plan(value: &str, layout: HKL, caps: bool) -> Result<Vec<KeyEvent>> {
    let shift = scan(VK_LSHIFT, layout)?;
    let mut result = Vec::new();
    for ch in value.bytes() {
        let mapping = unsafe { VkKeyScanExW(u16::from(ch), layout) };
        if mapping == -1 || (mapping as u16 >> 8) & !1 != 0 { return Err("当前键盘布局无法安全输入账号，请切换英文输入法后重试".into()); }
        let vk = mapping as u16 & 0xff;
        let key = scan(VIRTUAL_KEY(vk), layout)?;
        let shifted = shift_required(vk, mapping as u16 & 0x100 != 0, caps);
        if shifted { result.push(KeyEvent { key: shift, up: false }); }
        append_key(&mut result, key);
        if shifted { result.push(KeyEvent { key: shift, up: true }); }
    }
    Ok(result)
}
fn held_after(events: &[KeyEvent], inserted: usize) -> BTreeSet<Key> {
    let mut held = BTreeSet::new();
    for event in events.iter().take(inserted) {
        if event.up { held.remove(&event.key); } else { held.insert(event.key); }
    }
    held
}
fn send_account_keys(target: &Target, layout: HKL, budget: &Budget, events: &[KeyEvent]) -> Result<()> {
    target.guard(Field::Account, layout, budget)?;
    modifiers_released()?;
    let input: Vec<_> = events.iter().copied().map(keyboard).collect();
    let inserted = unsafe { SendInput(&input, size_of::<INPUT>() as i32) } as usize;
    if inserted != input.len() {
        for key in held_after(events, inserted) { unsafe { SendInput(&[keyboard(KeyEvent { key, up: true })], size_of::<INPUT>() as i32); } }
        return Err("WeGame 未完整接收账号按键，已停止填入；请检查当前内容".into());
    }
    target.observe(Field::Account, layout, budget, Duration::from_millis(20))
}
fn char_flags(key: Key) -> LPARAM { LPARAM(1_isize | ((key.scan as isize) << 16) | if key.extended { 1_isize << 24 } else { 0 }) }

/// 在阻塞工作线程调用。Ok 只能在界面显示“已发送填入，请检查 WeGame”，不能声称密码核验成功。
pub fn fill_credentials(account: &str, password: &str) -> Result<()> {
    let _lock = FILL_LOCK.try_lock().map_err(|_| "正在填入 WeGame，请稍候".to_string())?;
    if !valid_text(account) || !valid_text(password) { return Err("自动填入支持 128 个以内的可打印英文、数字和符号，未修改输入框".into()); }
    let budget = Budget::new();
    budget.check()?;
    modifiers_released()?;
    let target = discover(&budget)?;
    target.activation_stable(&budget)?;
    let account_layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(target.account, None)) };
    let password_layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(target.password, None)) };
    let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0;
    // 所有字符和清空按键在修改输入框之前完成预检。
    let account_keys = account_plan(account, account_layout, caps)?;
    let control = scan(VK_LCONTROL, account_layout)?;
    let mut clear_keys = vec![KeyEvent { key: control, up: false }];
    append_key(&mut clear_keys, scan(VK_A, account_layout)?);
    clear_keys.push(KeyEvent { key: control, up: true });
    append_key(&mut clear_keys, scan(VK_BACK, account_layout)?);
    let backspace = char_flags(scan(VK_BACK, password_layout)?);
    let mut password_chars = Vec::new();
    for ch in password.bytes() {
        let mapping = unsafe { VkKeyScanExW(u16::from(ch), password_layout) };
        if mapping == -1 { return Err("当前键盘布局不支持密码中的字符，未修改输入框".into()); }
        password_chars.push((WPARAM(usize::from(ch)), char_flags(scan(VIRTUAL_KEY(mapping as u16 & 0xff), password_layout)?)));
    }
    target.click(Field::Account, account_layout, &budget)?;
    send_account_keys(&target, account_layout, &budget, &clear_keys)?;
    if (unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0) != caps { return Err("CapsLock 状态已变化，已停止填入，请检查当前内容".into()); }
    send_account_keys(&target, account_layout, &budget, &account_keys)?;
    // 先完成账号，再处理密码，避免账号变化异步重置已经填好的密码。
    target.click(Field::Password, password_layout, &budget)?;
    target.password_message(password_layout, &budget, EM_SETSEL, WPARAM(0), LPARAM(-1))?;
    target.password_message(password_layout, &budget, WM_CHAR, WPARAM(8), backspace)?;
    target.observe(Field::Password, password_layout, &budget, Duration::from_millis(120))?;
    // 每栏先执行清空再填写；任何拒绝/失焦都不重试或切换输入方法。
    for (ch, flags) in password_chars { target.password_message(password_layout, &budget, WM_CHAR, ch, flags)?; }
    target.observe(Field::Password, password_layout, &budget, Duration::from_millis(350))
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
        assert_eq!(scaled.point(Field::Password).x, 600);
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
    fn partial_key_batches_only_release_keys_that_were_actually_pressed() {
        let control = Key { scan: 0x1d, extended: false };
        let letter = Key { scan: 0x1e, extended: false };
        let events = [KeyEvent { key: control, up: false }, KeyEvent { key: letter, up: false },
            KeyEvent { key: letter, up: true }, KeyEvent { key: control, up: true }];
        assert!(held_after(&events, 0).is_empty());
        assert_eq!(held_after(&events, 1), BTreeSet::from([control]));
        assert_eq!(held_after(&events, 2), BTreeSet::from([control, letter]));
        assert_eq!(held_after(&events, 3), BTreeSet::from([control]));
        assert!(held_after(&events, 4).is_empty());
    }
    #[test]
    fn key_flags_and_caps_lock_keep_punctuation_literal() {
        assert_eq!(char_flags(Key { scan: 0x1e, extended: false }).0, 0x1e0001);
        assert_eq!(char_flags(Key { scan: 0x1e, extended: true }).0, 0x11e0001);
        assert!(shift_required(0x41, false, true));
        assert!(!shift_required(0x41, true, true));
        assert!(shift_required(0x31, true, true));
        assert_eq!(normalize(-1920, -1920, 3840), Some(0));
        assert_eq!(normalize(1919, -1920, 3840), Some(65535));
        assert_eq!(normalize(1920, -1920, 3840), None);
    }
}
