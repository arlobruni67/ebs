//! Samples CPU (system, EBS, optional game process), RAM and NVIDIA GPU/NVENC load.
use crate::nvml;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSample {
    pub system_cpu_percent: f64,
    pub app_cpu_percent: f64,
    pub game_cpu_percent: f64,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub app_ram_mb: u64,
    pub gpu: Option<nvml::Sample>,
}

fn ft(f: FILETIME) -> i64 {
    ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as i64
}

/// Total CPU time of a process in 100 ns units.
fn process_cpu(h: HANDLE) -> Option<i64> {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some(ft(k) + ft(u))
}

pub struct SystemMonitor {
    last_idle: i64,
    last_kernel: i64,
    last_user: i64,
    last_wall: i64,
    last_app: i64,
    last_game: i64,
    game: Option<HANDLE>,
    cores: f64,
}
unsafe impl Send for SystemMonitor {}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMonitor {
    pub fn new() -> Self {
        let mut m = SystemMonitor {
            last_idle: 0,
            last_kernel: 0,
            last_user: 0,
            last_wall: 0,
            last_app: 0,
            last_game: 0,
            game: None,
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64,
        };
        m.sample();
        m
    }

    /// Also report the CPU use of this process (e.g. RobloxPlayerBeta) so recording impact can be judged.
    pub fn track_process(&mut self, pid: Option<u32>) {
        if let Some(h) = self.game.take() {
            unsafe { let _ = CloseHandle(h); }
        }
        self.game = pid.and_then(|p| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p) }.ok());
        self.last_game = self.game.and_then(process_cpu).unwrap_or(0);
    }

    pub fn sample(&mut self) -> SystemSample {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) };
        let (idle, kernel, user) = (ft(idle), ft(kernel), ft(user));
        let wall = crate::util::qpc();
        let d_idle = idle - self.last_idle;
        let d_total = (kernel - self.last_kernel) + (user - self.last_user); // kernel time includes idle
        let sys = if d_total > 0 { (100.0 * (d_total - d_idle) as f64 / d_total as f64).clamp(0.0, 100.0) } else { 0.0 };
        let me = unsafe { GetCurrentProcess() };
        let app = process_cpu(me).unwrap_or(0);
        let game = self.game.and_then(process_cpu).unwrap_or(0);
        let wall_sec = (crate::util::qpc_to_ms(wall - self.last_wall) / 1000.0).max(1e-3);
        let first = self.last_wall == 0;
        let pct = |d: i64| if first { 0.0 } else { d as f64 / 1e7 / wall_sec / self.cores * 100.0 };
        let app_pct = pct(app - self.last_app);
        let game_pct = if game == 0 { 0.0 } else { pct(game - self.last_game) };
        (self.last_idle, self.last_kernel, self.last_user, self.last_wall, self.last_app, self.last_game) = (idle, kernel, user, wall, app, game);

        let mut mem = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        let _ = unsafe { GetProcessMemoryInfo(me, &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, pmc.cb) };
        let total = mem.ullTotalPhys >> 20;
        SystemSample {
            system_cpu_percent: sys,
            app_cpu_percent: app_pct,
            game_cpu_percent: game_pct,
            ram_used_mb: total - (mem.ullAvailPhys >> 20),
            ram_total_mb: total,
            app_ram_mb: (pmc.PrivateUsage >> 20) as u64,
            gpu: nvml::read(),
        }
    }
}

impl Drop for SystemMonitor {
    fn drop(&mut self) {
        if let Some(h) = self.game.take() {
            unsafe { let _ = CloseHandle(h); }
        }
    }
}
