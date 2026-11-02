//! Optional, explicitly installed FakerInput 0.1.x virtual keyboard.
//! Only its vendor control collection is opened. No driver installation,
//! client patching, hooks, credential reads, or mouse reports live here.
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::*;
use windows::Win32::System::Threading::*;

type Result<T> = std::result::Result<T, String>;
const REPORT_SIZE: usize = 65;
static IO_QUARANTINED: AtomicBool = AtomicBool::new(false);

struct Handle(HANDLE);
impl Drop for Handle { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } } }
struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet { fn drop(&mut self) { unsafe { let _ = SetupDiDestroyDeviceInfoList(self.0); } } }
struct Preparsed(PHIDP_PREPARSED_DATA);
impl Drop for Preparsed { fn drop(&mut self) { unsafe { HidD_FreePreparsedData(self.0); } } }

fn expected_root(mut node: u32) -> bool {
    for _ in 0..8 {
        let mut id = [0_u16; 512];
        if unsafe { CM_Get_Device_IDW(node, &mut id, 0) } != CR_SUCCESS { return false; }
        let len = id.iter().position(|c| *c == 0).unwrap_or(id.len());
        let instance = String::from_utf16_lossy(&id[..len]);
        if instance.to_ascii_uppercase().starts_with("ROOT\\") {
            // SetupAPI creates ROOT\SYSTEM\nnnn instances for this INF. The
            // stable identity is the hardware-ID MULTI_SZ, not the instance path.
            let mut hardware_ids = [0_u16; 2048];
            let mut bytes = size_of_val(&hardware_ids) as u32;
            let mut property_type = 0;
            const CM_DRP_HARDWAREID_VALUE: u32 = 2;
            const REG_MULTI_SZ_VALUE: u32 = 7;
            if unsafe { CM_Get_DevNode_Registry_PropertyW(node, CM_DRP_HARDWAREID_VALUE,
                Some(&mut property_type), Some(hardware_ids.as_mut_ptr().cast()), &mut bytes, 0) } == CR_SUCCESS
                && property_type == REG_MULTI_SZ_VALUE && bytes % 2 == 0 && bytes >= 4
                && bytes as usize <= size_of_val(&hardware_ids)
                && root_hardware_id_matches(&instance, &hardware_ids[..bytes as usize / 2]) { return true; }
        }
        let mut parent = 0;
        if unsafe { CM_Get_Parent(&mut parent, node, 0) } != CR_SUCCESS || parent == node { return false; }
        node = parent;
    }
    false
}

fn root_hardware_id_matches(instance: &str, ids: &[u16]) -> bool {
    instance.to_ascii_uppercase().starts_with("ROOT\\") && ids.ends_with(&[0, 0])
        && ids.split(|c| *c == 0).filter(|part| !part.is_empty()).any(|part|
            String::from_utf16_lossy(part).eq_ignore_ascii_case("root\\FakerInput"))
}

fn expected_collection(handle: HANDLE) -> bool {
    let mut attr = HIDD_ATTRIBUTES { Size: size_of::<HIDD_ATTRIBUTES>() as u32, ..Default::default() };
    if !unsafe { HidD_GetAttributes(handle, &mut attr).as_bool() }
        || attr.VendorID != 0xfe0f || attr.ProductID != 0x00ff || attr.VersionNumber != 2 {
        return false;
    }
    let mut data = PHIDP_PREPARSED_DATA::default();
    if !unsafe { HidD_GetPreparsedData(handle, &mut data).as_bool() } { return false; }
    let data = Preparsed(data);
    let mut caps = HIDP_CAPS::default();
    unsafe { HidP_GetCaps(data.0, &mut caps) }.is_ok()
        && caps.UsagePage == 0xff00 && caps.Usage == 1
        && caps.OutputReportByteLength as usize == REPORT_SIZE
}

