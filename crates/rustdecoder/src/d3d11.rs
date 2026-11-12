//! D3D11 Video: the device, the decoder for a profile, the output texture array and the
//! per-picture submission (`DecoderBeginFrame`, buffers, `DecoderEndFrame`).

use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
const E_PENDING: windows::core::HRESULT = windows::core::HRESULT(0x8000000Au32 as i32);
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory6};
use windows::core::{BOOL, GUID, Interface};

use crate::{Error, Result};

fn gpu<T>(what: &str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| Error::Gpu(format!("{what}: {e}")))
}

/// The decoder profile and surface format for a stream of the given bit depth (HEVC).
pub fn hevc_profile(bit_depth: u8) -> Result<(GUID, DXGI_FORMAT)> {
    match bit_depth {
        8 => Ok((D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, DXGI_FORMAT_NV12)),
        10 => Ok((D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, DXGI_FORMAT_P010)),
        _ => Err(Error::Unsupported("HEVC bit depth other than 8 and 10")),
    }
}

/// DXVA_NoEncrypt: the configuration takes the bitstream in the clear.
const DXVA_NO_ENCRYPT: GUID = GUID::from_u128(0x1b81bed0_a0c7_11d3_b984_00c04f2e73c5);

/// The decoder profile and surface format for H.264 (8-bit 4:2:0, no film grain).
pub fn h264_profile() -> (GUID, DXGI_FORMAT) {
    (D3D11_DECODER_PROFILE_H264_VLD_NOFGT, DXGI_FORMAT_NV12)
}

/// A D3D11 device with video support, on an adapter that decodes the profile asked for.
#[derive(Clone)]
pub struct Gpu {
    /// `None` on the idle copy kept in the pool.
    lease: Option<std::sync::Arc<Lease>>,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub video_device: ID3D11VideoDevice,
    pub video_context: ID3D11VideoContext,
    pub adapter_name: String,
}

/// Devices no video uses right now, kept for the next one: making a device per video left two
/// handles behind each time on this driver (and costs tens of milliseconds). A device is never
/// shared by two videos at once (their decode work on one immediate context got mixed up).
struct Idle(Vec<Gpu>);
// SAFETY: an idle device is touched by no thread; it is handed to one video at a time.
unsafe impl Send for Idle {}
static IDLE: std::sync::Mutex<Idle> = std::sync::Mutex::new(Idle(Vec::new()));

/// Goes with every clone of a leased [`Gpu`]; when the last one is gone, the device goes back
/// to [`IDLE`].
struct Lease(Option<Gpu>);

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(g) = self.0.take() {
            // what the video released is freed now (D3D11 destroys on the next flush)
            unsafe { g.context.Flush() };
            IDLE.lock().unwrap_or_else(|e| e.into_inner()).0.push(g);
        }
    }
}

// SAFETY: as for `Gpu`, which already crosses threads with the decoder (see `player.rs`).
unsafe impl Send for Lease {}
unsafe impl Sync for Lease {}

