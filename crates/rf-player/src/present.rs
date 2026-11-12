//! Video çizimi: D3D11, sahipli pencerede flip modeli takas zinciri. YUV→RGB dönüşümü piksel
//! gölgelendiricisinde (BT.601/709/2020, sınırlı/tam aralık); en-boy korunur, kenarlar siyah.

use std::sync::OnceLock;

use anyhow::{Context, anyhow};
use filmcraft_color::{Matrix, Range};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1,
};
use windows::core::{Interface, PCSTR, s};

use crate::source::{Frame, Planes};

const SHADER: &str = r#"
cbuffer P : register(b0) {
    float4 m0; float4 m1; float4 m2;
    float4 off;   // y ofset, y ölçek, c ofset, c ölçek
    float4 misc;  // x: 0 YUV, 1 RGBA, 2 doğrusal RGBA; y: örnek ölçeği
};
Texture2D ty : register(t0);
Texture2D tu : register(t1);
Texture2D tv : register(t2);
SamplerState sm : register(s0);
struct VO { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VO vs(uint id : SV_VertexID) {
    VO o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps(VO i) : SV_Target {
    if (misc.x > 1.5) {
        float4 c = ty.Sample(sm, i.uv);
        return float4(pow(saturate(c.rgb / max(c.a, 1e-5)), 1.0 / 2.2), 1);
    }
    if (misc.x > 0.5) { return float4(ty.Sample(sm, i.uv).rgb, 1); }
    float y = (ty.Sample(sm, i.uv).r * misc.y - off.x) * off.y;
    float cb = (tu.Sample(sm, i.uv).r * misc.y - off.z) * off.w;
    float cr = (tv.Sample(sm, i.uv).r * misc.y - off.z) * off.w;
    float3 v = float3(y, cb, cr);
    return float4(saturate(float3(dot(m0.xyz, v), dot(m1.xyz, v), dot(m2.xyz, v))), 1);
}
"#;

fn compile(entry: PCSTR, target: PCSTR) -> anyhow::Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), PCSTR::null(), None, None, entry, target, 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = r {
        let msg = errors
            .map(|b| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned() })
            .unwrap_or_default();
        return Err(anyhow!("shader: {e} {msg}"));
    }
    let b = code.context("shader blob")?;
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }.to_vec())
}

fn shaders() -> anyhow::Result<&'static (Vec<u8>, Vec<u8>)> {
    static S: OnceLock<Result<(Vec<u8>, Vec<u8>), String>> = OnceLock::new();
    S.get_or_init(|| Ok((compile(s!("vs"), s!("vs_4_0")).map_err(|e| e.to_string())?, compile(s!("ps"), s!("ps_4_0")).map_err(|e| e.to_string())?)))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// Gölgelendirici sabitleri (80 bayt).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    m: [[f32; 4]; 3],
    off: [f32; 4],
    misc: [f32; 4],
}

fn params(frame: &Frame) -> Params {
    let (mode, bits) = match &frame.planes {
        Planes::Yuv8 { .. } => (0.0, 8),
        Planes::Yuv16 { bits, .. } => (0.0, *bits),
        Planes::Rgba8(_) => (1.0, 8),
        Planes::RgbaF32(_) => (2.0, 8),
    };
    let (kr, kb) = match frame.color.matrix {
        Matrix::Bt601 => Matrix::Bt601.kr_kb(),
        Matrix::Bt709 => Matrix::Bt709.kr_kb(),
        Matrix::Bt2020Ncl => Matrix::Bt2020Ncl.kr_kb(),
    };
    let kg = 1.0 - kr - kb;
    let m = [
        [1.0, 0.0, 2.0 * (1.0 - kr), 0.0],
        [1.0, -2.0 * kb * (1.0 - kb) / kg, -2.0 * kr * (1.0 - kr) / kg, 0.0],
        [1.0, 2.0 * (1.0 - kb), 0.0, 0.0],
    ];
    // Örnekler [0,1]'e ölçeklenir: 16 bit dokuda `bits` anlamlı bit.
    let max = ((1u32 << bits.min(16)) - 1) as f32;
    let sample_scale = if matches!(frame.planes, Planes::Yuv16 { .. }) { 65535.0 / max } else { 1.0 };
    let unit = (1u32 << bits.saturating_sub(8)) as f32 / max; // 8 bit'lik bir adımın karşılığı
    let off = match frame.color.range {
        Range::Limited => [16.0 * unit, 1.0 / (219.0 * unit), 128.0 * unit, 1.0 / (224.0 * unit)],
        Range::Full => [0.0, 1.0, 128.0 * unit, 1.0],
    };
    Params { m, off, misc: [mode, sample_scale, 0.0, 0.0] }
}