// A request owns every resource Windows can still access, including the device.
// The Box is never moved or freed while overlapped I/O is outstanding.
struct WriteRequest {
    device: Handle,
    bytes: [u8; REPORT_SIZE],
    overlapped: OVERLAPPED,
    _event: Handle,
}
pub struct Keyboard { handle: Option<Handle>, touched: bool }
impl Keyboard {
    /// Enumeration and identity checks only: this never sends a key.
    pub fn open() -> Result<Option<Self>> {
        if IO_QUARANTINED.load(Ordering::Acquire) {
            return Err("上次 HID 写入未及时结束，本进程已禁止继续输入；请检查设备并重启 GameReady".into());
        }
        let guid = unsafe { HidD_GetHidGuid() };
        let set = DeviceSet(unsafe { SetupDiGetClassDevsW(Some(&guid), PCWSTR::null(), None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) }.map_err(|_| "无法枚举 HID 输入设备".to_string())?);
        let mut paths = Vec::new();
        let mut root_seen = false;
        for index in 0..=1024 {
            if index == 1024 { return Err("HID 设备枚举超出上限，未发送输入".into()); }
            let mut interface = SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            if let Err(error) = unsafe { SetupDiEnumDeviceInterfaces(set.0, None, &guid, index, &mut interface) } {
                if error.code() == HRESULT::from_win32(ERROR_NO_MORE_ITEMS.0) { break; }
                return Err("HID 设备枚举失败，未发送输入".into());
            }
            let mut required = 0;
            unsafe { let _ = SetupDiGetDeviceInterfaceDetailW(set.0, &interface, None, 0, Some(&mut required), None); }
            if required < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 || required > 65536 { continue; }
            // u64 storage provides the alignment required by the x64 structure.
            let mut buffer = vec![0_u64; (required as usize + 7) / 8];
            let detail = buffer.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32; }
            let mut info = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if unsafe { SetupDiGetDeviceInterfaceDetailW(set.0, &interface, Some(detail), required,
                None, Some(&mut info)) }.is_err() || !expected_root(info.DevInst) { continue; }
            root_seen = true;
            let path = unsafe { PCWSTR((*detail).DevicePath.as_ptr()) };
            let handle = unsafe { CreateFileW(path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE,
                None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None) }
                .map_err(|_| "虚拟键盘接口不可用或被占用，请关闭其他输入工具后重试".to_string())?;
            let handle = Handle(handle);
            if expected_collection(handle.0) {
                let path = unsafe { path.to_string() }.map_err(|_| "HID 设备路径无效".to_string())?;
                paths.push(path);
            }
        }
        if paths.is_empty() {
            return if root_seen { Err("已检测到 FakerInput，但控制接口不兼容或尚未就绪，请检查设备或重启 Windows".into()) } else { Ok(None) };
        }
        if paths.len() != 1 { return Err("检测到多个虚拟键盘控制端，未发送输入".into()); }
        let path: Vec<u16> = paths[0].encode_utf16().chain(Some(0)).collect();
        // An exclusive control handle refuses interference with another mapper.
        let handle = Handle(unsafe { CreateFileW(PCWSTR(path.as_ptr()), GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0), None, OPEN_EXISTING, FILE_FLAG_OVERLAPPED, None) }
            .map_err(|_| "虚拟键盘控制端不可用或被其他程序占用，未发送输入".to_string())?);
        if !expected_collection(handle.0) { return Err("虚拟键盘身份发生变化，未发送输入".into()); }
        Ok(Some(Self { handle: Some(handle), touched: false }))
    }

    pub fn state(&mut self, modifiers: u8, key: u8) -> Result<()> {
        validate_state(modifiers, key)?;
        self.touched = true;
        let result = self.write_report(report(modifiers, key));
        if result.is_ok() && modifiers == 0 && key == 0 { self.touched = false; }
        result
    }
    pub fn release(&mut self) -> Result<()> { self.state(0, 0) }

    fn write_report(&mut self, bytes: [u8; REPORT_SIZE]) -> Result<()> {
        let event = Handle(unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|_| "无法创建键盘输入完成事件".to_string())?);
        let device = self.handle.take().ok_or("虚拟键盘已隔离，禁止继续发送按键")?;
        let mut io = Box::new(WriteRequest { device, bytes,
            overlapped: OVERLAPPED { hEvent: event.0, ..Default::default() }, _event: event });
        let mut written = 0;
        // For overlapped handles lpNumberOfBytesWritten must not point to a
        // short-lived stack variable. Always obtain the result from OVERLAPPED.
        let started = unsafe { WriteFile(io.device.0, Some(&io.bytes), None, Some(&mut io.overlapped)) };
        if let Err(error) = started {
            if error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) {
                self.handle = Some(io.device);
                return Err(format!("虚拟键盘写入失败（{:08x}），已停止", error.code().0 as u32));
            }
            if unsafe { WaitForSingleObject(io.overlapped.hEvent, 500) } != WAIT_OBJECT_0 {
                unsafe { let _ = CancelIoEx(io.device.0, Some(&io.overlapped)); }
                if unsafe { WaitForSingleObject(io.overlapped.hEvent, 500) } != WAIT_OBJECT_0 {
                    // Do not race a release report against the unfinished down
                    // report. The recovery worker retains the exclusive handle,
                    // drains it, and can only send a neutral keyboard report.
                    quarantine(io, bytes[3..].iter().any(|b| *b != 0));
                    return Err("虚拟键盘取消尚未完成，已隔离输入；后台只等待完成并尝试松键".into());
                }
                unsafe { let _ = GetOverlappedResult(io.device.0, &io.overlapped, &mut written, false); }
                self.handle = Some(io.device);
                return Err("虚拟键盘响应超时，输入已取消".into());
            }
        }
        let completed = unsafe { GetOverlappedResult(io.device.0, &io.overlapped, &mut written, false) };
        self.handle = Some(io.device);
        completed.map_err(|_| "虚拟键盘未完成输入，已停止".to_string())?;
        // The driver reports its 63-byte payload; HID transports can report 65.
        if !valid_completion_size(written) { return Err("虚拟键盘报告长度异常，已停止".into()); }
        Ok(())
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        if self.touched && self.handle.is_some() { let _ = self.write_report(report(0, 0)); }
    }
}

