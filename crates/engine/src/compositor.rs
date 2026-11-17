//! GPU compositor: draws scene items into the canvas, converts the canvas to NV12 for the encoder
//! (explicit colour matrix and range, with free rescale), blends transitions and draws the preview.
//! Every method takes the locked immediate context.
use crate::gpu::{upload, Ctx, Gpu};
use crate::math::Mat4;
use crate::scene::SceneItem;
use anyhow::Result;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub const HLSL: &str = r#"
cbuffer ItemCB : register(b0)
{
    row_major float4x4 xform;
    float4 uvRect;
    float opacity;
    float forceOpaque;
    float2 pad0;
};
cbuffer ConvCB : register(b1)
{
    float4 yCoef;
    float4 uCoef;
    float4 vCoef;
    float2 lumaTexel;
    float2 pad1;
};
cbuffer TransCB : register(b2)
{
    float progress;
    float kind;
    float2 pad2;
};
Texture2D tex : register(t0);
Texture2D tex2 : register(t1);
SamplerState smp : register(s0);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut VSQuad(uint id : SV_VertexID)
{
    float2 p = float2(id & 1, id >> 1);
    VSOut o;
    o.pos = mul(float4(p, 0, 1), xform);
    o.uv = lerp(uvRect.xy, uvRect.zw, p);
    return o;
}

float4 PSItem(VSOut i) : SV_Target
{
    float4 c = tex.Sample(smp, i.uv);
    c.a = (forceOpaque > 0.5 ? 1.0 : c.a) * opacity;
    return c;
}

float4 PSSolid(VSOut i) : SV_Target { return uvRect; }

VSOut VSFull(uint id : SV_VertexID)
{
    float2 p = float2(id & 1, id >> 1);
    VSOut o;
    o.pos = float4(p.x * 2 - 1, 1 - p.y * 2, 0, 1);
    o.uv = p;
    return o;
}

// Scene transitions: tex = outgoing scene, tex2 = incoming scene, progress 0..1 (eased on the CPU).
float4 PSTransition(VSOut i) : SV_Target
{
    float t = progress;
    int k = (int)kind;
    if (k == 1) return lerp(tex.Sample(smp, i.uv), tex2.Sample(smp, i.uv), t);
    if (k == 2) return t < 0.5 ? tex.Sample(smp, i.uv) * (1 - t * 2) : tex2.Sample(smp, i.uv) * (t * 2 - 1);
    if (k == 3)
    {
        float x = i.uv.x + t;
        return x < 1 ? tex.Sample(smp, float2(x, i.uv.y)) : tex2.Sample(smp, float2(x - 1, i.uv.y));
    }
    if (k == 4)
    {
        float m = smoothstep(t * 1.1 - 0.1, t * 1.1, i.uv.x);
        return lerp(tex2.Sample(smp, i.uv), tex.Sample(smp, i.uv), m);
    }
    return tex2.Sample(smp, i.uv);
}

float PSY(VSOut i) : SV_Target
{
    float3 c = tex.Sample(smp, i.uv).rgb;
    return dot(float4(c, 1), yCoef);
}