impl Gpu {
    /// The first hardware adapter (fastest first) whose video engine decodes `profile` into
    /// `format`.
    pub fn open(profile: &GUID, format: DXGI_FORMAT) -> Result<Gpu> {
        let reused = {
            let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
            idle.0.iter().position(|g| g.decodes(profile, format)).map(|i| idle.0.swap_remove(i))
        };
        let core = match reused {
            Some(g) => g,
            None => {
                let g = Self::open_new(profile, format)?;
                // the decoder thread, the converter and the window share the immediate context
                if let Ok(mt) = g.context.cast::<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>() {
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true);
                    }
                }
                g
            }
        };
        let mut leased = core.clone();
        leased.lease = Some(std::sync::Arc::new(Lease(Some(core))));
        Ok(leased)
    }

    /// A device on the first hardware adapter, for pictures decoded on the CPU (ProRes): no
    /// decoder profile is asked for; the device only holds the textures the pictures are
    /// uploaded to and is the one the converter draws on.
    pub fn open_any() -> Result<Gpu> {
        let reused = IDLE.lock().unwrap_or_else(|e| e.into_inner()).0.pop();
        let core = match reused {
            Some(g) => g,
            None => {
                let g = Self::open_new_any()?;
                if let Ok(mt) = g.context.cast::<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>() {
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true);
                    }
                }
                g
            }
        };
        let mut leased = core.clone();
        leased.lease = Some(std::sync::Arc::new(Lease(Some(core))));
        Ok(leased)
    }

    fn open_new_any() -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory6 = gpu("CreateDXGIFactory1", CreateDXGIFactory1())?;
            let mut why = String::from("no adapter");
            for i in 0.. {
                let Ok(adapter) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) else { break };
                let desc = gpu("GetDesc1", adapter.GetDesc1())?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
                let mut device = None;
                let mut context = None;
                let made = D3D11CreateDevice(
                    &adapter.cast::<IDXGIAdapter>().map_err(|e| Error::Gpu(e.to_string()))?,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                );
                if let Err(e) = made {
                    why = format!("{name}: {e}");
                    continue;
                }
                let (Some(device), Some(context)) = (device, context) else { continue };
                let video_device: ID3D11VideoDevice = match device.cast() {
                    Ok(v) => v,
                    Err(e) => {
                        why = format!("{name}: {e}");
                        continue;
                    }
                };
                let video_context: ID3D11VideoContext = match context.cast() {
                    Ok(v) => v,
                    Err(e) => {
                        why = format!("{name}: {e}");
                        continue;
                    }
                };
                return Ok(Gpu { lease: None, device, context, video_device, video_context, adapter_name: name });
            }
            Err(Error::Gpu(format!("no hardware adapter ({why})")))
        }
    }

    /// Whether this device's video engine decodes `profile` into `format`.
    fn decodes(&self, profile: &GUID, format: DXGI_FORMAT) -> bool {
        unsafe {
            let n = self.video_device.GetVideoDecoderProfileCount();
            (0..n).any(|i| self.video_device.GetVideoDecoderProfile(i).is_ok_and(|g| g == *profile))
                && self.video_device.CheckVideoDecoderFormat(profile, format).is_ok_and(|b| b.as_bool())
        }
    }

    fn open_new(profile: &GUID, format: DXGI_FORMAT) -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory6 = gpu("CreateDXGIFactory1", CreateDXGIFactory1())?;
            let mut why = String::from("no adapter");
            for i in 0.. {
                let Ok(adapter) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) else { break };
                let desc = gpu("GetDesc1", adapter.GetDesc1())?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
                match Self::on_adapter(&adapter.cast::<IDXGIAdapter>().map_err(|e| Error::Gpu(e.to_string()))?, profile, format, name.clone()) {
                    Ok(g) => return Ok(g),
                    Err(e) => why = format!("{name}: {e}"),
                }
            }
            Err(Error::Gpu(format!("no adapter decodes this stream ({why})")))
        }
    }

    unsafe fn on_adapter(adapter: &IDXGIAdapter, profile: &GUID, format: DXGI_FORMAT, name: String) -> Result<Gpu> {
        let mut device = None;
        let mut context = None;
        gpu(
            "D3D11CreateDevice",
            unsafe {
                D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            },
        )?;
        let (device, context) = (device.ok_or(Error::Gpu("no device".into()))?, context.ok_or(Error::Gpu("no context".into()))?);
        let video_device: ID3D11VideoDevice = gpu("ID3D11VideoDevice", device.cast())?;
        let video_context: ID3D11VideoContext = gpu("ID3D11VideoContext", context.cast())?;
        let n = unsafe { video_device.GetVideoDecoderProfileCount() };
        let mut found = false;
        for i in 0..n {
            if unsafe { video_device.GetVideoDecoderProfile(i) }.is_ok_and(|g| g == *profile) {
                found = true;
            }
        }
        if !found {
            return Err(Error::Gpu("decoder profile not offered".into()));
        }
        if !gpu("CheckVideoDecoderFormat", unsafe { video_device.CheckVideoDecoderFormat(profile, format) })?.as_bool() {
            return Err(Error::Gpu("output format not supported by the decoder".into()));
        }
        Ok(Gpu { lease: None, device, context, video_device, video_context, adapter_name: name })
    }

    /// Blocks until the GPU has finished everything submitted so far.
    pub fn wait_idle(&self) -> Result<()> {
        unsafe {
            let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
            let mut q = None;
            gpu("CreateQuery", self.device.CreateQuery(&desc, Some(&mut q)))?;
            let q = q.ok_or(Error::Gpu("no query".into()))?;
            self.context.End(&q);
            self.context.Flush();
            let mut done = BOOL(0);
            while !done.as_bool() {
                gpu("GetData", self.context.GetData(&q, Some(&mut done as *mut BOOL as *mut c_void), 4, 0))?;
                if !done.as_bool() {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            }
        }
        Ok(())
    }
}

