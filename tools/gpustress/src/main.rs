//! GPU load generator for testing the smoothyfiles hybrid renderer.
//!
//! `gpustress --seconds N --intensity 0..100 [--mode ps|cs] [--no-pdh]`
//!
//! * `ps` (default): full-screen triangle with a heavy pixel shader into an offscreen render
//!   target. Loads the `engtype_3D` engine the way a game does.
//! * `cs`: heavy compute shader dispatches writing a UAV buffer (D3D11 may schedule this on the
//!   3D engine or on a separate Compute engine depending on the driver; the PDH output shows it).
//!
//! Intensity is a duty cycle over 100 ms windows. Hard time limit: default 20 s, max 120 s.
//! Ctrl+C stops cleanly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_UNKNOWN, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile},
                ID3DBlob,
            },
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        System::{
            Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler},
            Performance::{
                PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
                PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
                PdhOpenQueryW,
            },
        },
    },
    core::{BOOL, PCSTR, PCWSTR, s, w},
};

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn ctrl_handler(kind: u32) -> BOOL {
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        STOP.store(true, Ordering::SeqCst);
        return BOOL(1);
    }
    BOOL(0)
}

const RT_W: u32 = 2560;
const RT_H: u32 = 1440;

const VS_SRC: &str = "float4 main(uint id:SV_VertexID):SV_Position{\
 float2 uv=float2(id&2, (id<<1)&2); return float4(uv*2.0-1.0,0,1);}";

const PS_SRC: &str = "float4 main(float4 p:SV_Position):SV_Target{\
 float x=p.x*0.001, y=p.y*0.001;\
 [loop] for(int i=0;i<LOOPS;i++){ x=sin(x*1.0001+y)+cos(y*0.999-x); y=x*y*0.5+sin(y);}\
 return float4(x,y,x*y,1);}";

const CS_SRC: &str = "RWStructuredBuffer<float> o:register(u0);\
 [numthreads(256,1,1)] void main(uint3 id:SV_DispatchThreadID){\
 float x=id.x*0.001, y=id.x*0.002;\
 [loop] for(int i=0;i<LOOPS;i++){ x=sin(x*1.0001+y)+cos(y*0.999-x); y=x*y*0.5+sin(y);}\
 o[id.x]=x+y;}";

fn compile(src: &str, loops: u32, entry: PCSTR, target: PCSTR) -> windows::core::Result<Vec<u8>> {
    let src = src.replace("LOOPS", &loops.to_string());
    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            src.as_ptr() as _,
            src.len(),
            None,
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if let Err(e) = r {
        if let Some(err) = err {
            let msg = unsafe {
                std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize())
            };
            eprintln!("shader error: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let blob = blob.unwrap();
    Ok(unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }.to_vec())
}

struct Args {
    seconds: f64,
    intensity: f64,
    mode: String,
    pdh: bool,
    loops: u32,
    chunk_ms: f64,
}

fn parse_args() -> Args {
    let mut a = Args { seconds: 20.0, intensity: 100.0, mode: "ps".into(), pdh: true, loops: 256, chunk_ms: 4.0 };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{k} needs a value")));
        match k.as_str() {
            "--seconds" => a.seconds = val().parse().unwrap_or_else(|_| usage("bad --seconds")),
            "--intensity" => a.intensity = val().parse().unwrap_or_else(|_| usage("bad --intensity")),
            "--mode" => a.mode = val(),
            "--loops" => a.loops = val().parse().unwrap_or_else(|_| usage("bad --loops")),
            "--chunk-ms" => a.chunk_ms = val().parse().unwrap_or_else(|_| usage("bad --chunk-ms")),
            "--no-pdh" => a.pdh = false,
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    a.seconds = a.seconds.clamp(0.5, 120.0);
    a.intensity = a.intensity.clamp(0.0, 100.0);
    if a.mode != "ps" && a.mode != "cs" {
        usage("--mode must be ps or cs");
    }
    a
}

fn usage(msg: &str) -> ! {
    if !msg.is_empty() {
        eprintln!("error: {msg}");
    }
    eprintln!("usage: gpustress [--seconds N (0.5..120, default 20)] [--intensity 0..100] [--mode ps|cs] [--loops N] [--chunk-ms F] [--no-pdh]");
    std::process::exit(if msg.is_empty() { 0 } else { 2 });
}

/// PDH sampler over all `\GPU Engine(*)` instances; groups by engine type, sums processes per engine,
/// reports the busiest engine of each type.
struct Pdh {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    buf: Vec<u64>,
}

impl Pdh {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                return None;
            }
            PdhCollectQueryData(query);
            Some(Self { query, counter, buf: Vec::new() })
        }
    }

    /// Returns (type, busiest engine %) pairs.
    fn sample(&mut self) -> Option<Vec<(String, f64)>> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
                != PDH_MORE_DATA
            {
                return None;
            }
            self.buf.resize((size as usize).div_ceil(8) + 1, 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return None;
            }
            let items = std::slice::from_raw_parts(items, count as usize);
            // key: "luid_.._phys_0_eng_N" (without pid) + engine type
            let mut engines: Vec<(String, String, f64)> = Vec::new();
            for it in items {
                if it.FmtValue.CStatus != 0 {
                    continue;
                }
                let Ok(name) = it.szName.to_string() else { continue };
                let Some((_, rest)) = name.split_once("luid_") else { continue };
                let Some((key, ty)) = rest.split_once("_engtype_") else { continue };
                let v = it.FmtValue.Anonymous.doubleValue;
                match engines.iter_mut().find(|(k, t, _)| k == key && t == ty) {
                    Some((_, _, acc)) => *acc += v,
                    None => engines.push((key.to_owned(), ty.to_owned(), v)),
                }
            }
            let mut out: Vec<(String, f64)> = Vec::new();
            for (_, ty, v) in engines {
                let v = v.min(100.0);
                match out.iter_mut().find(|(t, _)| *t == ty) {
                    Some((_, m)) => *m = m.max(v),
                    None => out.push((ty, v)),
                }
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Some(out)
        }
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn pick_adapter() -> windows::core::Result<IDXGIAdapter1> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut best: Option<(usize, IDXGIAdapter1)> = None;
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let d = a.GetDesc1()?;
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_owned();
            println!("adapter {}: {} ({} MiB dedicated)", i - 1, name, d.DedicatedVideoMemory >> 20);
            if best.as_ref().is_none_or(|(m, _)| d.DedicatedVideoMemory > *m) {
                best = Some((d.DedicatedVideoMemory, a));
            }
        }
        best.map(|b| b.1).ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
    }
}

