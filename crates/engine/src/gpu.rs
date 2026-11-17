//! The single D3D11 device the whole engine runs on: capture copies, compositing, colour conversion and
//! (for NVENC) the encoder input textures all live here, so a captured frame never leaves the GPU.
use anyhow::{anyhow, Context as _, Result};
use std::sync::{Mutex, MutexGuard};
use windows::core::{Interface, PCSTR};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

pub type Ctx = ID3D11DeviceContext;

#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram_mb: u64,
    pub luid: i64,
    pub has_outputs: bool,
}
impl AdapterInfo {
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id == 0x10DE
    }
    pub fn is_amd(&self) -> bool {
        self.vendor_id == 0x1002
    }
    pub fn is_intel(&self) -> bool {
        self.vendor_id == 0x8086
    }
    pub fn is_software(&self) -> bool {
        self.vendor_id == 0x1414
    }
}

pub struct Gpu {
    pub device: ID3D11Device,
    ctx: Mutex<crate::util::SendCell<ID3D11DeviceContext>>,
    pub adapter: AdapterInfo,
    pub factory: IDXGIFactory2,
    pub winrt_device: IDirect3DDevice,
}
// The device is multithread-protected; the immediate context is only used through `lock()`.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

pub type CtxGuard<'a> = MutexGuard<'a, crate::util::SendCell<ID3D11DeviceContext>>;

impl Gpu {
    pub fn enumerate_adapters() -> Result<Vec<AdapterInfo>> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
        let mut list = Vec::new();
        let mut i = 0u32;
        while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
            let d = unsafe { a.GetDesc1() }?;
            let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
            let outputs = unsafe { a.EnumOutputs(0) }.is_ok();
            list.push(AdapterInfo {
                index: i,
                name: String::from_utf16_lossy(&d.Description[..len]).trim().to_string(),
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                dedicated_vram_mb: (d.DedicatedVideoMemory / (1024 * 1024)) as u64,
                luid: ((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64,
                has_outputs: outputs,
            });
            i += 1;
        }
        Ok(list)
    }

    /// Discrete NVIDIA first (games render there and NVENC reads our textures directly), else the largest VRAM.
    pub fn choose_adapter(adapters: &[AdapterInfo], forced: Option<u32>) -> Option<AdapterInfo> {
        if let Some(f) = forced {
            if let Some(a) = adapters.iter().find(|a| a.index == f) {
                return Some(a.clone());
            }
        }
        adapters
            .iter()
            .find(|a| a.is_nvidia())
            .or_else(|| adapters.iter().filter(|a| !a.is_software()).max_by_key(|a| a.dedicated_vram_mb))
            .or(adapters.first())
            .cloned()
    }

