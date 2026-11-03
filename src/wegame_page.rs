//! Recognize only the public page-switch label using Windows' local OCR.
//! No credentials, full-window captures, files, or network requests are used.
use std::{mem::size_of, thread::sleep, time::Duration};
use windows::Foundation::AsyncStatus;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{HANDLE, HWND, POINT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, WindowFromPoint, GA_ROOT};

type Result<T> = std::result::Result<T, String>;
const UNAVAILABLE: &str = "无法识别 WeGame 登录方式，请手动切到 QQ 账号密码登录后重试";
struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { RoUninitialize(); } } }
struct Capture { root: HWND, source: HDC, memory: HDC, bitmap: HBITMAP, old: HGDIOBJ }
impl Drop for Capture {
    fn drop(&mut self) { unsafe {
        if !self.old.is_invalid() { SelectObject(self.memory, self.old); }
        if !self.bitmap.is_invalid() { let _ = DeleteObject(self.bitmap); }
        if !self.memory.is_invalid() { let _ = DeleteDC(self.memory); }
        if !self.source.is_invalid() { ReleaseDC(self.root, self.source); }
    } }
}
fn matches_label(text: &str) -> bool {
    text.chars().filter(|ch| !ch.is_whitespace()).collect::<String>() == "QQ账号密码登录"
}

pub fn has_password_login_link(root: HWND, width: i32, height: i32, check: impl FnMut() -> Result<()>) -> Result<bool> {
    Ok(matches_label(&password_login_link_text(root, width, height, check)?))
}
pub fn password_login_link_text(root: HWND, width: i32, height: i32, mut check: impl FnMut() -> Result<()>) -> Result<String> {
    check()?;
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|_| UNAVAILABLE.to_string())?;
    let _apartment = Apartment;
    let languages = OcrEngine::AvailableRecognizerLanguages().map_err(|_| UNAVAILABLE.to_string())?;
    let language = (0..languages.Size().unwrap_or(0)).filter_map(|i| languages.GetAt(i).ok())
        .find(|language| language.LanguageTag().is_ok_and(|tag| tag.to_string().starts_with("zh")))
        .ok_or(UNAVAILABLE)?;
    let engine = OcrEngine::TryCreateFromLanguage(&language).map_err(|_| UNAVAILABLE.to_string())?;
    let (output_width, output_height) = (900, 120);
    let mut origin = POINT { x: width * 330 / 960, y: height * 480 / 540 };
    if !unsafe { ClientToScreen(root, &mut origin).as_bool() } { return Err(UNAVAILABLE.into()); }
    let (source_width, source_height) = (width * 300 / 960, height * 40 / 540);
    for point in [origin, POINT { x: origin.x + source_width - 1, y: origin.y + source_height - 1 }] {
        if unsafe { GetForegroundWindow() } != root || unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) } != root { return Err(UNAVAILABLE.into()); }
    }
    let mut capture = Capture { root: HWND::default(), source: unsafe { GetDC(HWND::default()) }, memory: HDC::default(), bitmap: HBITMAP::default(), old: HGDIOBJ::default() };
    if capture.source.is_invalid() { return Err(UNAVAILABLE.into()); }
    capture.memory = unsafe { CreateCompatibleDC(capture.source) };
    if capture.memory.is_invalid() { return Err(UNAVAILABLE.into()); }
    let info = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: output_width, biHeight: -output_height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
    let mut bits = std::ptr::null_mut();
    capture.bitmap = unsafe { CreateDIBSection(capture.source, &info, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0) }
        .map_err(|_| UNAVAILABLE.to_string())?;
    if bits.is_null() { return Err(UNAVAILABLE.into()); }
    capture.old = unsafe { SelectObject(capture.memory, capture.bitmap) };
    if capture.old.is_invalid() { return Err(UNAVAILABLE.into()); }
    // The supported 960x540 login profile keeps this strip below all fields and login buttons.
    check()?;
    if !unsafe { StretchBlt(capture.memory, 0, 0, output_width, output_height, capture.source,
        origin.x, origin.y, source_width, source_height, SRCCOPY) }.as_bool() { return Err(UNAVAILABLE.into()); }
    unsafe { let _ = GdiFlush(); }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), (output_width * output_height * 4) as usize) };
    for pixel in pixels.chunks_exact_mut(4) {
        let gray = ((u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])) / 3) as u8;
        pixel.copy_from_slice(&[gray, gray, gray, 255]);
    }
    let writer = DataWriter::new().map_err(|_| UNAVAILABLE.to_string())?;
    writer.WriteBytes(pixels).map_err(|_| UNAVAILABLE.to_string())?;
    let bitmap = SoftwareBitmap::CreateWithAlpha(BitmapPixelFormat::Bgra8, output_width, output_height, BitmapAlphaMode::Ignore)
        .map_err(|_| UNAVAILABLE.to_string())?;
    bitmap.CopyFromBuffer(&writer.DetachBuffer().map_err(|_| UNAVAILABLE.to_string())?).map_err(|_| UNAVAILABLE.to_string())?;
    drop(capture);
    let operation = engine.RecognizeAsync(&bitmap).map_err(|_| UNAVAILABLE.to_string())?;
    loop {
        if let Err(error) = check() { let _ = operation.Cancel(); return Err(error); }
        match operation.Status().map_err(|_| UNAVAILABLE.to_string())? {
            AsyncStatus::Completed => break,
            AsyncStatus::Started => sleep(Duration::from_millis(5)),
            _ => return Err(UNAVAILABLE.into()),
        }
    }
    let text = operation.GetResults().and_then(|result| result.Text()).map_err(|_| UNAVAILABLE.to_string())?;
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_full_password_page_switch_label_is_accepted() {
        assert!(matches_label("QQ 账号密码登录"));
        assert!(matches_label("QQ 账 号 密 码 登 录\n"));
        for other in ["登录", "快捷安全登录", "QQ 扫码登录", "微信登录", "", "加载中", "不是QQ账号密码登录"] { assert!(!matches_label(other)); }
    }
}
