//! YUV → RGB on the GPU: a decoded NV12 / P010 surface (a slice of a texture array) becomes a
//! BGRA8 texture of any size on the same D3D11 device. One pixel shader does the chroma
//! upsampling (the sampler's bilinear filter on the half size chroma plane), the colour matrix
//! and range from the stream's VUI, scaling, rotation by 0 / 90 / 180 / 270 degrees, an optional
//! mirror, and a first version of HDR → SDR tone mapping (see [`Transfer`]).
//!
//! The shader is compiled when the converter is created, with `D3DCompile` from
//! `d3dcompiler_47.dll` (the same way the GPUI fork compiles its shaders), so the build needs no
//! shader tools.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2DARRAY};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM, DXGI_SAMPLE_DESC};
use windows::core::{BOOL, Interface, PCSTR, s};

use crate::hevc::Vui;
use crate::{Error, Result};

fn gpu<T>(what: &str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| Error::Gpu(format!("{what}: {e}")))
}

/// Which luma coefficients turn Y'CbCr into R'G'B'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
    Smpte240,
}

impl Matrix {
    /// (Kr, Kb)
    fn k(self) -> (f64, f64) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
            Matrix::Smpte240 => (0.212, 0.087),
        }
    }
}

/// The transfer function of the picture. PQ and HLG are tone mapped to SDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    /// Gamma encoded SDR (BT.709, sRGB, BT.601 ...): shown as it is.
    Sdr,
    /// SMPTE ST 2084 (HDR10, Dolby Vision profile 8.1 base layer ...).
    Pq,
    /// ARIB STD-B67 (hybrid log-gamma).
    Hlg,
}

/// How to read the picture's colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colorimetry {
    pub matrix: Matrix,
    pub full_range: bool,
    pub transfer: Transfer,
    /// The primaries are BT.2020: wide gamut, converted to BT.709 for the display.
    pub bt2020_primaries: bool,
}

impl Colorimetry {
    /// From the VUI of the stream. Unsignalled streams get BT.709 when they are HD (1280 wide or
    /// more, or more than 576 lines) and BT.601 below, limited range.
    pub fn from_vui(vui: &Vui, width: u32, height: u32) -> Self {
        let hd = width >= 1280 || height > 576;
        let by_size = if hd { Matrix::Bt709 } else { Matrix::Bt601 };
        let matrix = match vui.matrix_coefficients {
            1 => Matrix::Bt709,
            5 | 6 => Matrix::Bt601,
            7 => Matrix::Smpte240,
            9 | 10 => Matrix::Bt2020,
            // 2 (unspecified), 0 (RGB, not carried as YUV), anything else
            _ if vui.colour_primaries == 9 => Matrix::Bt2020,
            _ => by_size,
        };
        let transfer = match vui.transfer_characteristics {
            16 => Transfer::Pq,
            18 => Transfer::Hlg,
            _ => Transfer::Sdr,
        };
        Colorimetry { matrix, full_range: vui.full_range, transfer, bt2020_primaries: vui.colour_primaries == 9 }
    }

    /// BT.709, limited range, SDR.
    pub fn bt709() -> Self {
        Colorimetry { matrix: Matrix::Bt709, full_range: false, transfer: Transfer::Sdr, bt2020_primaries: false }
    }
}

/// A decoded picture to convert: a slice of an NV12 / P010 texture array and the visible
/// rectangle inside the surface.
#[derive(Debug, Clone)]
pub struct Source {
    pub texture: ID3D11Texture2D,
    pub slice: u32,
    /// Visible rectangle in luma samples (the conformance window).
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
}

impl Source {
    pub fn from_frame(f: &crate::decoder::Frame) -> Self {
        Source { texture: f.texture.clone(), slice: f.index, left: f.crop.0, top: f.crop.2, width: f.width, height: f.height, bit_depth: f.bit_depth }
    }
}

/// What to do with the picture on the way.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub colour: Colorimetry,
    /// Clockwise degrees to turn the picture: 0, 90, 180 or 270 (the track's `rotation`).
    pub rotation: u16,
    /// Mirror the (rotated) picture left to right.
    pub flip: bool,
}