    pub fn create(info: &AdapterInfo) -> Result<Gpu> {
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }?;
        let adapter = unsafe { factory.EnumAdapters1(info.index) }?;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        }
        .context("D3D11CreateDevice")?;
        let device: ID3D11Device = device.ok_or_else(|| anyhow!("no device"))?;
        let ctx = ctx.ok_or_else(|| anyhow!("no context"))?;
        let mt: ID3D11Multithread = device.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        let dxgi: IDXGIDevice = device.cast()?;
        let insp = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?;
        let winrt_device: IDirect3DDevice = insp.cast()?;
        Ok(Gpu { device, ctx: Mutex::new(crate::util::SendCell(ctx)), adapter: info.clone(), factory, winrt_device })
    }

    /// Serialises use of the immediate context between the render thread, capture callbacks and outputs.
    pub fn lock(&self) -> CtxGuard<'_> {
        self.ctx.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn texture(&self, w: u32, h: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
        self.texture_ex(w, h, format, bind.0 as u32, D3D11_USAGE_DEFAULT, 0, 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn texture_ex(&self, w: u32, h: u32, format: DXGI_FORMAT, bind: u32, usage: D3D11_USAGE, cpu: u32, misc: u32) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: bind,
            CPUAccessFlags: cpu,
            MiscFlags: misc,
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.with_context(|| format!("CreateTexture2D {w}x{h} {format:?}"))?;
        t.ok_or_else(|| anyhow!("CreateTexture2D returned null"))
    }

    pub fn rtv(&self, tex: &ID3D11Texture2D, format: Option<DXGI_FORMAT>) -> Result<ID3D11RenderTargetView> {
        let mut v = None;
        unsafe {
            match format {
                Some(f) => {
                    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: f,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
                    };
                    self.device.CreateRenderTargetView(tex, Some(&desc), Some(&mut v))?
                }
                None => self.device.CreateRenderTargetView(tex, None, Some(&mut v))?,
            }
        }
        v.ok_or_else(|| anyhow!("CreateRenderTargetView null"))
    }

    pub fn srv(&self, tex: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        let mut v = None;
        unsafe { self.device.CreateShaderResourceView(tex, None, Some(&mut v)) }?;
        v.ok_or_else(|| anyhow!("CreateShaderResourceView null"))
    }

    pub fn dynamic_cb(&self, size: u32) -> Result<ID3D11Buffer> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size.div_ceil(16) * 16,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut b = None;
        unsafe { self.device.CreateBuffer(&desc, None, Some(&mut b)) }?;
        b.ok_or_else(|| anyhow!("CreateBuffer null"))
    }

    pub fn vertex_shader(&self, hlsl: &str, entry: &str) -> Result<ID3D11VertexShader> {
        let code = compile(hlsl, entry, "vs_5_0")?;
        let mut s = None;
        unsafe { self.device.CreateVertexShader(&code, None, Some(&mut s)) }?;
        s.ok_or_else(|| anyhow!("CreateVertexShader null"))
    }

    pub fn pixel_shader(&self, hlsl: &str, entry: &str) -> Result<ID3D11PixelShader> {
        let code = compile(hlsl, entry, "ps_5_0")?;
        let mut s = None;
        unsafe { self.device.CreatePixelShader(&code, None, Some(&mut s)) }?;
        s.ok_or_else(|| anyhow!("CreatePixelShader null"))
    }
}

/// Writes `value` into a dynamic constant buffer.
pub fn upload<T: Copy>(ctx: &Ctx, buf: &ID3D11Buffer, value: &T) {
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        if ctx.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).is_ok() {
            std::ptr::copy_nonoverlapping(value as *const T as *const u8, m.pData as *mut u8, std::mem::size_of::<T>());
            ctx.Unmap(buf, 0);
        }
    }
}

pub fn compile(hlsl: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    let entry_c = std::ffi::CString::new(entry)?;
    let target_c = std::ffi::CString::new(target)?;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            hlsl.as_ptr() as *const _,
            hlsl.len(),
            PCSTR(c"ebs.hlsl".as_ptr() as *const u8),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = r {
        let msg = errors
            .map(|b| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() })
            .unwrap_or_default();
        return Err(anyhow!("shader {entry} ({target}): {e} {msg}"));
    }
    let b = code.ok_or_else(|| anyhow!("no shader blob"))?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

/// Reads a BGRA texture back to the CPU (tests, snapshots). Must hold the context lock.
pub fn read_bgra(gpu: &Gpu, ctx: &Ctx, tex: &ID3D11Texture2D) -> Result<(Vec<u8>, u32, u32)> {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut d) };
    let staging = gpu.texture_ex(d.Width, d.Height, d.Format, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32, 0)?;
    let mut out = vec![0u8; (d.Width * d.Height * 4) as usize];
    unsafe {
        ctx.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        for y in 0..d.Height as usize {
            let src = (m.pData as *const u8).add(y * m.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * d.Width as usize * 4), d.Width as usize * 4);
        }
        ctx.Unmap(&staging, 0);
    }
    Ok((out, d.Width, d.Height))
}

/// Saves BGRA pixels as a top-down 32-bit BMP.
pub fn save_bmp(path: &std::path::Path, px: &[u8], w: u32, h: u32) -> Result<()> {
    let mut f = Vec::with_capacity(54 + px.len());
    let size = 54 + px.len() as u32;
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&size.to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(-(h as i32)).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    f.extend_from_slice(px);
    std::fs::write(path, f)?;
    Ok(())
}