// Chroma is sited left (MPEG-2 / H.264 default): shift half a luma texel left.
float2 PSUV(VSOut i) : SV_Target
{
    float3 c = tex.Sample(smp, i.uv - float2(lumaTexel.x * 0.5, 0)).rgb;
    float4 c4 = float4(c, 1);
    return float2(dot(c4, uCoef), dot(c4, vCoef));
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct ItemCB {
    xform: Mat4,
    uv_rect: [f32; 4],
    opacity: f32,
    force_opaque: f32,
    pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TransCB {
    progress: f32,
    kind: f32,
    pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConvCB {
    y: [f32; 4],
    u: [f32; 4],
    v: [f32; 4],
    luma_texel: [f32; 2],
    pad: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorSpace {
    #[default]
    Rec709,
    Rec601,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorRange {
    #[default]
    Limited,
    Full,
}

/// Size of a resize handle in preview pixels (also used for hit testing in the UI).
pub const HANDLE_SIZE: f32 = 9.0;
/// Distance of the rotate handle above the top edge, in preview pixels.
pub const ROTATE_HANDLE_OFFSET: f32 = 26.0;

pub struct Compositor {
    vs_quad: ID3D11VertexShader,
    vs_full: ID3D11VertexShader,
    ps_item: ID3D11PixelShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    ps_solid: ID3D11PixelShader,
    ps_transition: ID3D11PixelShader,
    item_cb: ID3D11Buffer,
    conv_cb: ID3D11Buffer,
    trans_cb: ID3D11Buffer,
    linear: ID3D11SamplerState,
    point: ID3D11SamplerState,
    alpha_blend: ID3D11BlendState,
    no_blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
}
unsafe impl Send for Compositor {}
unsafe impl Sync for Compositor {}

fn sampler(gpu: &Gpu, filter: D3D11_FILTER) -> Result<ID3D11SamplerState> {
    let d = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut s = None;
    unsafe { gpu.device.CreateSamplerState(&d, Some(&mut s)) }?;
    Ok(s.unwrap())
}

fn blend(gpu: &Gpu, enable: bool) -> Result<ID3D11BlendState> {
    let mut d = D3D11_BLEND_DESC::default();
    d.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: enable.into(),
        SrcBlend: if enable { D3D11_BLEND_SRC_ALPHA } else { D3D11_BLEND_ONE },
        DestBlend: if enable { D3D11_BLEND_INV_SRC_ALPHA } else { D3D11_BLEND_ZERO },
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: if enable { D3D11_BLEND_INV_SRC_ALPHA } else { D3D11_BLEND_ZERO },
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut b = None;
    unsafe { gpu.device.CreateBlendState(&d, Some(&mut b)) }?;
    Ok(b.unwrap())
}

fn to_clip(w: f32, h: f32) -> Mat4 {
    Mat4::scale(2.0 / w, -2.0 / h) * Mat4::translate(-1.0, 1.0)
}

impl Compositor {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let r = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let mut raster = None;
        unsafe { gpu.device.CreateRasterizerState(&r, Some(&mut raster)) }?;
        Ok(Self {
            vs_quad: gpu.vertex_shader(HLSL, "VSQuad")?,
            vs_full: gpu.vertex_shader(HLSL, "VSFull")?,
            ps_item: gpu.pixel_shader(HLSL, "PSItem")?,
            ps_y: gpu.pixel_shader(HLSL, "PSY")?,
            ps_uv: gpu.pixel_shader(HLSL, "PSUV")?,
            ps_solid: gpu.pixel_shader(HLSL, "PSSolid")?,
            ps_transition: gpu.pixel_shader(HLSL, "PSTransition")?,
            item_cb: gpu.dynamic_cb(std::mem::size_of::<ItemCB>() as u32)?,
            conv_cb: gpu.dynamic_cb(std::mem::size_of::<ConvCB>() as u32)?,
            trans_cb: gpu.dynamic_cb(std::mem::size_of::<TransCB>() as u32)?,
            linear: sampler(gpu, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
            point: sampler(gpu, D3D11_FILTER_MIN_MAG_MIP_POINT)?,
            alpha_blend: blend(gpu, true)?,
            no_blend: blend(gpu, false)?,
            raster: raster.unwrap(),
        })
    }

    fn begin_pass(&self, ctx: &Ctx, rtv: &ID3D11RenderTargetView, w: u32, h: u32, alpha: bool) {
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.RSSetState(&self.raster);
            ctx.OMSetBlendState(if alpha { &self.alpha_blend } else { &self.no_blend }, None, 0xffff_ffff);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None);
        }
    }

    fn end_pass(ctx: &Ctx) {
        unsafe {
            ctx.PSSetShaderResources(0, Some(&[None, None]));
            // Unbind outputs so the target can be read as a texture next.
            ctx.OMSetRenderTargets(None, None);
        }
    }

    fn bind_quad(&self, ctx: &Ctx, ps: &ID3D11PixelShader) {
        unsafe {
            ctx.VSSetShader(&self.vs_quad, None);
            ctx.PSSetShader(ps, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.item_cb.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.item_cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.linear.clone())]));
        }
    }

    /// Draws every visible item into `target`. Nested scenes clear to transparent so the parent shows through.
    pub fn render_items(&self, ctx: &Ctx, items: &[SceneItem], target: &ID3D11RenderTargetView, cw: u32, ch: u32, transparent: bool) {
        unsafe { ctx.ClearRenderTargetView(target, &if transparent { [0.0; 4] } else { [0.0, 0.0, 0.0, 1.0] }) };
        self.begin_pass(ctx, target, cw, ch, true);
        self.bind_quad(ctx, &self.ps_item);
        let clip = to_clip(cw as f32, ch as f32);
        for item in items {
            if !item.visible || item.opacity <= 0.0 {
                continue;
            }
            let src = &item.source;
            let (sw, sh) = src.size();
            let Some(srv) = src.frame() else { continue };
            if sw == 0 || sh == 0 || item.cropped_width() == 0 || item.cropped_height() == 0 {
                continue;
            }
            let uv = [
                item.crop_left as f32 / sw as f32,
                item.crop_top as f32 / sh as f32,
                1.0 - item.crop_right as f32 / sw as f32,
                1.0 - item.crop_bottom as f32 / sh as f32,
            ];
            upload(ctx, &self.item_cb, &ItemCB {
                xform: item.pixel_transform() * clip,
                uv_rect: uv,
                opacity: item.opacity.clamp(0.0, 1.0),
                force_opaque: if src.force_opaque() { 1.0 } else { 0.0 },
                pad: [0.0; 2],
            });
            // Pixel-exact 1:1 placement looks sharper with point sampling.
            let one_to_one = item.rotation == 0.0 && item.scale_x == 1.0 && item.scale_y == 1.0 && item.x == item.x.round() && item.y == item.y.round();
            unsafe {
                ctx.PSSetSamplers(0, Some(&[Some(if one_to_one { self.point.clone() } else { self.linear.clone() })]));
                ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
                ctx.Draw(4, 0);
            }
        }
        Self::end_pass(ctx);
    }

    /// Draws a texture letterboxed into a render target (preview window).
    #[allow(clippy::too_many_arguments)]
    pub fn blit(&self, ctx: &Ctx, srv: &ID3D11ShaderResourceView, sw: u32, sh: u32, target: &ID3D11RenderTargetView, dw: u32, dh: u32, bg: [f32; 4]) {
        unsafe { ctx.ClearRenderTargetView(target, &bg) };
        self.begin_pass(ctx, target, dw, dh, false);
        let s = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
        let (w, h) = (sw as f32 * s, sh as f32 * s);
        let (x, y) = ((dw as f32 - w) / 2.0, (dh as f32 - h) / 2.0);
        let m = Mat4::scale(w, h) * Mat4::translate(x, y) * to_clip(dw as f32, dh as f32);
        upload(ctx, &self.item_cb, &ItemCB { xform: m, uv_rect: [0.0, 0.0, 1.0, 1.0], opacity: 1.0, force_opaque: 1.0, pad: [0.0; 2] });
        self.bind_quad(ctx, &self.ps_item);
        unsafe {
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.Draw(4, 0);
        }
        Self::end_pass(ctx);
    }

    /// Selection outline + resize/rotate handles of an item in the preview (same letterbox mapping as `blit`).
    pub fn draw_selection(&self, ctx: &Ctx, item: &SceneItem, cw: u32, ch: u32, target: &ID3D11RenderTargetView, dw: u32, dh: u32) {
        let (iw, ih) = (item.display_width(), item.display_height());
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        self.begin_pass(ctx, target, dw, dh, false);
        let s = (dw as f32 / cw as f32).min(dh as f32 / ch as f32);
        let (ox, oy) = ((dw as f32 - cw as f32 * s) / 2.0, (dh as f32 - ch as f32 * s) / 2.0);
        let canvas_to_clip = Mat4::scale(s, s) * Mat4::translate(ox, oy) * to_clip(dw as f32, dh as f32);
        let item2clip = item.pixel_transform() * canvas_to_clip;
        let (tx, ty) = (2.0 / s / iw, 2.0 / s / ih);
        self.bind_quad(ctx, &self.ps_solid);
        let color = if item.locked { [0.9, 0.63, 0.23, 1.0] } else { [0.24, 0.5, 0.92, 1.0] };
        let draw = |m: Mat4, c: [f32; 4]| {
            upload(ctx, &self.item_cb, &ItemCB { xform: m * item2clip, uv_rect: c, opacity: 1.0, force_opaque: 1.0, pad: [0.0; 2] });
            unsafe { ctx.Draw(4, 0) };
        };
        draw(Mat4::scale(1.0, ty), color);
        draw(Mat4::scale(1.0, ty) * Mat4::translate(0.0, 1.0 - ty), color);
        draw(Mat4::scale(tx, 1.0), color);
        draw(Mat4::scale(tx, 1.0) * Mat4::translate(1.0 - tx, 0.0), color);
        if !item.locked {
            let (hx, hy) = (HANDLE_SIZE / s / iw, HANDLE_SIZE / s / ih);
            let ry = ROTATE_HANDLE_OFFSET / s / ih;
            let white = [1.0, 1.0, 1.0, 1.0];
            let square = |u: f32, v: f32, sx: f32, sy: f32, c: [f32; 4]| draw(Mat4::scale(sx, sy) * Mat4::translate(u - sx / 2.0, v - sy / 2.0), c);
            square(0.5, -ry / 2.0, tx * 0.6, ry, color);
            square(0.5, -ry, hx * 1.2, hy * 1.2, color);
            square(0.5, -ry, hx * 0.7, hy * 0.7, white);
            for (u, v) in [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (1.0, 0.5), (1.0, 1.0), (0.5, 1.0), (0.0, 1.0), (0.0, 0.5)] {
                square(u, v, hx, hy, color);
                square(u, v, hx * 0.55, hy * 0.55, white);
            }
        }
        Self::end_pass(ctx);
    }

    /// Blends two rendered scenes. kind: 1 fade, 2 through black, 3 slide, 4 wipe.
    #[allow(clippy::too_many_arguments)]
    pub fn render_transition(&self, ctx: &Ctx, from: &ID3D11ShaderResourceView, to: &ID3D11ShaderResourceView, target: &ID3D11RenderTargetView, w: u32, h: u32, kind: i32, progress: f32) {
        upload(ctx, &self.trans_cb, &TransCB { progress: progress.clamp(0.0, 1.0), kind: kind as f32, pad: [0.0; 2] });
        self.begin_pass(ctx, target, w, h, false);
        unsafe {
            ctx.VSSetShader(&self.vs_full, None);
            ctx.PSSetShader(&self.ps_transition, None);
            ctx.PSSetConstantBuffers(2, Some(&[Some(self.trans_cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.linear.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(from.clone()), Some(to.clone())]));
            ctx.Draw(4, 0);
        }
        Self::end_pass(ctx);
    }

    /// Converts the canvas to NV12 (BT.709/601, limited/full) at the output size; bilinear sampling rescales for free.
    pub fn convert_to_nv12(&self, ctx: &Ctx, canvas: &ID3D11ShaderResourceView, dst: &Nv12Target, space: ColorSpace, range: ColorRange) {
        let (kr, kb) = if space == ColorSpace::Rec709 { (0.2126f32, 0.0722f32) } else { (0.299, 0.114) };
        let kg = 1.0 - kr - kb;
        let full = range == ColorRange::Full;
        let (ys, yo, cs, co) = if full { (1.0, 0.0, 1.0, 128.0 / 255.0) } else { (219.0 / 255.0, 16.0 / 255.0, 224.0 / 255.0, 128.0 / 255.0) };
        let (cb_s, cr_s) = (cs / (2.0 * (1.0 - kb)), cs / (2.0 * (1.0 - kr)));
        upload(ctx, &self.conv_cb, &ConvCB {
            y: [kr * ys, kg * ys, kb * ys, yo],
            u: [-kr * cb_s, -kg * cb_s, (1.0 - kb) * cb_s, co],
            v: [(1.0 - kr) * cr_s, -kg * cr_s, -kb * cr_s, co],
            luma_texel: [1.0 / dst.width as f32, 1.0 / dst.height as f32],
            pad: [0.0; 2],
        });
        unsafe {
            ctx.VSSetShader(&self.vs_full, None);
            ctx.PSSetConstantBuffers(1, Some(&[Some(self.conv_cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.linear.clone())]));
        }
        self.begin_pass(ctx, &dst.luma_rtv, dst.width, dst.height, false);
        unsafe {
            ctx.PSSetShaderResources(0, Some(&[Some(canvas.clone())]));
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(4, 0);
        }
        self.begin_pass(ctx, &dst.chroma_rtv, dst.width / 2, dst.height / 2, false);
        unsafe {
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(4, 0);
        }
        Self::end_pass(ctx);
    }
}

/// An NV12 texture with render-target views for its luma and chroma planes.
pub struct Nv12Target {
    pub texture: ID3D11Texture2D,
    pub luma_rtv: ID3D11RenderTargetView,
    pub chroma_rtv: ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}
unsafe impl Send for Nv12Target {}

impl Nv12Target {
    pub fn new(gpu: &Gpu, w: u32, h: u32) -> Result<Self> {
        let texture = gpu.texture(w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)?;
        Ok(Self {
            luma_rtv: gpu.rtv(&texture, Some(DXGI_FORMAT_R8_UNORM))?,
            chroma_rtv: gpu.rtv(&texture, Some(DXGI_FORMAT_R8G8_UNORM))?,
            texture,
            width: w,
            height: h,
        })
    }
}