/// Size of a picture of `w` x `h` after turning it by `rotation` degrees.
pub fn rotated_size(w: u32, h: u32, rotation: u16) -> (u32, u32) {
    if rotation == 90 || rotation == 270 { (h, w) } else { (w, h) }
}

/// The largest size with the aspect ratio of `w` x `h` that fits `max_w` x `max_h` (never above
/// the original size).
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

/// The BGRA8 texture a picture is converted into.
#[derive(Debug, Clone)]
pub struct Target {
    pub texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}

const HLSL: &str = r#"
cbuffer P : register(b0) {
    float4 surf;   // surface width, height
    float4 crop;   // visible left, top, width, height (luma samples)
    float4 yp;     // black level, luma gain, chroma middle, chroma gain (in units of the sample range)
    float4 coef;   // R from Cr, G from Cb, G from Cr, B from Cb
    float4 misc;   // sample scale, rotation (0..3), flip, transfer (0 SDR, 1 PQ, 2 HLG)
    float4 gamut;  // x: convert BT.2020 primaries to BT.709, y: blend neighbouring lines (deinterlace)
    float4 planar; // planar CPU pictures: chroma halved in x, Y rows, chroma plane width
    float4 vorg;   // planar CPU pictures: where the V plane starts relative to the U plane
};
Texture2DArray<float>  TY : register(t0);
Texture2DArray<float2> TC : register(t1);
SamplerState S : register(s0);

struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VOut vs(uint id : SV_VertexID) {
    float2 p = float2((id << 1) & 2, id & 2);
    VOut o;
    o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = p;
    return o;
}

static const float M1 = 0.1593017578125;
static const float M2 = 78.84375;
static const float C1 = 0.8359375;
static const float C2 = 18.8515625;
static const float C3 = 18.6875;

float3 pq_eotf(float3 e) {   // signal -> 0..1 (10000 nits)
    float3 p = pow(max(e, 0), 1.0 / M2);
    return pow(max(p - C1, 0) / (C2 - C3 * p), 1.0 / M1);
}
float3 pq_oetf(float3 y) {
    float3 p = pow(max(y, 0), M1);
    return pow((C1 + C2 * p) / (1 + C3 * p), M2);
}

// BT.2390 EETF with a 1000 nit source and a 203 nit target (the HDR reference white, shown as
// SDR white), black level 0. Works on PQ signals.
float3 eetf(float3 e) {
    float lw = pq_oetf(float3(0.1, 0.1, 0.1)).x;
    float lt = pq_oetf(float3(203.0 / 10000.0, 0, 0)).x;
    float3 e1 = min(e / lw, 1.0);
    float maxl = lt / lw;
    float ks = 1.5 * maxl - 0.5;
    float3 t = (e1 - ks) / (1 - ks);
    float3 t2 = t * t;
    float3 t3 = t2 * t;
    float3 p = (2 * t3 - 3 * t2 + 1) * ks + (t3 - 2 * t2 + t) * (1 - ks) + (-2 * t3 + 3 * t2) * maxl;
    return ((e1 < ks) ? e1 : p) * lw;
}

float3 hlg_inverse(float3 e) {
    const float a = 0.17883277, b = 0.28466892, c = 0.55991073;
    return (e <= 0.5) ? e * e / 3.0 : (exp((e - c) / a) + b) / 12.0;
}