/// The decoder, with the texture array its pictures are written into.
pub struct HwDecoder {
    pub gpu_adapter: String,
    decoder: ID3D11VideoDecoder,
    views: Vec<ID3D11VideoDecoderOutputView>,
    pub texture: ID3D11Texture2D,
    pub format: DXGI_FORMAT,
    /// Size of the surfaces (the coded size, rounded up).
    pub width: u32,
    pub height: u32,
    staging: Option<ID3D11Texture2D>,
    /// ConfigBitstreamRaw of the configuration in use: 1 is the short slice format.
    pub config_bitstream_raw: u32,
}

/// Rounds a coded size up to what the surfaces need.
pub fn surface_size(coded: u32) -> u32 {
    coded.div_ceil(128) * 128
}

impl HwDecoder {
    pub fn new(g: &Gpu, profile: &GUID, format: DXGI_FORMAT, coded_width: u32, coded_height: u32, surfaces: usize) -> Result<Self> {
        // D3D11 frees released objects on the next flush: the previous video's decoder and
        // surfaces (several megabytes) go now, not whenever the shared device flushes next
        unsafe { g.context.Flush() };
        Self::with_alignment(g, profile, format, coded_width, coded_height, surfaces, 128, 1)
    }

    /// Like [`HwDecoder::new`], with the surface size rounded up to a multiple of `align`
    /// (128 for HEVC, 16 for H.264). `short_raw` is the ConfigBitstreamRaw value that means the
    /// short slice format for the codec: 1 for HEVC, but 2 for H.264 (where 1 is the long one).
    #[allow(clippy::too_many_arguments)]
    pub fn with_alignment(g: &Gpu, profile: &GUID, format: DXGI_FORMAT, coded_width: u32, coded_height: u32, surfaces: usize, align: u32, short_raw: u32) -> Result<Self> {
        let width = coded_width.div_ceil(align) * align;
        let height = coded_height.div_ceil(align) * align;
        let desc = D3D11_VIDEO_DECODER_DESC { Guid: *profile, SampleWidth: width, SampleHeight: height, OutputFormat: format };
        unsafe {
            let n = gpu("GetVideoDecoderConfigCount", g.video_device.GetVideoDecoderConfigCount(&desc))?;
            let mut chosen = None;
            for i in 0..n {
                let mut cfg = D3D11_VIDEO_DECODER_CONFIG::default();
                gpu("GetVideoDecoderConfig", g.video_device.GetVideoDecoderConfig(&desc, i, &mut cfg))?;
                log::debug!("decoder config {i}: ConfigBitstreamRaw {}, min render targets {}", cfg.ConfigBitstreamRaw, cfg.ConfigMinRenderTargetBuffCount);
                if cfg.ConfigBitstreamRaw == short_raw && cfg.guidConfigBitstreamEncryption == DXVA_NO_ENCRYPT && chosen.is_none() {
                    chosen = Some(cfg);
                }
            }
            let cfg = chosen.ok_or(Error::Unsupported("this GPU decodes this codec only with the long slice format"))?;
            let decoder = gpu("CreateVideoDecoder", g.video_device.CreateVideoDecoder(&desc, &cfg))?;

            let tdesc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: surfaces as u32,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            if g.device.CreateTexture2D(&tdesc, None, Some(&mut tex)).is_err() {
                let t2 = D3D11_TEXTURE2D_DESC { BindFlags: D3D11_BIND_DECODER.0 as u32, ..tdesc };
                gpu("CreateTexture2D", g.device.CreateTexture2D(&t2, None, Some(&mut tex)))?;
            }
            let texture = tex.ok_or(Error::Gpu("no texture".into()))?;
            let mut views = Vec::with_capacity(surfaces);
            for i in 0..surfaces {
                let mut vd = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC { DecodeProfile: *profile, ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D, ..Default::default() };
                vd.Anonymous.Texture2D = D3D11_TEX2D_VDOV { ArraySlice: i as u32 };
                let mut v = None;
                gpu("CreateVideoDecoderOutputView", g.video_device.CreateVideoDecoderOutputView(&texture, &vd, Some(&mut v)))?;
                views.push(v.ok_or(Error::Gpu("no output view".into()))?);
            }
            Ok(Self { gpu_adapter: g.adapter_name.clone(), decoder, views, texture, format, width, height, staging: None, config_bitstream_raw: cfg.ConfigBitstreamRaw })
        }
    }