fn main() {
    let args = parse_args();
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true).ok() };
    if let Err(e) = run(&args) {
        eprintln!("gpustress failed: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> windows::core::Result<()> {
    unsafe {
        let adapter = pick_adapter()?;
        let (mut dev, mut ctx): (Option<ID3D11Device>, Option<ID3D11DeviceContext>) = (None, None);
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )?;
        let (dev, ctx) = (dev.unwrap(), ctx.unwrap());
        println!("mode={} intensity={}% seconds={} loops={}", args.mode, args.intensity, args.seconds, args.loops);

        let is_ps = args.mode == "ps";
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        let mut uav: Option<ID3D11UnorderedAccessView> = None;
        let groups: u32 = 8192; // 8192 * 256 threads
        if is_ps {
            let vs = compile(VS_SRC, 0, s!("main"), s!("vs_5_0"))?;
            let ps = compile(PS_SRC, args.loops, s!("main"), s!("ps_5_0"))?;
            let mut vso = None;
            dev.CreateVertexShader(&vs, None, Some(&mut vso))?;
            let mut pso = None;
            dev.CreatePixelShader(&ps, None, Some(&mut pso))?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: RT_W,
                Height: RT_H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut tex = None;
            dev.CreateTexture2D(&desc, None, Some(&mut tex))?;
            dev.CreateRenderTargetView(&tex.unwrap(), None, Some(&mut rtv))?;
            ctx.VSSetShader(vso.as_ref(), None);
            ctx.PSSetShader(pso.as_ref(), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: RT_W as f32,
                Height: RT_H as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            ctx.OMSetRenderTargets(Some(&[rtv.clone()]), None);
        } else {
            let cs = compile(CS_SRC, args.loops, s!("main"), s!("cs_5_0"))?;
            let mut cso = None;
            dev.CreateComputeShader(&cs, None, Some(&mut cso))?;
            let bdesc = D3D11_BUFFER_DESC {
                ByteWidth: groups * 256 * 4,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                StructureByteStride: 4,
                ..Default::default()
            };
            let mut buf = None;
            dev.CreateBuffer(&bdesc, None, Some(&mut buf))?;
            let udesc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_UNKNOWN,
                ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_UAV { FirstElement: 0, NumElements: groups * 256, Flags: 0 },
                },
            };
            dev.CreateUnorderedAccessView(&buf.unwrap(), Some(&udesc), Some(&mut uav))?;
            let uavs = [uav.clone()];
            ctx.CSSetShader(cso.as_ref(), None);
            ctx.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), None);
        }

        let submit = |n: u32| {
            for _ in 0..n {
                if is_ps {
                    ctx.Draw(3, 0);
                } else {
                    ctx.Dispatch(groups, 1, 1);
                }
            }
        };
        let mk_query = || -> windows::core::Result<ID3D11Query> {
            let mut q = None;
            dev.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q))?;
            Ok(q.unwrap())
        };
        let wait = |q: &ID3D11Query| {
            let mut done = BOOL(0);
            while ctx.GetData(q, Some(&mut done as *mut _ as *mut _), 4, 0).is_err() || !done.as_bool() {
                if STOP.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_micros(100));
            }
        };

        // Calibrate: how many units fit in a ~4 ms chunk.
        let q0 = mk_query()?;
        let mut per_chunk = 1u32;
        for _ in 0..6 {
            let t = Instant::now();
            submit(per_chunk);
            ctx.End(&q0);
            ctx.Flush();
            wait(&q0);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let next = ((per_chunk as f64) * args.chunk_ms / ms.max(0.05)).round().clamp(1.0, 65536.0) as u32;
            println!("calibrate: {per_chunk} units took {ms:.2} ms");
            per_chunk = next;
        }
        println!("chunk = {per_chunk} units (~4 ms)");

        let mut pdh = if args.pdh { Pdh::open() } else { None };
        if args.pdh && pdh.is_none() {
            eprintln!("warning: PDH GPU Engine counters unavailable");
        }

        let start = Instant::now();
        let limit = Duration::from_secs_f64(args.seconds);
        let period = Duration::from_millis(100);
        let busy = period.mul_f64(args.intensity / 100.0);
        let mut chunks: u64 = 0;
        let mut last_print = Instant::now();
        let mut chunks_at_print = 0u64;
        let mut stats: Vec<(String, f64, f64, u32)> = Vec::new(); // type, max, sum, n
        let queries = [mk_query()?, mk_query()?];
        let mut in_flight = [false; 2];
        let mut qi = 0usize;

        while !STOP.load(Ordering::Relaxed) && start.elapsed() < limit {
            let win = Instant::now();
            // Busy phase: keep up to 2 chunks queued so the GPU never starves.
            while win.elapsed() < busy && !STOP.load(Ordering::Relaxed) {
                if in_flight[qi] {
                    wait(&queries[qi]);
                    in_flight[qi] = false;
                }
                submit(per_chunk);
                ctx.End(&queries[qi]);
                ctx.Flush();
                in_flight[qi] = true;
                qi ^= 1;
                chunks += 1;
            }
            for i in 0..2 {
                if in_flight[i] {
                    wait(&queries[i]);
                    in_flight[i] = false;
                }
            }
            let rest = period.saturating_sub(win.elapsed());
            if !rest.is_zero() {
                std::thread::sleep(rest);
            }

            if last_print.elapsed() >= Duration::from_secs(1) {
                let dt = last_print.elapsed().as_secs_f64();
                let rate = (chunks - chunks_at_print) as f64 / dt;
                chunks_at_print = chunks;
                last_print = Instant::now();
                let mut line = format!("t={:5.1}s  dispatches/s={:7.1}", start.elapsed().as_secs_f64(), rate);
                if let Some(p) = pdh.as_mut() {
                    if let Some(s) = p.sample() {
                        for (ty, v) in &s {
                            line.push_str(&format!("  {ty}={v:5.1}%"));
                            match stats.iter_mut().find(|m| m.0 == *ty) {
                                Some(m) => {
                                    m.1 = m.1.max(*v);
                                    m.2 += v;
                                    m.3 += 1;
                                }
                                None => stats.push((ty.clone(), *v, *v, 1)),
                            }
                        }
                    }
                }
                println!("{line}");
            }
        }
        for i in 0..2 {
            if in_flight[i] {
                wait(&queries[i]);
            }
        }
        ctx.ClearState();
        ctx.Flush();
        let secs = start.elapsed().as_secs_f64();
        println!(
            "done: {:.1}s, {} chunks ({} units each), average {:.1} dispatches/s{}",
            secs,
            chunks,
            per_chunk,
            chunks as f64 / secs,
            if STOP.load(Ordering::Relaxed) { " (stopped by Ctrl+C)" } else { "" }
        );
        for (ty, max, sum, n) in stats {
            println!("PDH engtype_{ty}: max {max:.1}%  avg {:.1}%", sum / n as f64);
        }
    }
    Ok(())
}