/// En-boy korunarak `w`×`h` içine sığdırılmış dikdörtgen (piksel en-boyu dahil).
fn fit(frame: &Frame, w: u32, h: u32) -> D3D11_VIEWPORT {
    let fw = frame.width.max(1) as f32;
    let fh = frame.height.max(1) as f32;
    let s = (w as f32 / fw).min(h as f32 / fh);
    let (vw, vh) = ((fw * s).round(), (fh * s).round());
    D3D11_VIEWPORT { TopLeftX: ((w as f32 - vw) / 2.0).floor(), TopLeftY: ((h as f32 - vh) / 2.0).floor(), Width: vw, Height: vh, MinDepth: 0.0, MaxDepth: 1.0 }
}

struct Textures {
    key: (u32, u32, u8),
    tex: Vec<ID3D11Texture2D>,
    views: Vec<Option<ID3D11ShaderResourceView>>,
}

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    textures: Option<Textures>,
}

fn create_device(kind: D3D_DRIVER_TYPE) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    unsafe {
        D3D11CreateDevice(None, kind, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx))?;
    }
    Ok((device.unwrap(), ctx.unwrap()))
}

impl Gpu {
    fn new() -> anyhow::Result<Gpu> {
        let (device, ctx) = create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_device(D3D_DRIVER_TYPE_WARP)).context("D3D11 device")?;
        let (vsb, psb) = shaders()?;
        unsafe {
            let mut vs = None;
            device.CreateVertexShader(vsb, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(psb, None, Some(&mut ps))?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Params>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cb = None;
            device.CreateBuffer(&desc, None, Some(&mut cb))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            Ok(Gpu { device, ctx, vs: vs.unwrap(), ps: ps.unwrap(), cb: cb.unwrap(), sampler: sampler.unwrap(), textures: None })
        }
    }

    fn texture(&self, w: u32, h: u32, format: DXGI_FORMAT) -> anyhow::Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w.max(1),
            Height: h.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        unsafe {
            let mut t = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
            let t = t.unwrap();
            let mut v = None;
            self.device.CreateShaderResourceView(&t, None, Some(&mut v))?;
            Ok((t, v.unwrap()))
        }
    }

    /// Karenin düzlemlerini dokulara yükler (boyut ya da biçim değişince dokular yeniden yaratılır).
    fn upload(&mut self, frame: &Frame) -> anyhow::Result<()> {
        let (w, h) = (frame.width, frame.height);
        // (genişlik, yükseklik, biçim, bayt/örnek, veri)
        let planes: Vec<(u32, u32, DXGI_FORMAT, u32, *const u8, usize)> = match &frame.planes {
            Planes::Yuv8 { planes, chroma, .. } => {
                let (sx, sy) = chroma.shifts();
                let (cw, ch) = ((w + (1 << sx) - 1) >> sx, (h + (1 << sy) - 1) >> sy);
                vec![
                    (w, h, DXGI_FORMAT_R8_UNORM, 1, planes[0].as_ptr(), planes[0].len()),
                    (cw, ch, DXGI_FORMAT_R8_UNORM, 1, planes[1].as_ptr(), planes[1].len()),
                    (cw, ch, DXGI_FORMAT_R8_UNORM, 1, planes[2].as_ptr(), planes[2].len()),
                ]
            }
            Planes::Yuv16 { planes, chroma, .. } => {
                let (sx, sy) = chroma.shifts();
                let (cw, ch) = ((w + (1 << sx) - 1) >> sx, (h + (1 << sy) - 1) >> sy);
                vec![
                    (w, h, DXGI_FORMAT_R16_UNORM, 2, planes[0].as_ptr() as *const u8, planes[0].len() * 2),
                    (cw, ch, DXGI_FORMAT_R16_UNORM, 2, planes[1].as_ptr() as *const u8, planes[1].len() * 2),
                    (cw, ch, DXGI_FORMAT_R16_UNORM, 2, planes[2].as_ptr() as *const u8, planes[2].len() * 2),
                ]
            }
            Planes::Rgba8(d) => vec![(w, h, DXGI_FORMAT_R8G8B8A8_UNORM, 4, d.as_ptr(), d.len())],
            Planes::RgbaF32(d) => vec![(w, h, DXGI_FORMAT_R32G32B32A32_FLOAT, 16, d.as_ptr() as *const u8, d.len() * 4)],
        };
        let kind = match &frame.planes {
            Planes::Yuv8 { .. } => 0,
            Planes::Yuv16 { .. } => 1,
            Planes::Rgba8(_) => 2,
            Planes::RgbaF32(_) => 3,
        };
        let key = (w, h, kind);
        if self.textures.as_ref().is_none_or(|t| t.key != key) {
            let mut tex = Vec::new();
            let mut views = Vec::new();
            for (pw, ph, f, ..) in &planes {
                let (t, v) = self.texture(*pw, *ph, *f)?;
                tex.push(t);
                views.push(Some(v));
            }
            self.textures = Some(Textures { key, tex, views });
        }
        let t = self.textures.as_ref().unwrap();
        for (i, (pw, ph, _, bps, ptr, len)) in planes.iter().enumerate() {
            let pitch = pw * bps;
            if (*len as u64) < pitch as u64 * *ph as u64 {
                return Err(anyhow!("plane {i}: {len} bytes < {pitch}x{ph}"));
            }
            unsafe { self.ctx.UpdateSubresource(&t.tex[i], 0, None, *ptr as *const _, pitch, 0) };
        }
        Ok(())
    }

    /// `rtv`'yi (w×h) siyaha boyar ve kareyi sığdırarak çizer.
    fn draw(&mut self, frame: &Frame, rtv: &ID3D11RenderTargetView, w: u32, h: u32) -> anyhow::Result<()> {
        self.upload(frame)?;
        let p = params(frame);
        unsafe {
            let ctx = &self.ctx;
            ctx.UpdateSubresource(&self.cb, 0, None, &p as *const Params as *const _, 0, 0);
            ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[fit(frame, w, h)]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            let views = &self.textures.as_ref().unwrap().views;
            let mut srv: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];
            for (i, v) in views.iter().enumerate().take(3) {
                srv[i] = v.clone();
            }
            ctx.PSSetShaderResources(0, Some(&srv));
            ctx.Draw(3, 0);
        }
        Ok(())
    }
}

