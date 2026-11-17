//! Moving test pattern that looks like gameplay to an encoder (camera pan, detail, grain, fast shapes,
//! static HUD). Used by the benchmark when nothing is moving on screen, and by tests.
use super::{RenderArgs, SourceBase, VideoSource};
use crate::gpu::{upload, Gpu};
use anyhow::Result;
use std::sync::atomic::{AtomicI64, Ordering};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

const HLSL: &str = r#"
cbuffer CB : register(b0) { float time; float2 size; float pad; };
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut VS(uint id : SV_VertexID)
{
    float2 p = float2(id & 1, id >> 1);
    VSOut o; o.pos = float4(p.x * 2 - 1, 1 - p.y * 2, 0, 1); o.uv = p; return o;
}
float hash(float2 p) { return frac(sin(dot(p, float2(127.1, 311.7))) * 43758.5453); }
float4 PS(VSOut i) : SV_Target
{
    float2 px = i.uv * size;
    float2 w = px + float2(time * 420.0, sin(time * 0.7) * 120.0);
    float2 cell = floor(w / 24.0);
    float3 c = lerp(float3(0.25, 0.45, 0.18), float3(0.55, 0.32, 0.20), step(0.5, hash(cell)));
    c *= 0.75 + 0.25 * hash(floor(w / 3.0));
    float edge = min(frac(w.x / 24.0), frac(w.y / 24.0));
    c *= smoothstep(0.0, 0.08, edge) * 0.4 + 0.6;
    c += (hash(floor(px / 2.0) + floor(time * 60.0)) - 0.5) * 0.10;
    [unroll] for (int k = 0; k < 6; k++)
    {
        float2 ctr = size * float2(0.5 + 0.4 * sin(time * (1.3 + k * 0.37) + k), 0.5 + 0.35 * cos(time * (1.7 + k * 0.29) + k * 2.0));
        float d = length(px - ctr);
        float3 col = float3(frac(k * 0.37 + 0.1), frac(k * 0.61 + 0.5), frac(k * 0.83 + 0.3));
        c = lerp(c, col, smoothstep(70.0, 66.0, d));
    }
    if (px.y > size.y - 90.0) c = float3(0.08, 0.08, 0.1) + step(0.5, frac(px.x / 160.0)) * 0.05;
    return float4(saturate(c), 1);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Cb {
    time: f32,
    size: [f32; 2],
    pad: f32,
}

pub struct TestPatternSource {
    base: SourceBase,
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    last_tick: AtomicI64,
}
unsafe impl Send for TestPatternSource {}
unsafe impl Sync for TestPatternSource {}

impl TestPatternSource {
    pub fn new(gpu: &Gpu, w: u32, h: u32, name: &str) -> Result<Self> {
        let tex = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0))?;
        let s = Self {
            base: SourceBase::new(name),
            rtv: gpu.rtv(&tex, None)?,
            srv: gpu.srv(&tex)?,
            vs: gpu.vertex_shader(HLSL, "VS")?,
            ps: gpu.pixel_shader(HLSL, "PS")?,
            cb: gpu.dynamic_cb(16)?,
            tex,
            last_tick: AtomicI64::new(-1),
        };
        s.base.set_size(w, h);
        s.base.set_status("Test pattern");
        Ok(s)
    }
}

impl VideoSource for TestPatternSource {
    fn base(&self) -> &SourceBase {
        &self.base
    }
    fn kind(&self) -> &'static str {
        "Test pattern"
    }
    fn frame(&self) -> Option<ID3D11ShaderResourceView> {
        Some(self.srv.clone())
    }
    fn renders_own_frames(&self) -> bool {
        true
    }
    fn render(&self, a: &RenderArgs) {
        if self.last_tick.swap(a.tick, Ordering::Relaxed) == a.tick {
            return;
        }
        let (w, h) = self.size();
        let ctx = a.ctx;
        upload(ctx, &self.cb, &Cb { time: a.seconds as f32, size: [w as f32, h as f32], pad: 0.0 });
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.Draw(4, 0);
            ctx.OMSetRenderTargets(None, None);
        }
        self.base.frames_delivered.fetch_add(1, Ordering::Relaxed);
        self.base.last_frame_qpc.store(crate::util::qpc(), Ordering::Relaxed);
    }
    fn save(&self) -> serde_json::Value {
        let (w, h) = self.size();
        serde_json::json!({ "kind": "pattern", "width": w, "height": h })
    }
}

impl TestPatternSource {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.tex
    }
}