float3 srgb_oetf(float3 x) {
    x = saturate(x);
    return (x <= 0.0031308) ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

// where in the picture (luma samples) this output pixel comes from
float2 srcpos(VOut i) {
    float2 o = i.uv;
    if (misc.z > 0.5) o.x = 1.0 - o.x;
    int rot = (int)(misc.y + 0.5);
    float2 s = o;
    if (rot == 1) s = float2(o.y, 1.0 - o.x);
    else if (rot == 2) s = float2(1.0 - o.x, 1.0 - o.y);
    else if (rot == 3) s = float2(1.0 - o.y, o.x);

    float2 p = crop.xy + s * crop.zw;
    return clamp(p, crop.xy + 0.5, crop.xy + crop.zw - 0.5);
}

float4 shade(float y, float2 c) {
    float Y = (y - yp.x) * yp.y;
    float cb = (c.x - yp.z) * yp.w;
    float cr = (c.y - yp.z) * yp.w;
    float3 rgb = float3(Y + coef.x * cr, Y - coef.y * cb - coef.z * cr, Y + coef.w * cb);

    int tf = (int)(misc.w + 0.5);
    if (tf == 0) return float4(saturate(rgb), 1);

    float3 pq;
    if (tf == 1) {
        pq = saturate(rgb);
    } else {
        float3 sc = hlg_inverse(saturate(rgb));
        float ys = dot(sc, float3(0.2627, 0.6780, 0.0593));
        float3 nits = 1000.0 * pow(max(ys, 1e-6), 0.2) * sc;   // 1000 nit display, system gamma 1.2
        pq = pq_oetf(nits / 10000.0);
    }
    float3 lin = pq_eotf(eetf(pq)) * (10000.0 / 203.0);          // 1.0 = SDR white
    if (gamut.x > 0.5) {
        lin = float3(1.6605 * lin.r - 0.5876 * lin.g - 0.0728 * lin.b,
                     -0.1246 * lin.r + 1.1329 * lin.g - 0.0083 * lin.b,
                     -0.0182 * lin.r - 0.1006 * lin.g + 1.1187 * lin.b);
    }
    return float4(srgb_oetf(lin), 1);
}

float4 ps(VOut i) : SV_Target {
    float2 p = srcpos(i);
    float3 uv = float3(p / surf.xy, 0);
    float y = TY.SampleLevel(S, uv, 0) * misc.x;
    float2 c = TC.SampleLevel(S, uv, 0) * misc.x;
    if (gamut.y > 0.5) {
        // Linear blend of the fields: the line with its two neighbours, 1 2 1 (luma), and the
        // chroma row with the rows above and below (one chroma row is two luma lines). Moving
        // edges lose their comb, still edges lose a little vertical sharpness.
        float lo = crop.y + 0.5, hi = crop.y + crop.w - 0.5;
        float3 up = float3(float2(p.x, clamp(p.y - 1.0, lo, hi)) / surf.xy, 0);
        float3 dn = float3(float2(p.x, clamp(p.y + 1.0, lo, hi)) / surf.xy, 0);
        y = 0.5 * y + 0.25 * (TY.SampleLevel(S, up, 0) + TY.SampleLevel(S, dn, 0)) * misc.x;
        float3 cu = float3(float2(p.x, clamp(p.y - 2.0, lo, hi)) / surf.xy, 0);
        float3 cd = float3(float2(p.x, clamp(p.y + 2.0, lo, hi)) / surf.xy, 0);
        c = 0.5 * c + 0.25 * (TC.SampleLevel(S, cu, 0) + TC.SampleLevel(S, cd, 0)) * misc.x;
    }
    return shade(y, c);
}

// Planar 4:2:2 / 4:4:4 pictures decoded on the CPU, in one R16 texture: Y on top, then U and V
// (side by side for 4:2:2, below one another for 4:4:4). planar: chroma halved in x (1 / 0),
// rows of the Y plane, width of a chroma plane; vorg: where the V plane starts.
float4 ps_planar(VOut i) : SV_Target {
    float2 p = srcpos(i);
    float y = TY.SampleLevel(S, float3(p / surf.xy, 0), 0) * misc.x;
    float cx = clamp(p.x * (planar.x > 0.5 ? 0.5 : 1.0), 0.5, planar.z - 0.5);
    float2 up = float2(cx, planar.y + p.y);
    float2 vp = up + vorg.xy;
    float u = TY.SampleLevel(S, float3(up / surf.xy, 0), 0) * misc.x;
    float v = TY.SampleLevel(S, float3(vp / surf.xy, 0), 0) * misc.x;
    return shade(y, float2(u, v));
}
"#;

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut err = None;
        let r = D3DCompile(HLSL.as_ptr() as *const c_void, HLSL.len(), PCSTR::null(), None, None, entry, target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &mut code, Some(&mut err));
        if let Err(e) = r {
            let msg = err.map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned()).unwrap_or_default();
            return Err(Error::Gpu(format!("shader compile: {e} {msg}")));
        }
        let b = code.ok_or(Error::Gpu("no shader code".into()))?;
        Ok(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    surf: [f32; 4],
    crop: [f32; 4],
    yp: [f32; 4],
    coef: [f32; 4],
    misc: [f32; 4],
    gamut: [f32; 4],
    planar: [f32; 4],
    vorg: [f32; 4],
}

