//! Minimal NVIDIA Management Library binding (nvml.dll ships with the driver in System32), loaded at
//! runtime so PCs without an NVIDIA driver still work. GPU / NVENC utilisation for the dashboard and
//! the optimiser.
use std::ffi::c_void;
use std::sync::OnceLock;
use windows::core::{s, w};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Dev = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}
#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

struct Api {
    device: Dev,
    get_util: unsafe extern "C" fn(Dev, *mut Utilization) -> i32,
    get_enc: unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> i32,
    get_dec: unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> i32,
    get_mem: unsafe extern "C" fn(Dev, *mut Memory) -> i32,
    get_sessions: unsafe extern "C" fn(Dev, *mut u32, *mut c_void) -> i32,
    get_temp: unsafe extern "C" fn(Dev, i32, *mut u32) -> i32,
    name: Option<String>,
    driver: Option<String>,
}
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub gpu_percent: u32,
    pub encoder_percent: u32,
    pub decoder_percent: u32,
    pub vram_used_mb: u64,
    pub vram_total_mb: u64,
    pub encoder_sessions: u32,
    pub temp_c: u32,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load() }).as_ref()
}

unsafe fn sym<T>(m: HMODULE, name: windows::core::PCSTR) -> Option<T> {
    GetProcAddress(m, name).map(|f| std::mem::transmute_copy(&f))
}

unsafe fn load() -> Option<Api> {
    let m = LoadLibraryW(w!("nvml.dll")).ok()?;
    let init: unsafe extern "C" fn() -> i32 = sym(m, s!("nvmlInit_v2"))?;
    let count: unsafe extern "C" fn(*mut u32) -> i32 = sym(m, s!("nvmlDeviceGetCount_v2"))?;
    let handle: unsafe extern "C" fn(u32, *mut Dev) -> i32 = sym(m, s!("nvmlDeviceGetHandleByIndex_v2"))?;
    let get_name: unsafe extern "C" fn(Dev, *mut u8, u32) -> i32 = sym(m, s!("nvmlDeviceGetName"))?;
    let get_driver: unsafe extern "C" fn(*mut u8, u32) -> i32 = sym(m, s!("nvmlSystemGetDriverVersion"))?;
    if init() != 0 {
        return None;
    }
    let mut n = 0;
    if count(&mut n) != 0 || n == 0 {
        return None;
    }
    let mut device = std::ptr::null_mut();
    if handle(0, &mut device) != 0 {
        return None;
    }
    let text = |f: &dyn Fn(*mut u8) -> i32| {
        let mut b = [0u8; 96];
        (f(b.as_mut_ptr()) == 0).then(|| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(96)]).into_owned())
    };
    Some(Api {
        device,
        get_util: sym(m, s!("nvmlDeviceGetUtilizationRates"))?,
        get_enc: sym(m, s!("nvmlDeviceGetEncoderUtilization"))?,
        get_dec: sym(m, s!("nvmlDeviceGetDecoderUtilization"))?,
        get_mem: sym(m, s!("nvmlDeviceGetMemoryInfo"))?,
        get_sessions: sym(m, s!("nvmlDeviceGetEncoderSessions"))?,
        get_temp: sym(m, s!("nvmlDeviceGetTemperature"))?,
        name: text(&|p| get_name(device, p, 96)),
        driver: text(&|p| get_driver(p, 96)),
    })
}

pub fn available() -> bool {
    api().is_some()
}
pub fn device_name() -> Option<String> {
    api()?.name.clone()
}
pub fn driver_version() -> Option<String> {
    api()?.driver.clone()
}

pub fn read() -> Option<Sample> {
    let a = api()?;
    unsafe {
        let mut u = Utilization::default();
        let (mut enc, mut dec, mut period) = (0, 0, 0);
        let mut m = Memory::default();
        let mut sessions = 0;
        let mut t = 0;
        (a.get_util)(a.device, &mut u);
        (a.get_enc)(a.device, &mut enc, &mut period);
        (a.get_dec)(a.device, &mut dec, &mut period);
        (a.get_mem)(a.device, &mut m);
        (a.get_sessions)(a.device, &mut sessions, std::ptr::null_mut());
        (a.get_temp)(a.device, 0, &mut t);
        Some(Sample {
            gpu_percent: u.gpu,
            encoder_percent: enc,
            decoder_percent: dec,
            vram_used_mb: m.used >> 20,
            vram_total_mb: m.total >> 20,
            encoder_sessions: sessions,
            temp_c: t,
        })
    }
}
