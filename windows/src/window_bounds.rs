//! Keep this UI thread's visible windows inside their monitor's work area.
//! Win32 rectangles include decorations and use physical pixels on our DPI-aware
//! UI thread, so neither the title bar nor the taskbar needs a guessed allowance.
use windows::Win32::{
    Foundation::{BOOL, HWND, LPARAM, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        EnumThreadWindows, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed,
        SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
    },
};

pub fn constrain_windows() {
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(constrain_window), LPARAM(0));
    }
}

unsafe extern "system" fn constrain_window(hwnd: HWND, _: LPARAM) -> BOOL {
    // SAFETY: EnumThreadWindows supplies a live HWND on its owning UI thread.
    unsafe {
    // Leave minimisation and Windows' maximised-window placement alone.
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    let mut monitor = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetWindowRect(hwnd, &mut rect).is_err()
        || !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool()
    {
        return BOOL(1);
    }
    let work = monitor.rcWork;
    let available_width = work.right - work.left;
    let available_height = work.bottom - work.top;
    if available_width <= 0 || available_height <= 0 {
        return BOOL(1);
    }
    let width = (rect.right - rect.left).min(available_width);
    let height = (rect.bottom - rect.top).min(available_height);
    if width <= 0 || height <= 0 {
        return BOOL(1);
    }
    let x = rect.left.clamp(work.left, work.right - width);
    let y = rect.top.clamp(work.top, work.bottom - height);
    if x != rect.left || y != rect.top || width != rect.right - rect.left || height != rect.bottom - rect.top {
        let _ = SetWindowPos(hwnd, None, x, y, width, height, SWP_NOACTIVATE | SWP_NOZORDER);
    }
    BOOL(1)
    }
}
