//! The refresh rate of the monitor the window is on, so frame timing is judged against the
//! display rather than against the app's own frame rate (an app drawing at 72 fps on a
//! 144 Hz monitor misses every other refresh, although its frames are evenly spaced).

use eframe::wgpu::rwh::HasWindowHandle;

/// Refresh rate in Hz of the monitor showing the window, if the platform reports it.
#[cfg(windows)]
pub fn refresh_hz(window: &impl HasWindowHandle) -> Option<f32> {
    use eframe::wgpu::rwh::RawWindowHandle;
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => imp::refresh_hz(handle.hwnd.get() as _),
        _ => None,
    }
}

/// Refresh rate in Hz of the monitor showing the window, if the platform reports it.
#[cfg(not(windows))]
pub fn refresh_hz(_window: &impl HasWindowHandle) -> Option<f32> {
    None
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW,
        MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
    };

    pub fn refresh_hz(hwnd: HWND) -> Option<f32> {
        // SAFETY: plain Win32 queries with zero-initialised, correctly sized structs.
        unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() {
                return None;
            }
            let mut info: MONITORINFOEXW = std::mem::zeroed();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(
                monitor,
                &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
            ) == 0
            {
                return None;
            }
            let mut mode: DEVMODEW = std::mem::zeroed();
            mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
                return None;
            }
            // 0 and 1 mean "the hardware's default rate", which says nothing.
            (mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency as f32)
        }
    }
}