fn quarantine(io: Box<WriteRequest>, release_after: bool) {
    IO_QUARANTINED.store(true, Ordering::Release);
    // Transfer the allocation by address so a failed thread spawn cannot drop
    // an in-flight request. On spawn failure it deliberately remains retained.
    let address = Box::into_raw(io) as usize;
    let _ = std::thread::Builder::new().name("hid-release-recovery".into()).spawn(move || {
        // SAFETY: this worker is the sole owner; the Box allocation stays pinned.
        let io = unsafe { Box::from_raw(address as *mut WriteRequest) };
        if unsafe { WaitForSingleObject(io.overlapped.hEvent, INFINITE) } != WAIT_OBJECT_0 {
            // An invalid wait is not proof of cancellation. Retain ALL resources
            // rather than allowing a use-after-free or reusing the device.
            let _ = Box::into_raw(io);
            return;
        }
        let mut count = 0;
        unsafe { let _ = GetOverlappedResult(io.device.0, &io.overlapped, &mut count, false); }
        let mut device = Keyboard { handle: Some(io.device), touched: false };
        if release_after { let _ = device.write_report(report(0, 0)); }
        // Keep the process quarantined even if emergency release completes.
    });
}

fn valid_completion_size(size: u32) -> bool { matches!(size, 63 | 65) }

fn validate_state(modifiers: u8, key: u8) -> Result<()> {
    let printable = matches!(key, 0x04..=0x27 | 0x2c..=0x38 | 0x64);
    if (key == 0 && modifiers == 0)
        || ((printable || key == 0x2a) && modifiers == 0)
        || (printable && modifiers == 2)
        || (key == 0x04 && modifiers == 1) { return Ok(()); }
    Err("HID 报告包含未允许的按键或组合，未发送输入".into())
}

fn report(modifiers: u8, key: u8) -> [u8; REPORT_SIZE] {
    let mut bytes = [0; REPORT_SIZE];
    bytes[0] = 0x40;
    bytes[1] = 9;
    bytes[2] = 1;
    bytes[3] = modifiers;
    bytes[5] = key;
    bytes
}