struct TimerSet {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    busy: bool,
}

/// Converts decoded pictures to BGRA8 textures.
pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_planar: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    raster: ID3D11RasterizerState,
    cbuf: ID3D11Buffer,
    views: HashMap<(usize, u32), (ID3D11Texture2D, ID3D11ShaderResourceView, ID3D11ShaderResourceView)>,
    timers: Vec<TimerSet>,
    next_timer: usize,
    gpu_ns: f64,
    gpu_samples: u64,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    deinterlace: bool,
}

impl Converter {
    /// A converter on the device the pictures are decoded on.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        let vs_code = compile(s!("vs"), s!("vs_5_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_5_0"))?;
        let ps_planar_code = compile(s!("ps_planar"), s!("ps_5_0"))?;
        unsafe {
            let mut ps_planar = None;
            gpu("CreatePixelShader (planar)", device.CreatePixelShader(&ps_planar_code, None, Some(&mut ps_planar)))?;
            let mut vs = None;
            gpu("CreateVertexShader", device.CreateVertexShader(&vs_code, None, Some(&mut vs)))?;
            let mut ps = None;
            gpu("CreatePixelShader", device.CreatePixelShader(&ps_code, None, Some(&mut ps)))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                ..Default::default()
            };
            let mut sampler = None;
            gpu("CreateSamplerState", device.CreateSamplerState(&sd, Some(&mut sampler)))?;
            let rd = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: BOOL(1), ..Default::default() };
            let mut raster = None;
            gpu("CreateRasterizerState", device.CreateRasterizerState(&rd, Some(&mut raster)))?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Constants>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            gpu("CreateBuffer", device.CreateBuffer(&bd, None, Some(&mut cbuf)))?;
            let none = || Error::Gpu("device returned nothing".into());
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs: vs.ok_or_else(none)?,
                ps: ps.ok_or_else(none)?,
                ps_planar: ps_planar.ok_or_else(none)?,
                sampler: sampler.ok_or_else(none)?,
                raster: raster.ok_or_else(none)?,
                cbuf: cbuf.ok_or_else(none)?,
                views: HashMap::new(),
                timers: Vec::new(),
                next_timer: 0,
                gpu_ns: 0.0,
                gpu_samples: 0,
                staging: None,
                deinterlace: false,
            })
        }
    }

    /// A BGRA8 texture of `width` x `height` to convert into (also usable as a shader resource,
    /// and as the source of a swap chain copy).
    pub fn create_target(&self, width: u32, height: u32) -> Result<Target> {
        unsafe {
            let d = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut t = None;
            gpu("CreateTexture2D (target)", self.device.CreateTexture2D(&d, None, Some(&mut t)))?;
            let texture = t.ok_or(Error::Gpu("no texture".into()))?;
            let mut rtv = None;
            gpu("CreateRenderTargetView", self.device.CreateRenderTargetView(&texture, None, Some(&mut rtv)))?;
            Ok(Target { texture, rtv: rtv.ok_or(Error::Gpu("no view".into()))?, width, height })
        }
    }

    fn planes(&mut self, tex: &ID3D11Texture2D, slice: u32) -> Result<(ID3D11ShaderResourceView, ID3D11ShaderResourceView, u32, u32, DXGI_FORMAT)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (fy, fc) = match desc.Format {
            DXGI_FORMAT_NV12 => (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM),
            DXGI_FORMAT_P010 => (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM),
            // planar Y, U, V in one texture (pictures decoded on the CPU): one view serves all
            DXGI_FORMAT_R16_UNORM => (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16_UNORM),
            _ => return Err(Error::Unsupported("surface format other than NV12, P010 and planar R16")),
        };
        let key = (tex.as_raw() as usize, slice);
        if let Some((_, y, c)) = self.views.get(&key) {
            return Ok((y.clone(), c.clone(), desc.Width, desc.Height, desc.Format));
        }
        if self.views.len() > 64 {
            self.views.clear();
        }
        let make = |f: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
            let mut vd = D3D11_SHADER_RESOURCE_VIEW_DESC { Format: f, ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY, ..Default::default() };
            vd.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_SRV { MostDetailedMip: 0, MipLevels: 1, FirstArraySlice: slice, ArraySize: 1 };
            let mut v = None;
            gpu("CreateShaderResourceView", unsafe { self.device.CreateShaderResourceView(tex, Some(&vd), Some(&mut v)) })?;
            v.ok_or(Error::Gpu("no view".into()))
        };
        let (y, c) = (make(fy)?, make(fc)?);
        self.views.insert(key, (tex.clone(), y.clone(), c.clone()));
        Ok((y, c, desc.Width, desc.Height, desc.Format))
    }

    fn constants(src: &Source, p: &Params, surf: (u32, u32)) -> Constants {
        let bd = src.bit_depth as i32;
        let maxv = ((1u32 << bd) - 1) as f64;
        let k = (1u32 << (bd - 8)) as f64;
        let (yoff, ygain, cgain) = if p.colour.full_range { (0.0, 1.0, 1.0) } else { (16.0 * k / maxv, maxv / (219.0 * k), maxv / (224.0 * k)) };
        let cmid = 128.0 * k / maxv;
        let (kr, kb) = p.colour.matrix.k();
        let kg = 1.0 - kr - kb;
        let sample_scale = if bd > 8 { 65535.0 / (((1u32 << (16 - bd)) as f64) * maxv) } else { 1.0 };
        let rot = match p.rotation % 360 {
            90 => 1.0,
            180 => 2.0,
            270 => 3.0,
            _ => 0.0,
        };
        let tf = match p.colour.transfer {
            Transfer::Sdr => 0.0,
            Transfer::Pq => 1.0,
            Transfer::Hlg => 2.0,
        };
        Constants {
            surf: [surf.0 as f32, surf.1 as f32, 0.0, 0.0],
            crop: [src.left as f32, src.top as f32, src.width as f32, src.height as f32],
            yp: [yoff as f32, ygain as f32, cmid as f32, cgain as f32],
            coef: [(2.0 * (1.0 - kr)) as f32, (2.0 * kb * (1.0 - kb) / kg) as f32, (2.0 * kr * (1.0 - kr) / kg) as f32, (2.0 * (1.0 - kb)) as f32],
            misc: [sample_scale as f32, rot, if p.flip { 1.0 } else { 0.0 }, tf],
            gamut: [if p.colour.bt2020_primaries && p.colour.transfer != Transfer::Sdr { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            planar: [0.0; 4],
            vorg: [0.0; 4],
        }
    }

    /// Shows interlaced pictures with the two fields blended (a simple linear deinterlace in the
    /// pixel shader, off by default). The decoder gives the fields woven into one frame; where
    /// they were captured 1/50 s apart, moving edges show a comb that this removes at the price
    /// of some vertical sharpness. Not a motion adaptive filter and not a bob (one picture per
    /// field): frames come out at the frame rate. Applies to pictures converted afterwards.
    pub fn set_deinterlace(&mut self, on: bool) {
        self.deinterlace = on;
    }

    /// Wraps an existing BGRA8 render-target texture (a swap chain's back buffer) as a target.
    pub fn target_for(&self, texture: &ID3D11Texture2D) -> Result<Target> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut d);
            if d.Format != DXGI_FORMAT_B8G8R8A8_UNORM || d.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0 {
                return Err(Error::Unsupported("target texture other than a BGRA8 render target"));
            }
            let mut rtv = None;
            gpu("CreateRenderTargetView", self.device.CreateRenderTargetView(texture, None, Some(&mut rtv)))?;
            Ok(Target { texture: texture.clone(), rtv: rtv.ok_or(Error::Gpu("no view".into()))?, width: d.Width, height: d.Height })
        }
    }

    /// Fills the whole target with a colour (red, green, blue, alpha; 0..1).
    pub fn clear(&self, dst: &Target, rgba: [f32; 4]) {
        unsafe { self.context.ClearRenderTargetView(&dst.rtv, &rgba) };
    }

    /// Draws `src` into `dst`, scaled to the whole target. Commands go to the device's
    /// immediate context; nothing waits for the GPU.
    pub fn convert(&mut self, src: &Source, params: &Params, dst: &Target) -> Result<()> {
        self.convert_rect(src, params, dst, (0, 0, dst.width, dst.height))
    }

    /// Like [`Converter::convert`], into the rectangle `(x, y, width, height)` of the target
    /// only (a letterboxed picture); the rest of the target is left as it is.
    pub fn convert_rect(&mut self, src: &Source, params: &Params, dst: &Target, rect: (i32, i32, u32, u32)) -> Result<()> {
        let (ysrv, csrv, sw, sh, format) = self.planes(&src.texture, src.slice)?;
        if src.left + src.width > sw || src.top + src.height > sh {
            return Err(Error::Invalid("visible rectangle outside the surface"));
        }
        let mut c = Self::constants(src, params, (sw, sh));
        c.gamut[1] = if self.deinterlace { 1.0 } else { 0.0 };
        let planar = format == DXGI_FORMAT_R16_UNORM;
        if planar {
            // Y plane rows = visible rows; the texture is 2x that (4:2:2) or 3x (4:4:4) high
            let hy = src.top + src.height;
            if sh == 2 * hy {
                let cw = (sw / 2) as f32;
                c.planar = [1.0, hy as f32, cw, 0.0];
                c.vorg = [cw, 0.0, 0.0, 0.0];
            } else if sh == 3 * hy {
                c.planar = [0.0, hy as f32, sw as f32, 0.0];
                c.vorg = [0.0, hy as f32, 0.0, 0.0];
            } else {
                return Err(Error::Invalid("planar picture: texture height is neither 2 nor 3 times the picture height"));
            }
        }
        unsafe {
            let ctx = self.context.clone();
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            gpu("Map (constants)", ctx.Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)))?;
            std::ptr::copy_nonoverlapping(&c as *const Constants as *const u8, m.pData as *mut u8, std::mem::size_of::<Constants>());
            ctx.Unmap(&self.cbuf, 0);

            let timer = self.timer_begin();
            ctx.OMSetRenderTargets(Some(&[Some(dst.rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: rect.0 as f32, TopLeftY: rect.1 as f32, Width: rect.2 as f32, Height: rect.3 as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.RSSetState(&self.raster);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(if planar { &self.ps_planar } else { &self.ps }, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(ysrv), Some(csrv)]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None, None]));
            ctx.OMSetRenderTargets(None, None);
            self.timer_end(timer);
        }
        Ok(())
    }

    fn timer_begin(&mut self) -> Option<usize> {
        self.collect_timers();
        unsafe {
            if self.timers.len() < 4 {
                let mk = |q: D3D11_QUERY| -> Option<ID3D11Query> {
                    let mut o = None;
                    self.device.CreateQuery(&D3D11_QUERY_DESC { Query: q, MiscFlags: 0 }, Some(&mut o)).ok()?;
                    o
                };
                let (d, b, e) = (mk(D3D11_QUERY_TIMESTAMP_DISJOINT)?, mk(D3D11_QUERY_TIMESTAMP)?, mk(D3D11_QUERY_TIMESTAMP)?);
                self.timers.push(TimerSet { disjoint: d, begin: b, end: e, busy: false });
            }
            let n = self.timers.len();
            let i = (0..n).map(|k| (self.next_timer + k) % n).find(|&i| !self.timers[i].busy)?;
            self.next_timer = i + 1;
            let t = &self.timers[i];
            self.context.Begin(&t.disjoint);
            self.context.End(&t.begin);
            Some(i)
        }
    }

    fn timer_end(&mut self, t: Option<usize>) {
        let Some(i) = t else { return };
        unsafe {
            self.context.End(&self.timers[i].end);
            self.context.End(&self.timers[i].disjoint);
        }
        self.timers[i].busy = true;
    }

    fn collect_timers(&mut self) {
        for t in &mut self.timers {
            if !t.busy {
                continue;
            }
            unsafe {
                let mut dj = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT { Frequency: 0, Disjoint: BOOL(0) };
                let _ = self.context.GetData(&t.disjoint, Some(&mut dj as *mut _ as *mut c_void), std::mem::size_of_val(&dj) as u32, 0);
                if dj.Frequency == 0 {
                    continue; // not finished (S_FALSE leaves the data alone)
                }
                let (mut a, mut b) = (u64::MAX, u64::MAX);
                let _ = self.context.GetData(&t.begin, Some(&mut a as *mut _ as *mut c_void), 8, 0);
                let _ = self.context.GetData(&t.end, Some(&mut b as *mut _ as *mut c_void), 8, 0);
                t.busy = false;
                if !dj.Disjoint.as_bool() && a != u64::MAX && b != u64::MAX && b >= a {
                    self.gpu_ns += (b - a) as f64 * 1e9 / dj.Frequency as f64;
                    self.gpu_samples += 1;
                }
            }
        }
    }

    /// Average GPU time of one conversion in milliseconds, and how many were measured (from
    /// timestamp queries read back a few frames late).
    pub fn gpu_time(&mut self) -> (f64, u64) {
        self.collect_timers();
        if self.gpu_samples == 0 { (0.0, 0) } else { (self.gpu_ns / self.gpu_samples as f64 / 1e6, self.gpu_samples) }
    }

    /// Copies a target to the CPU as RGBA bytes, row after row (for tests). Waits for the GPU.
    pub fn read_rgba(&mut self, t: &Target) -> Result<Vec<u8>> {
        unsafe {
            if self.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (t.width, t.height)) {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: t.width,
                    Height: t.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                };
                let mut s = None;
                gpu("CreateTexture2D (staging)", self.device.CreateTexture2D(&d, None, Some(&mut s)))?;
                self.staging = Some((s.ok_or(Error::Gpu("no texture".into()))?, t.width, t.height));
            }
            let staging = &self.staging.as_ref().unwrap().0;
            self.context.CopyResource(staging, &t.texture);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            gpu("Map", self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)))?;
            let mut out = Vec::with_capacity((t.width * t.height * 4) as usize);
            for row in 0..t.height as usize {
                let line = std::slice::from_raw_parts((m.pData as *const u8).add(row * m.RowPitch as usize), t.width as usize * 4);
                for px in line.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            self.context.Unmap(staging, 0);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_from_vui() {
        let mut v = Vui { colour_primaries: 2, transfer_characteristics: 2, matrix_coefficients: 2, ..Default::default() };
        assert_eq!(Colorimetry::from_vui(&v, 1920, 1080).matrix, Matrix::Bt709);
        assert_eq!(Colorimetry::from_vui(&v, 640, 360).matrix, Matrix::Bt601);
        v.matrix_coefficients = 9;
        v.colour_primaries = 9;
        v.transfer_characteristics = 16;
        let c = Colorimetry::from_vui(&v, 3840, 2160);
        assert_eq!((c.matrix, c.transfer, c.bt2020_primaries), (Matrix::Bt2020, Transfer::Pq, true));
    }

    #[test]
    fn sizes() {
        assert_eq!(rotated_size(1920, 1080, 90), (1080, 1920));
        assert_eq!(rotated_size(1920, 1080, 180), (1920, 1080));
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(1080, 1920, 1920, 1080), (608, 1080));
        assert_eq!(fit(640, 360, 1920, 1080), (640, 360));
    }
}
