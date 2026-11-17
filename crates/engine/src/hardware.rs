//! Snapshot of the machine the optimiser plans for.
use crate::capture::{enumerate_monitors, MonitorTarget};
use crate::gpu::{AdapterInfo, Gpu};
use std::fmt::Write;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Clone, Debug)]
pub struct DriveSpec {
    pub name: String,
    pub free_gb: u64,
    pub total_gb: u64,
    pub format: String,
    pub removable: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DisplayRefresh {
    pub numerator: u32,
    pub denominator: u32,
}
impl DisplayRefresh {
    pub fn hz(&self) -> f64 {
        if self.denominator == 0 {
            0.0
        } else {
            self.numerator as f64 / self.denominator as f64
        }
    }
}

#[derive(Clone, Debug)]
pub struct HardwareInfo {
    pub cpu_name: String,
    pub logical_cores: usize,
    pub ram_total_mb: u64,
    pub ram_free_mb: u64,
    pub windows_version: String,
    pub adapters: Vec<AdapterInfo>,
    pub nvidia_driver: Option<String>,
    pub monitors: Vec<MonitorTarget>,
    pub refresh_rates: Vec<DisplayRefresh>,
    pub drives: Vec<DriveSpec>,
}

fn reg_value(key: &str, name: &str) -> Option<RegValue> {
    let mut size = 0u32;
    let mut ty = REG_VALUE_TYPE::default();
    let (k, n) = (HSTRING::from(key), HSTRING::from(name));
    unsafe {
        if RegGetValueW(HKEY_LOCAL_MACHINE, &k, &n, RRF_RT_REG_SZ | RRF_RT_REG_DWORD, Some(&mut ty), None, Some(&mut size)) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let r: WIN32_ERROR = RegGetValueW(HKEY_LOCAL_MACHINE, &k, &n, RRF_RT_REG_SZ | RRF_RT_REG_DWORD, Some(&mut ty), Some(buf.as_mut_ptr() as _), Some(&mut size));
        if r != ERROR_SUCCESS {
            return None;
        }
        if ty == REG_DWORD {
            return Some(RegValue::Dword(u32::from_le_bytes(buf[..4].try_into().ok()?)));
        }
        let wide: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Some(RegValue::Str(String::from_utf16_lossy(&wide)))
    }
}
enum RegValue {
    Str(String),
    Dword(u32),
}
fn reg_str(key: &str, name: &str) -> String {
    match reg_value(key, name) {
        Some(RegValue::Str(s)) => s,
        Some(RegValue::Dword(d)) => d.to_string(),
        None => String::new(),
    }
}

impl HardwareInfo {
    pub fn detect() -> HardwareInfo {
        let cpu = reg_str(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString");
        let mut mem = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
        let cv = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let build = reg_str(cv, "CurrentBuild");
        let mut os = format!("{} {} (build {}.{})", reg_str(cv, "ProductName"), reg_str(cv, "DisplayVersion"), build, reg_str(cv, "UBR"));
        // Windows 11 still reports "Windows 10" in ProductName; the build number tells the truth.
        if build.parse::<u32>().unwrap_or(0) >= 22000 {
            os = os.replace("Windows 10", "Windows 11");
        }
        HardwareInfo {
            cpu_name: cpu.trim().into(),
            logical_cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            ram_total_mb: mem.ullTotalPhys >> 20,
            ram_free_mb: mem.ullAvailPhys >> 20,
            windows_version: os,
            adapters: Gpu::enumerate_adapters().unwrap_or_default(),
            nvidia_driver: crate::nvml::driver_version(),
            monitors: enumerate_monitors(),
            refresh_rates: active_refresh_rates(),
            drives: drives(),
        }
    }
}

/// Exact (rational) refresh rate of every active display path, e.g. 60000/1001 = 59.94 Hz.
pub fn active_refresh_rates() -> Vec<DisplayRefresh> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) != ERROR_SUCCESS {
            return out;
        }
        for p in &paths[..np as usize] {
            let r = p.targetInfo.refreshRate;
            out.push(DisplayRefresh { numerator: r.Numerator, denominator: r.Denominator });
        }
    }
    out
}

fn drives() -> Vec<DriveSpec> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let h = HSTRING::from(root.as_str());
        let ty = unsafe { GetDriveTypeW(&h) }; // 2 = removable, 3 = fixed
        if ty != 3 && ty != 2 {
            continue;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(&h, Some(&mut free), Some(&mut total), None) }.is_err() {
            continue; // not ready (empty card reader)
        }
        let mut fs = [0u16; 32];
        let _ = unsafe { GetVolumeInformationW(PCWSTR(h.as_ptr()), None, None, None, None, Some(&mut fs)) };
        let format = String::from_utf16_lossy(&fs[..fs.iter().position(|&c| c == 0).unwrap_or(fs.len())]);
        out.push(DriveSpec { name: root, free_gb: free >> 30, total_gb: total >> 30, format, removable: ty == 2 });
    }
    out
}

impl std::fmt::Display for HardwareInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        let _ = writeln!(s, "CPU      : {} ({} threads)", self.cpu_name, self.logical_cores);
        let _ = writeln!(s, "RAM      : {:.1} GB total, {:.1} GB free", self.ram_total_mb as f64 / 1024.0, self.ram_free_mb as f64 / 1024.0);
        let _ = writeln!(s, "Windows  : {}", self.windows_version);
        for a in &self.adapters {
            let _ = writeln!(s, "GPU {}    : {} [{:04X}:{:04X}] {} MB VRAM{}", a.index, a.name, a.vendor_id, a.device_id, a.dedicated_vram_mb,
                if a.has_outputs { ", drives a display" } else { ", no display attached" });
        }
        if let Some(d) = &self.nvidia_driver {
            let _ = writeln!(s, "NVIDIA   : driver {d}");
        }
        for m in &self.monitors {
            let _ = writeln!(s, "Monitor  : {m}");
        }
        for r in &self.refresh_rates {
            let _ = writeln!(s, "Refresh  : {}/{} = {:.3} Hz", r.numerator, r.denominator, r.hz());
        }
        for d in &self.drives {
            let _ = writeln!(s, "Drive    : {} {} GB free of {} GB ({}, {})", d.name, d.free_gb, d.total_gb, d.format, if d.removable { "Removable" } else { "Fixed" });
        }
        f.write_str(&s)
    }
}