/// Set-1 physical scan code to USB HID usage. No Enter, GUI, or system keys.
pub fn usage(scan: u16, extended: bool) -> Result<u8> {
    if extended { return Err("当前字符需要尚未适配的扩展按键，未发送输入".into()); }
    let key = match scan {
        0x1e => 0x04, 0x30 => 0x05, 0x2e => 0x06, 0x20 => 0x07,
        0x12 => 0x08, 0x21 => 0x09, 0x22 => 0x0a, 0x23 => 0x0b,
        0x17 => 0x0c, 0x24 => 0x0d, 0x25 => 0x0e, 0x26 => 0x0f,
        0x32 => 0x10, 0x31 => 0x11, 0x18 => 0x12, 0x19 => 0x13,
        0x10 => 0x14, 0x13 => 0x15, 0x1f => 0x16, 0x14 => 0x17,
        0x16 => 0x18, 0x2f => 0x19, 0x11 => 0x1a, 0x2d => 0x1b,
        0x15 => 0x1c, 0x2c => 0x1d,
        0x02..=0x0b => (scan - 0x02) as u8 + 0x1e,
        0x0e => 0x2a, 0x39 => 0x2c, 0x0c => 0x2d, 0x0d => 0x2e,
        0x1a => 0x2f, 0x1b => 0x30, 0x2b => 0x31, 0x27 => 0x33,
        0x28 => 0x34, 0x29 => 0x35, 0x33 => 0x36, 0x34 => 0x37,
        0x35 => 0x38, 0x56 => 0x64,
        _ => return Err("当前字符无法转换为已验证的 HID 按键，未发送输入".into()),
    };
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_identity_uses_exact_hardware_id_not_generated_instance_name() {
        let ids: Vec<u16> = "root\\FakerInput\0\0".encode_utf16().collect();
        assert!(root_hardware_id_matches("ROOT\\SYSTEM\\0001", &ids));
        assert!(!root_hardware_id_matches("USB\\SYSTEM\\0001", &ids));
        assert!(!root_hardware_id_matches("ROOT\\FAKERINPUT\\0001", &[0, 0]));
        assert!(!root_hardware_id_matches("ROOT\\SYSTEM\\0001", &ids[..ids.len() - 1]));
        let other: Vec<u16> = "root\\FakerInputOther\0\0".encode_utf16().collect();
        assert!(!root_hardware_id_matches("ROOT\\SYSTEM\\0001", &other));
    }
    #[test]
    fn report_only_addresses_keyboard_and_releases_all_keys() {
        let down = report(2, 0x04);
        assert_eq!(&down[..11], &[0x40, 9, 1, 2, 0, 4, 0, 0, 0, 0, 0]);
        assert!(down[11..].iter().all(|b| *b == 0));
        let up = report(0, 0);
        assert_eq!(&up[..3], &[0x40, 9, 1]);
        assert!(up[3..].iter().all(|b| *b == 0));
    }
    #[test]
    fn maps_physical_positions_and_rejects_login_or_system_keys() {
        assert_eq!(usage(0x1e, false), Ok(4));
        assert_eq!(usage(0x02, false), Ok(0x1e));
        assert_eq!(usage(0x0b, false), Ok(0x27));
        assert_eq!(usage(0x0e, false), Ok(0x2a));
        assert_eq!(usage(0x56, false), Ok(0x64));
        for scan in [0x1c, 0x01, 0x3b, 0x5b, 0x00] { assert!(usage(scan, false).is_err()); }
        assert!(usage(0x1c, true).is_err());
    }
    #[test]
    fn direct_reports_cannot_submit_login_or_send_system_shortcuts() {
        for key in [0x28, 0x29, 0x2b, 0x3a, 0x4c, 0xe3] {
            for modifier in [0, 1, 2, 4, 8] { assert!(validate_state(modifier, key).is_err()); }
        }
        assert!(validate_state(1, 0x04).is_ok()); // Ctrl+A only
        assert!(validate_state(1, 0x19).is_err()); // no Ctrl+V
        assert!(validate_state(2, 0x1e).is_ok());
        assert!(validate_state(0, 0x2a).is_ok());
        assert!(validate_state(0, 0).is_ok());
        assert!(valid_completion_size(63));
        assert!(valid_completion_size(65));
        for size in [0, 9, 64, 66] { assert!(!valid_completion_size(size)); }
    }
}
