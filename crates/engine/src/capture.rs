//! Capture targets: top-level windows, games (by exe) and monitors.
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const KNOWN_GAME_EXES: &[&str] = &[
    "RobloxPlayerBeta", "Windows10Universal", "javaw", "Minecraft.Windows", "FortniteClient-Win64-Shipping",
    "VALORANT-Win64-Shipping", "cs2", "GTA5", "RocketLeague", "r5apex", "Overwatch", "eldenring",
];

#[derive(Clone, Debug)]
pub struct WindowTarget {
    /// HWND as an integer so targets can cross threads.
    pub hwnd: isize,
    pub title: String,
    pub class_name: String,
    /// Executable name without ".exe".
    pub exe_name: String,
    pub pid: u32,
}
impl WindowTarget {
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }
}
impl std::fmt::Display for WindowTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "[{}] {}", self.exe_name, self.title)
    }
}

#[derive(Clone, Debug)]
pub struct MonitorTarget {
    pub index: usize,
    pub hmonitor: isize,
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: f64,
    pub primary: bool,
}
impl std::fmt::Display for MonitorTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Display {}: {}x{} @ {} Hz{}", self.index + 1, self.width, self.height, self.refresh_hz, if self.primary { " (primary)" } else { "" })
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn exe_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let file = path.rsplit(['\\', '/']).next()?.to_string();
        Some(file.strip_suffix(".exe").or(file.strip_suffix(".EXE")).unwrap_or(&file).to_string())
    }
}

fn is_capturable(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.0.is_null()) {
            return false;
        }
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) != 0 {
            return false;
        }
        let mut cloaked = 0i32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 {
            return false;
        }
    }
    !matches!(class_name(hwnd).as_str(), "Progman" | "Shell_TrayWnd" | "WorkerW")
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, l: LPARAM) -> BOOL {
    let list = &mut *(l.0 as *mut Vec<HWND>);
    list.push(hwnd);
    true.into()
}

pub fn enumerate_windows() -> Vec<WindowTarget> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_windows_cb), LPARAM(&mut hwnds as *mut _ as isize));
    }
    let me = unsafe { GetCurrentProcessId() };
    hwnds
        .into_iter()
        .filter(|&h| is_capturable(h))
        .filter_map(|h| {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            if pid == me {
                return None;
            }
            Some(WindowTarget { hwnd: h.0 as isize, title: window_title(h), class_name: class_name(h), exe_name: exe_name(pid)?, pid })
        })
        .collect()
}

pub fn is_window(hwnd: isize) -> bool {
    unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() }
}
pub fn is_minimised(hwnd: isize) -> bool {
    unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

/// The game's main window: a non-minimised one first.
pub fn find_window_by_exe(exe: &str) -> Option<WindowTarget> {
    let name = exe.strip_suffix(".exe").unwrap_or(exe);
    let mut v: Vec<_> = enumerate_windows().into_iter().filter(|w| w.exe_name.eq_ignore_ascii_case(name)).collect();
    v.sort_by_key(|w| is_minimised(w.hwnd));
    v.into_iter().next()
}

pub fn enumerate_games() -> Vec<WindowTarget> {
    enumerate_windows().into_iter().filter(|w| KNOWN_GAME_EXES.iter().any(|g| g.eq_ignore_ascii_case(&w.exe_name))).collect()
}

unsafe extern "system" fn enum_monitors_cb(h: HMONITOR, _: HDC, _: *mut RECT, l: LPARAM) -> BOOL {
    let list = &mut *(l.0 as *mut Vec<HMONITOR>);
    list.push(h);
    true.into()
}

pub fn enumerate_monitors() -> Vec<MonitorTarget> {
    let mut mons: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(HDC::default(), None, Some(enum_monitors_cb), LPARAM(&mut mons as *mut _ as isize));
    }
    let mut out = Vec::new();
    for h in mons {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !unsafe { GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool() {
            continue;
        }
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        let hz = if unsafe { EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() { dm.dmDisplayFrequency as f64 } else { 60.0 };
        let r = mi.monitorInfo.rcMonitor;
        out.push(MonitorTarget {
            index: out.len(),
            hmonitor: h.0 as isize,
            device_name: wide_to_string(&mi.szDevice),
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
            refresh_hz: hz,
            primary: mi.monitorInfo.dwFlags & 1 != 0,
        });
    }
    out
}