/// Bir pencereye bağlı video çizici.
pub struct Presenter {
    gpu: Gpu,
    swap: IDXGISwapChain1,
    size: (u32, u32),
    last: Option<Frame>,
}

impl Presenter {
    pub fn new(hwnd: isize) -> anyhow::Result<Presenter> {
        let gpu = Gpu::new()?;
        let factory: IDXGIFactory2 = unsafe { gpu.device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swap = unsafe { factory.CreateSwapChainForHwnd(&gpu.device, HWND(hwnd as *mut _), &desc, None, None)? };
        let d = unsafe { swap.GetDesc1()? };
        Ok(Presenter { gpu, swap, size: (d.Width, d.Height), last: None })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == self.size {
            return;
        }
        unsafe {
            self.gpu.ctx.OMSetRenderTargets(None, None);
            self.gpu.ctx.ClearState();
            if self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT(0), DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
                self.size = (w, h);
            }
        }
        self.redraw();
    }

    pub fn present(&mut self, frame: &Frame) {
        self.last = Some(frame.clone());
        self.redraw();
    }

    /// Son kareyi yeniden çizer (boyut değişince ya da WM_PAINT'te).
    pub fn redraw(&mut self) {
        let Some(frame) = self.last.clone() else { return };
        let r: anyhow::Result<()> = (|| unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let mut rtv = None;
            self.gpu.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            self.gpu.draw(&frame, &rtv.unwrap(), self.size.0, self.size.1)?;
            let _ = self.swap.Present(1, DXGI_PRESENT(0));
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!("present: {e}");
        }
    }
}

/// Kareyi ekran dışı bir hedefe (w×h) çizer ve RGBA olarak geri okur (testler için).
pub fn render_offscreen(frame: &Frame, w: u32, h: u32) -> anyhow::Result<Vec<u8>> {
    let mut gpu = Gpu::new()?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    unsafe {
        let mut target = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut target))?;
        let target = target.unwrap();
        let mut rtv = None;
        gpu.device.CreateRenderTargetView(&target, None, Some(&mut rtv))?;
        gpu.draw(frame, &rtv.unwrap(), w, h)?;
        let sdesc = D3D11_TEXTURE2D_DESC { Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..desc };
        let mut staging = None;
        gpu.device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        gpu.ctx.CopyResource(&staging, &target);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let row = std::slice::from_raw_parts((map.pData as *const u8).add((y * map.RowPitch) as usize), (w * 4) as usize);
            out.extend_from_slice(row);
        }
        gpu.ctx.Unmap(&staging, 0);
        Ok(out)
    }
}