    pub fn surfaces(&self) -> usize {
        self.views.len()
    }

    unsafe fn put(g: &Gpu, dec: &ID3D11VideoDecoder, ty: D3D11_VIDEO_DECODER_BUFFER_TYPE, data: &[u8], pad_to: usize) -> Result<usize> {
        let mut size = 0u32;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        gpu("GetDecoderBuffer", unsafe { g.video_context.GetDecoderBuffer(dec, ty, &mut size, &mut ptr) })?;
        let total = if pad_to > 1 { data.len().next_multiple_of(pad_to) } else { data.len() };
        if total > size as usize || ptr.is_null() {
            let _ = unsafe { g.video_context.ReleaseDecoderBuffer(dec, ty) };
            return Err(Error::Gpu(format!("decoder buffer type {} holds {size} bytes, need {total}", ty.0)));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
            std::ptr::write_bytes((ptr as *mut u8).add(data.len()), 0, total - data.len());
        }
        gpu("ReleaseDecoderBuffer", unsafe { g.video_context.ReleaseDecoderBuffer(dec, ty) })?;
        Ok(total)
    }

    /// Decodes one picture into `surface`. `slices` is the DXVA slice control array (short
    /// format) as bytes, `bitstream` the slice NAL units each behind `00 00 01`. The last
    /// slice's size is grown over the zero padding the buffer gets.
    pub fn decode(&mut self, g: &Gpu, surface: usize, pic_params: &[u8], qmatrix: Option<&[u8]>, slices: &mut [crate::dxva::SliceShort], bitstream: &[u8]) -> Result<()> {
        // the bitstream's padding changes the last slice
        let padded = bitstream.len().next_multiple_of(128);
        if let Some(last) = slices.last_mut() {
            last.slice_bytes_in_buffer += (padded - bitstream.len()) as u32;
        }
        self.decode_controls(g, surface, pic_params, qmatrix, crate::dxva::slice_bytes(slices), bitstream)
    }

