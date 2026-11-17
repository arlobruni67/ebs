//! Clock, thread priority and small helpers shared by the engine.
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateWaitableTimerExW, SetWaitableTimer,
    WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

pub fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe { QueryPerformanceCounter(&mut v).ok() };
    v
}

pub fn qpc_freq() -> i64 {
    static F: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        let mut v = 0i64;
        unsafe { QueryPerformanceFrequency(&mut v).ok() };
        v
    })
}

pub fn qpc_to_ms(ticks: i64) -> f64 {
    ticks as f64 * 1000.0 / qpc_freq() as f64
}

/// MMCSS registration for the current thread ("Capture", "Pro Audio"...). Reverted on drop.
pub struct MmcssGuard(HANDLE);
impl MmcssGuard {
    pub fn capture() -> Self {
        let mut idx = 0u32;
        Self(unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut idx) }.unwrap_or_default())
    }
    pub fn pro_audio() -> Self {
        let mut idx = 0u32;
        Self(unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) }.unwrap_or_default())
    }
}
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.0);
            }
        }
    }
}

/// High-resolution waitable timer used to pace the render loop precisely without spinning a core.
pub struct PreciseTimer(HANDLE);
unsafe impl Send for PreciseTimer {}
impl PreciseTimer {
    pub fn new() -> Self {
        let h = unsafe {
            CreateWaitableTimerExW(None, windows::core::PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
        }
        .unwrap_or_default();
        Self(h)
    }

    /// Sleeps until `target` (QPC): timer until ~0.3 ms before, then yield-spins the rest.
    pub fn wait_until(&self, target: i64) {
        let freq = qpc_freq();
        loop {
            let remaining = target - qpc();
            if remaining <= 0 {
                return;
            }
            let remain_100ns = remaining as i128 * 10_000_000 / freq as i128;
            if remain_100ns > 3000 && !self.0.is_invalid() {
                let due = -((remain_100ns - 3000) as i64);
                unsafe {
                    let _ = SetWaitableTimer(self.0, &due, 0, None, None, false);
                    WaitForSingleObject(self.0, 100);
                }
            } else if remain_100ns > 3000 {
                std::thread::sleep(std::time::Duration::from_micros(((remain_100ns - 3000) / 10) as u64));
            } else {
                std::thread::yield_now();
            }
        }
    }
}
impl Default for PreciseTimer {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for PreciseTimer {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 1 ms system timer resolution while alive.
pub struct TimerResolution;
impl TimerResolution {
    pub fn high() -> Self {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        TimerResolution
    }
}
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

/// f64 stored in an atomic (for cross-thread statistics).
#[derive(Default)]
pub struct AtomicF64(std::sync::atomic::AtomicU64);
impl AtomicF64 {
    pub fn load(&self) -> f64 {
        f64::from_bits(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }
    pub fn store(&self, v: f64) {
        self.0.store(v.to_bits(), std::sync::atomic::Ordering::Relaxed)
    }
}

/// Makes a COM pointer (or anything) Send/Sync where we guarantee access is serialised (GPU context lock).
pub struct SendCell<T>(pub T);
unsafe impl<T> Send for SendCell<T> {}
unsafe impl<T> Sync for SendCell<T> {}
impl<T> std::ops::Deref for SendCell<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Writes a file via a temporary file and a replace-move, so a crash never leaves a half-written file.
/// Uses MoveFileExW directly: std::fs::rename fails with ERROR_NOT_SAME_DEVICE on some systems (file
/// system filter drivers), and then falls back to copying.
pub fn write_replace(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    let (a, b) = (windows::core::HSTRING::from(tmp.as_os_str()), windows::core::HSTRING::from(path.as_os_str()));
    if unsafe { MoveFileExW(&a, &b, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }.is_ok() {
        return Ok(());
    }
    std::fs::copy(&tmp, path)?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}