    /// Like [`HwDecoder::decode`] with the slice control buffer given as bytes, untouched (AV1:
    /// one `DXVA_Tile_AV1` per tile).
    pub fn decode_controls(&mut self, g: &Gpu, surface: usize, pic_params: &[u8], qmatrix: Option<&[u8]>, controls: &[u8], bitstream: &[u8]) -> Result<()> {
        let view = self.views.get(surface).ok_or(Error::Invalid("surface index"))?;
        let ctx = &g.video_context;
        unsafe {
            let mut tries = 0;
            loop {
                match ctx.DecoderBeginFrame(&self.decoder, view, 0, None) {
                    Ok(()) => break,
                    Err(e) if (e.code() == DXGI_ERROR_WAS_STILL_DRAWING || e.code() == E_PENDING) && tries < 2000 => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_micros(500));
                    }
                    Err(e) => return Err(Error::Gpu(format!("DecoderBeginFrame: {e}"))),
                }
            }
            let result = (|| -> Result<()> {
                let mut descs: Vec<D3D11_VIDEO_DECODER_BUFFER_DESC> = Vec::new();
                let mb_count = (self.width.div_ceil(16) * self.height.div_ceil(16)) as u32;
                let n = Self::put(g, &self.decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, pic_params, 1)?;
                descs.push(D3D11_VIDEO_DECODER_BUFFER_DESC { BufferType: D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, DataSize: n as u32, ..Default::default() });
                if let Some(q) = qmatrix {
                    let n = Self::put(g, &self.decoder, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, q, 1)?;
                    descs.push(D3D11_VIDEO_DECODER_BUFFER_DESC { BufferType: D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, DataSize: n as u32, ..Default::default() });
                }
                // the bitstream first (its padding changes the last slice), then the controls
                let n = Self::put(g, &self.decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, bitstream, 128)?;
                let m = Self::put(g, &self.decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, controls, 1)?;
                descs.push(D3D11_VIDEO_DECODER_BUFFER_DESC { BufferType: D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, DataSize: m as u32, NumMBsInBuffer: mb_count, ..Default::default() });
                descs.push(D3D11_VIDEO_DECODER_BUFFER_DESC { BufferType: D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, DataSize: n as u32, NumMBsInBuffer: mb_count, ..Default::default() });
                gpu("SubmitDecoderBuffers", ctx.SubmitDecoderBuffers(&self.decoder, &descs))
            })();
            let end = ctx.DecoderEndFrame(&self.decoder);
            result?;
            gpu("DecoderEndFrame", end)
        }
    }

    /// Copies a picture to the CPU as planes of 16-bit samples (8-bit pictures use the low
    /// byte), cropped to `crop` = (left, right, top, bottom) in luma samples out of the coded
    /// `coded` size. For tests; the pictures normally stay on the GPU.
    pub fn read_back(&mut self, g: &Gpu, surface: usize, coded: (u32, u32), crop: (u32, u32, u32, u32)) -> Result<Planes> {
        unsafe {
            if self.staging.is_none() {
                let d = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: self.format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                gpu("CreateTexture2D (staging)", g.device.CreateTexture2D(&d, None, Some(&mut t)))?;
                self.staging = t;
            }
            let staging = self.staging.as_ref().unwrap();
            g.context.CopySubresourceRegion(staging, 0, 0, 0, 0, &self.texture, surface as u32, None);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            gpu("Map", g.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)))?;
            let high = self.format == DXGI_FORMAT_P010;
            let (l, r, t, b) = crop;
            let (w, h) = (coded.0 - l - r, coded.1 - t - b);
            let base = m.pData as *const u8;
            let pitch = m.RowPitch as usize;
            let sample = |row: usize, col: usize, plane_row0: usize| -> u16 {
                let p = base.add((plane_row0 + row) * pitch + col * if high { 2 } else { 1 });
                if high { u16::from_le_bytes([*p, *p.add(1)]) >> 6 } else { *p as u16 }
            };
            let mut y = Vec::with_capacity((w * h) as usize);
            for row in 0..h as usize {
                for col in 0..w as usize {
                    y.push(sample(row + t as usize, col + l as usize, 0));
                }
            }
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut uv = Vec::with_capacity((cw * ch * 2) as usize);
            for row in 0..ch as usize {
                for col in 0..(cw * 2) as usize {
                    uv.push(sample(row + t as usize / 2, col + l as usize, self.height as usize));
                }
            }
            g.context.Unmap(staging, 0);
            Ok(Planes { width: w, height: h, bit_depth: if high { 10 } else { 8 }, y, uv })
        }
    }
}

/// A picture on the CPU: luma, and interleaved chroma (U, V, U, V, …), one `u16` per sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planes {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub y: Vec<u16>,
    pub uv: Vec<u16>,
}

/// The decoder profile and surface format for an AV1 stream of the given bit depth (main
/// profile: 8-bit and 10-bit 4:2:0).
pub fn av1_profile(bit_depth: u8) -> Result<(GUID, DXGI_FORMAT)> {
    match bit_depth {
        8 => Ok((D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_NV12)),
        10 => Ok((D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_P010)),
        _ => Err(Error::Unsupported("AV1 bit depth other than 8 and 10")),
    }
}

/// The decoder profile and surface format for a VP9 stream of the given bit depth (profile 0
/// is 8-bit 4:2:0, profile 2 is 10-bit 4:2:0).
pub fn vp9_profile(bit_depth: u8) -> Result<(GUID, DXGI_FORMAT)> {
    match bit_depth {
        8 => Ok((D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0, DXGI_FORMAT_NV12)),
        10 => Ok((D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2, DXGI_FORMAT_P010)),
        _ => Err(Error::Unsupported("VP9 bit depth other than 8 and 10")),
    }
}
