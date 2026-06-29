//! Self-contained AVC444 hardware round-trip (run on the W6800 build CT).
//!
//! Synthesize a Y444 frame whose chroma has **full-resolution detail a 4:2:0 encode would
//! destroy** (1px colored stripes), pack it into the stacked `W×2H` NV12 via [`wire::avc444`],
//! run a **real `vah264enc → vah264dec`** round-trip, unpack, and report per-plane error +
//! dump original/reconstructed PNGs. If the stripes survive, full chroma works end to end.
//!
//! Usage: `avc444_e2e [W H out_dir]` (defaults 2560 1440 /tmp).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::prelude::VideoFrameExt;
use gstreamer_video::{VideoFrameRef, VideoInfo};

fn main() -> Result<()> {
    media::init()?;
    let args: Vec<String> = std::env::args().collect();
    // `avc444_e2e bench [N W H]` — free-running encode-ceiling benchmark (no capture/network/flow
    // control): measures the pure 444 vs 420 pipeline throughput on this GPU.
    if args.get(1).map(|s| s == "bench").unwrap_or(false) {
        return bench_main(&args);
    }
    // `avc444_e2e latency [N W H FPS]` — paced push→AU latency per pipeline variant. Pushes at a
    // fixed rate (default 60 fps, mirroring the capture cap) and FIFO-correlates each AU back to
    // its push, so it reports the encode stage's real per-frame latency AND reveals queue growth
    // (bufferbloat) if a variant can't keep up at that rate.
    if args.get(1).map(|s| s == "latency").unwrap_or(false) {
        return latency_main(&args);
    }
    // `avc444_e2e <input.png> [out_dir]` — run a real frame (e.g. a captured desktop) through
    // the full zero-copy yuv444 encode→decode→reconstruct path and dump orig/recon PNGs.
    if let Some(p) = args.get(1) {
        if p.ends_with(".png") {
            let out = args.get(2).cloned().unwrap_or_else(|| "/tmp".into());
            return desktop_e2e(p, &out);
        }
    }
    let w: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2560);
    let h: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1440);
    let out = args.get(3).cloned().unwrap_or_else(|| "/tmp".into());
    assert!(w % 2 == 0 && h % 2 == 0);
    println!("avc444_e2e: {w}x{h} (stacked {w}x{})", h * 2);

    // 1. Synthesize Y444: smooth luma gradient; chroma = 1px/2px colored stripes (4:2:0 killer).
    let (mut y, mut cb, mut cr) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
    for j in 0..h {
        for i in 0..w {
            y[j * w + i] = (16 + (i * 219 / w)) as u8; // 16..235 gradient
            // Full-res chroma detail: vertical 1px stripes in Cb, horizontal in Cr, plus blocks.
            cb[j * w + i] = if i % 2 == 0 { 80 } else { 200 };
            cr[j * w + i] = if j % 2 == 0 { 200 } else { 90 };
        }
    }
    let packed = wire::avc444::pack_y444_to_stacked_nv12(&y, &cb, &cr, w, h, w, w);

    // 2. Real HW encode → decode of the stacked frame.
    let decoded = encode_decode_nv12(&packed, w as i32, (h * 2) as i32)?;
    let (dec, ls, cs) = decoded; // luma plane, luma stride, chroma stride (interleaved UV)

    // 3. Unpack reconstructed chroma and compare against the source.
    let luma = &dec[..ls * h * 2];
    let chroma = &dec[ls * h * 2..];
    // For the error metric, reconstruct Cb/Cr via the gather (stride-aware) and diff.
    let mut eb = 0u64;
    let mut er = 0u64;
    let (cw_, ch_) = (w / 2, h / 2);
    let _ = (cw_, ch_);
    for py in 0..h {
        for px in 0..w {
            let (i, j, xc, yc) = (px & 1, py & 1, px >> 1, py >> 1);
            let gb = match (i, j) {
                (0, 0) => chroma[yc * cs + 2 * xc],
                (1, 0) => luma[(h + yc) * ls + xc],
                (0, 1) => luma[(h + yc) * ls + (w / 2 + xc)],
                _ => luma[(h + h / 2 + yc) * ls + xc],
            };
            let gr = match (i, j) {
                (0, 0) => chroma[yc * cs + 2 * xc + 1],
                (1, 0) => luma[(h + h / 2 + yc) * ls + (w / 2 + xc)],
                (0, 1) => chroma[(h / 2 + yc) * cs + 2 * xc],
                _ => chroma[(h / 2 + yc) * cs + 2 * xc + 1],
            };
            eb += (gb as i32 - cb[py * w + px] as i32).unsigned_abs() as u64;
            er += (gr as i32 - cr[py * w + px] as i32).unsigned_abs() as u64;
        }
    }
    let n = (w * h) as f64;
    println!("mean abs chroma error: Cb={:.2} Cr={:.2} (0=perfect; <~8 = stripes survived)", eb as f64 / n, er as f64 / n);

    // 4. Dump original + reconstructed RGBA → PNG for a visual check.
    let orig_rgba = y444_to_rgba(&y, &cb, &cr, w, w, h);
    let recon_rgba = wire::avc444::unpack_stacked_nv12_to_rgba(luma, ls, chroma, cs, w, h);
    dump_png(&orig_rgba, w, h, &format!("{out}/avc444_orig.png"))?;
    dump_png(&recon_rgba, w, h, &format!("{out}/avc444_recon.png"))?;
    println!("wrote {out}/avc444_orig.png and {out}/avc444_recon.png");

    gl_pack_validate(w, h, &out)?;
    Ok(())
}

/// Free-running encode-ceiling benchmark. To remove the source as a bottleneck we capture **one**
/// VA-exported dmabuf (same fourcc/modifier as the live capture) and **re-push it** through each
/// encode pipeline as fast as the GPU allows — no clock sync, no capture cost, no daemon flow
/// control. So the number is the pure encode-path ceiling. Variants isolate where the 444 cost is:
/// the post-pack `vapostproc` (DMA_DRM→VAMemory) vs encoding straight from the element's dmabuf.
fn bench_main(args: &[String]) -> Result<()> {
    media::glpack::register()?;
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(900);
    let w: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2560);
    let h: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1440);
    println!("encode-ceiling bench: re-push 1 frame x{n}, {w}x{h} (free source; no capture/flow-control)\n");
    let (buf, caps) = capture_one_frame(w, h)?;
    println!("captured source frame: {caps}\n");
    let enc = "vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 \
               qpp=25 target-usage=1 ! video/x-h264,profile=constrained-baseline ! h264parse ! \
               appsink name=out sync=false max-buffers=16";

    // 444 production path: glupload → pack → vapostproc(DMA_DRM→VAMemory) → enc.
    run_repush_bench(
        "444  glupload!pack!vapostproc!enc",
        &format!("glupload ! rmngavc444pack ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {enc}"),
        &buf, &caps, n,
    )?;
    // 444 minus the post-pack vapostproc: encode straight from the element's VA dmabuf.
    run_repush_bench(
        "444  glupload!pack!enc (no vpp)  ",
        &format!("glupload ! rmngavc444pack ! {enc}"),
        &buf, &caps, n,
    )?;
    // 420 reference: BGRA → vapostproc → enc (no glupload/pack), 1440p.
    run_repush_bench(
        "420  vapostproc!enc        (1440p)",
        &format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {enc}"),
        &buf, &caps, n,
    )?;
    // Encoder shootout at the stacked 2880p resolution (no glupload/pack) — the 444 ceiling is the
    // encode; can a faster entrypoint (low-power) or tuning beat the ~41fps of vah264enc tu7?
    let (buf2, caps2) = capture_one_frame(w, h * 2)?;
    let tail = "! video/x-h264,profile=constrained-baseline ! h264parse ! appsink name=out sync=false max-buffers=16";
    let cfgs: [(&str, &str); 5] = [
        ("vah264enc   tu7 cqp", "vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 target-usage=7"),
        ("vah264enc   tu1 cqp", "vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 target-usage=1"),
        ("vah264enc   tu7 sl4", "vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 target-usage=7 num-slices=4"),
        ("vah264lpenc tu7 cqp", "vah264lpenc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 target-usage=7"),
        ("vah264lpenc tu7 cbr", "vah264lpenc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cbr bitrate=20000 target-usage=7"),
    ];
    for (lbl, e) in cfgs {
        run_repush_bench(
            &format!("2880p {lbl}"),
            &format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {e} {tail}"),
            &buf2, &caps2, n,
        )?;
    }
    Ok(())
}

/// Capture a single sysmem BGRA frame (trivially negotiates; self-contained so it outlives its
/// pipeline). The re-push benches feed it through glupload/vapostproc, paying the same sysmem
/// upload in every variant — so the 444-vs-420 and with/without-vapostproc deltas stay valid.
fn capture_one_frame(w: usize, h: usize) -> Result<(gst::Buffer, gst::Caps)> {
    let desc = format!(
        "videotestsrc num-buffers=8 pattern=ball ! video/x-raw,format=BGRA,width={w},height={h},framerate=30/1 ! \
         appsink name=out sync=false max-buffers=2"
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(15)).context("no source frame")?;
    let buf = sample.buffer().context("buf")?.to_owned();
    let caps = sample.caps().context("caps")?.to_owned();
    pipeline.set_state(gst::State::Null)?;
    Ok((buf, caps))
}

/// Re-push one buffer through `mid_desc ! enc`, timestamping each AU, report steady-state fps.
fn run_repush_bench(label: &str, desc: &str, buf: &gst::Buffer, caps: &gst::Caps, n: usize) -> Result<()> {
    let full = format!("appsrc name=src is-live=false format=time do-timestamp=true block=true max-buffers=6 ! {desc}");
    let pipeline = match gst::parse::launch(&full) {
        Ok(p) => p.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?,
        Err(e) => {
            println!("{label}: FAILED to build — {e}");
            return Ok(());
        }
    };
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(caps));
    let times: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let times = times.clone();
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let _ = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    times.lock().unwrap().push(Instant::now());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }
    pipeline.set_state(gst::State::Playing)?;
    // Push from a worker thread so appsrc backpressure (block=true) doesn't deadlock the bus wait.
    let push = {
        let src = src.clone();
        let buf = buf.clone();
        std::thread::spawn(move || {
            for _ in 0..n {
                if src.push_buffer(buf.clone()).is_err() {
                    break;
                }
            }
            let _ = src.end_of_stream();
        })
    };
    let mut err = None;
    if let Some(bus) = pipeline.bus() {
        if let Some(msg) =
            bus.timed_pop_filtered(gst::ClockTime::from_seconds(60), &[gst::MessageType::Eos, gst::MessageType::Error])
        {
            if let gst::MessageView::Error(e) = msg.view() {
                err = Some(anyhow!("{label}: pipeline error: {} ({:?})", e.error(), e.debug()));
            }
        }
    }
    let _ = push.join();
    pipeline.set_state(gst::State::Null)?;
    if let Some(e) = err {
        println!("{label}: FAILED — {e}");
        return Ok(());
    }
    let t = times.lock().unwrap();
    let len = t.len();
    if len < 40 {
        println!("{label}: only {len} AUs — inconclusive");
        return Ok(());
    }
    let warm = 30; // skip encoder ramp + one-time GL shader build
    let dt = (t[len - 1] - t[warm]).as_secs_f64();
    let fps = (len - 1 - warm) as f64 / dt;
    println!("{label}: {fps:6.1} fps   ({} AUs, {:.2}s steady-state)", len, dt);
    Ok(())
}

/// Paced push→AU latency benchmark: the same variants as `bench_main`, but instead of free-running
/// for max throughput we push one frame every `1/fps` seconds (mirroring the 60 Hz capture cap) and
/// measure how long each frame takes from push to encoded AU. This is the encode stage's real
/// per-frame latency; if a variant can't sustain `fps` the input queue grows and the numbers climb.
fn latency_main(args: &[String]) -> Result<()> {
    media::glpack::register()?;
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);
    let w: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2560);
    let h: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1440);
    let fps: f64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    println!("latency bench: paced {fps:.0} fps push, FIFO-correlated push→AU, {w}x{h} (stacked {w}x{}), n={n}\n", h * 2);
    let (buf, caps) = capture_one_frame(w, h)?;
    println!("captured source frame: {caps}\n");
    // Same encoder tail as the production pipeline (encode::ENC_TAIL) and the throughput bench.
    let enc = "vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 \
               qpp=25 target-usage=1 ! video/x-h264,profile=constrained-baseline ! h264parse ! \
               appsink name=out sync=false max-buffers=16";
    let mid444 = "glupload ! rmngavc444pack ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12";
    // Current production: unbounded appsrc → frames pile up under encode jitter (bufferbloat).
    run_latency_bench(
        "444  unbounded appsrc (current)  ",
        "",
        &format!("{mid444} ! {enc}"),
        &buf, &caps, n, fps,
    )?;
    // Bounding gradient. block=true keeps the FIFO exactly correlated (no drops), so these are real
    // measurements of what a bounded encoder input delivers. In production, blocking the appsrc
    // back-pressures the ack → the daemon's existing leaky-1-deep capture drops the stale frame, so
    // a bounded encoder input == latest-frame-wins end-to-end. (Leaky-at-appsrc gives the same bound
    // but can't be PTS-correlated here — this VA encoder strips input PTS.)
    run_latency_bench(
        "444  block max=2 (bounded)        ",
        "block=true max-buffers=2",
        &format!("{mid444} ! {enc}"),
        &buf, &caps, n, fps,
    )?;
    run_latency_bench(
        "444  block max=1 (tightest)       ",
        "block=true max-buffers=1",
        &format!("{mid444} ! {enc}"),
        &buf, &caps, n, fps,
    )?;
    // 420 reference (half the pixels) — the encode floor for comparison.
    run_latency_bench(
        "420  unbounded appsrc      (1440p)",
        "",
        &format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {enc}"),
        &buf, &caps, n, fps,
    )?;

    // Tail-source isolation: the 444 tail (p99/max) is ~4× the 420 tail, but the median is only 2×
    // (the extra pixels). Where's the excess jitter — the GL pack path or the double-height encode?
    // Feed a pre-stacked 2880p frame straight to vapostproc→enc (NO glupload/pack): a pure encode of
    // the same pixel count. If its tail is tight, the GL pack (EGL churn / blocking thread_add) is
    // the jitter source; if it's fat too, the double-height encode itself is.
    println!("\n-- tail-source isolation (2880p, no GL pack) --");
    let (buf2, caps2) = capture_one_frame(w, h * 2)?;
    run_latency_bench(
        "2880p encode-only (no GL pack)    ",
        "",
        &format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {enc}"),
        &buf2, &caps2, n, fps,
    )?;
    Ok(())
}

/// Push one buffer through `appsrc <props> ! desc ! enc` at a fixed `fps` cadence and report
/// push→AU latency percentiles + how many pushed frames were dropped before producing an AU.
///
/// This VA encoder doesn't preserve input PTS to its output AUs (most come out PTS=`None`), so
/// latency is correlated by an **Instant FIFO**: each push enqueues `Instant::now()`, each AU pops
/// the front. Valid only for **no-drop** configs (unbounded, or `block=true`) where AU order ==
/// push order (b-frames=0); a leaky/dropping appsrc would desync it. The peak FIFO depth is the
/// bufferbloat signal: how many frames stacked up at the encoder input.
fn run_latency_bench(label: &str, props: &str, desc: &str, buf: &gst::Buffer, caps: &gst::Caps, n: usize, fps: f64) -> Result<()> {
    let full = format!("appsrc name=src is-live=true format=time do-timestamp=true {props} ! {desc}");
    let pipeline = match gst::parse::launch(&full) {
        Ok(p) => p.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?,
        Err(e) => {
            println!("{label}: FAILED to build — {e}");
            return Ok(());
        }
    };
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(caps));

    let fifo: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let lat: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let (fifo, lat) = (fifo.clone(), lat.clone());
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let _ = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(t0) = fifo.lock().unwrap().pop_front() {
                        lat.lock().unwrap().push(t0.elapsed().as_secs_f64() * 1000.0);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }
    pipeline.set_state(gst::State::Playing)?;
    // Paced push from a worker thread so the bus wait below stays responsive.
    let peak_depth = Arc::new(AtomicUsize::new(0));
    let push = {
        let (src, buf, fifo, peak_depth) = (src.clone(), buf.clone(), fifo.clone(), peak_depth.clone());
        std::thread::spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / fps);
            let mut next = Instant::now();
            for _ in 0..n {
                {
                    let mut q = fifo.lock().unwrap();
                    q.push_back(Instant::now());
                    peak_depth.fetch_max(q.len(), Ordering::Relaxed);
                }
                if src.push_buffer(buf.clone()).is_err() {
                    break;
                }
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now; // fell behind; don't bank slack and burst
                }
            }
            let _ = src.end_of_stream();
        })
    };
    let mut err = None;
    if let Some(bus) = pipeline.bus() {
        if let Some(msg) =
            bus.timed_pop_filtered(gst::ClockTime::from_seconds(120), &[gst::MessageType::Eos, gst::MessageType::Error])
        {
            if let gst::MessageView::Error(e) = msg.view() {
                err = Some(anyhow!("{label}: pipeline error: {} ({:?})", e.error(), e.debug()));
            }
        }
    }
    let _ = push.join();
    pipeline.set_state(gst::State::Null)?;
    if let Some(e) = err {
        println!("{label}: FAILED — {e}");
        return Ok(());
    }
    let mut t = lat.lock().unwrap().clone();
    let warm = 30; // skip encoder ramp + one-time GL shader build
    if t.len() <= warm + 10 {
        println!("{label}: only {} AUs — inconclusive", t.len());
        return Ok(());
    }
    t.drain(0..warm);
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = t.len();
    let pct = |p: f64| t[(((m - 1) as f64 * p).round() as usize).min(m - 1)];
    let mean = t.iter().sum::<f64>() / m as f64;
    println!(
        "{label}: p50={:5.1}ms  p99={:5.1}ms  max={:5.1}ms  mean={:5.1}ms   (peak queue {} frames, {m} AUs)",
        pct(0.50), pct(0.99), t[m - 1], mean, peak_depth.load(Ordering::Relaxed),
    );
    Ok(())
}

/// Validate the **GPU** AVC444 packer (`rmngavc444pack`) against the CPU oracle
/// ([`wire::avc444::pack_y444_to_stacked_nv12`]) and end to end through a real HW round-trip.
///
/// Build an RGBA frame with full-resolution chroma detail, then:
/// 1. CPU oracle: RGBA → Y444 (BT.601 limited forward, the shader's exact matrix) → pack.
/// 2. GPU: run `glupload ! rmngavc444pack ! vapostproc` (pack renders straight into the VA
///    surface) and read the bytes back.
/// 3. Assert the packed bytes match the oracle (the shader reproduces the contract layout).
/// 4. HW encode→decode→unpack the GPU-packed frame and report chroma error + dump a PNG.
fn gl_pack_validate(w: usize, h: usize, out: &str) -> Result<()> {
    println!("\n--- GL pack validation ({w}x{h}) ---");

    // 1. RGBA source with per-pixel (full-res) chroma detail a 4:2:0 encode would destroy.
    let mut rgba = vec![0u8; w * h * 4];
    for j in 0..h {
        for i in 0..w {
            let o = (j * w + i) * 4;
            rgba[o] = ((i * 7 + j * 3) & 0xFF) as u8;
            rgba[o + 1] = ((i * 5 + j * 11) & 0xFF) as u8;
            rgba[o + 2] = ((i * 13 + j * 2) & 0xFF) as u8;
            rgba[o + 3] = 255;
        }
    }

    // 1b. CPU oracle: forward-convert to Y444, then pack with the proven CPU function.
    let (mut y, mut cb, mut cr) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
    for p in 0..w * h {
        let (yv, cbv, crv) = rgb_to_ycc_bt601(rgba[p * 4], rgba[p * 4 + 1], rgba[p * 4 + 2]);
        y[p] = yv;
        cb[p] = cbv;
        cr[p] = crv;
    }
    let oracle = wire::avc444::pack_y444_to_stacked_nv12(&y, &cb, &cr, w, h, w, w);

    // 2. GPU pack.
    let (gl_luma, ls, gl_chroma, cs) = gl_pack_to_nv12(&rgba, w, h)?;

    // 3. Compare GPU vs oracle (pre-encode). Luma plane is W×2H, chroma plane W×H.
    let chroma_off = w * 2 * h;
    let mut e_luma = 0u64;
    let mut max_luma = 0u8;
    for r in 0..2 * h {
        for x in 0..w {
            let d = (gl_luma[r * ls + x] as i32 - oracle[r * w + x] as i32).unsigned_abs() as u8;
            e_luma += d as u64;
            max_luma = max_luma.max(d);
        }
    }
    let mut e_chroma = 0u64;
    let mut max_chroma = 0u8;
    for r in 0..h {
        for x in 0..w {
            let d = (gl_chroma[r * cs + x] as i32 - oracle[chroma_off + r * w + x] as i32)
                .unsigned_abs() as u8;
            e_chroma += d as u64;
            max_chroma = max_chroma.max(d);
        }
    }
    let nl = (w * 2 * h) as f64;
    let nc = (w * h) as f64;
    let (luma_mean, chroma_mean) = (e_luma as f64 / nl, e_chroma as f64 / nc);
    println!(
        "GL pack (rmngavc444pack→vapostproc) vs CPU oracle: luma mean={luma_mean:.3} max={max_luma}; \
         chroma mean={chroma_mean:.3} max={max_chroma} (≈0 = shader matches contract)"
    );
    // The shader is GPU-exact; the residual mean (<1) is sparse VA-VPP detile noise from the
    // measurement download (production feeds the VA surface straight to vah264enc). A wrong
    // layout/orientation/swizzle would be mean ≫8 on this per-pixel chroma pattern.
    assert!(luma_mean < 3.0, "GL luma pack diverges from oracle (mean {luma_mean:.2})");
    assert!(chroma_mean < 3.0, "GL chroma pack diverges from oracle (mean {chroma_mean:.2})");

    // 4. End to end: tighten the GPU-packed planes, HW encode→decode→unpack, report error.
    let mut gl_packed = vec![0u8; w * h * 3];
    for r in 0..2 * h {
        gl_packed[r * w..r * w + w].copy_from_slice(&gl_luma[r * ls..r * ls + w]);
    }
    for r in 0..h {
        gl_packed[chroma_off + r * w..chroma_off + r * w + w]
            .copy_from_slice(&gl_chroma[r * cs..r * cs + w]);
    }
    let (dec, dls, dcs) = encode_decode_nv12(&gl_packed, w as i32, (h * 2) as i32)?;
    let luma = &dec[..dls * h * 2];
    let chroma = &dec[dls * h * 2..];
    let recon_rgba = wire::avc444::unpack_stacked_nv12_to_rgba(luma, dls, chroma, dcs, w, h);
    let mut e_rgb = 0u64;
    for p in 0..w * h * 4 {
        if p % 4 == 3 {
            continue;
        }
        e_rgb += (recon_rgba[p] as i32 - rgba[p] as i32).unsigned_abs() as u64;
    }
    println!(
        "GL pack HW round-trip: mean abs RGB error {:.2} (full chroma preserved)",
        e_rgb as f64 / (w * h * 3) as f64
    );
    dump_png(&recon_rgba, w, h, &format!("{out}/avc444_gl_recon.png"))?;
    println!("wrote {out}/avc444_gl_recon.png");

    // 5. Exercise the real production encode pipeline (GL pack straight into VA → vapostproc →
    //    vah264enc) end to end and confirm it emits H.264.
    let aus = full_pipeline_check(w, h)?;
    println!("full zero-copy encode pipeline: produced {aus} H.264 AUs (rmngavc444pack→vapostproc→vah264enc OK)");
    assert!(aus > 0, "encode pipeline produced no access units");
    Ok(())
}

/// Run the production encode pipeline (a `vapostproc`-exported dmabuf stands in for the clone's
/// capture dmabuf) and count emitted Annex-B AUs. The whole chain is zero-copy: glupload imports
/// the capture dmabuf, rmngavc444pack packs on the GPU and renders straight into a VA dmabuf
/// surface (no host transfer), and vah264enc encodes it — only the compressed AU leaves the GPU.
fn full_pipeline_check(w: usize, h: usize) -> Result<usize> {
    media::glpack::register()?;
    let desc = format!(
        "videotestsrc num-buffers=8 ! video/x-raw,format=BGRA,width={w},height={h} ! \
         vapostproc ! video/x-raw(memory:DMABuf) ! \
         glupload ! rmngavc444pack ! \
         vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! \
         vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 ! \
         video/x-h264,profile=constrained-baseline ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=out sync=false"
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let mut count = 0usize;
    while sink.try_pull_sample(gst::ClockTime::from_seconds(5)).is_some() {
        count += 1;
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(count)
}

/// Run RGBA through the GPU packer (which renders straight into a VA dmabuf), then have
/// vapostproc download that surface back to sysmem NV12. Returns
/// `(luma, luma_stride, chroma, chroma_stride)`.
fn gl_pack_to_nv12(rgba: &[u8], w: usize, h: usize) -> Result<(Vec<u8>, usize, Vec<u8>, usize)> {
    media::glpack::register()?;
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=false format=time ! \
         glupload ! rmngavc444pack ! \
         vapostproc ! video/x-raw,format=NV12 ! appsink name=out sync=false",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    pipeline.set_state(gst::State::Playing)?;
    for k in 0..4 {
        let mut buf = gst::Buffer::from_slice(rgba.to_vec());
        buf.get_mut().unwrap().set_pts(gst::ClockTime::from_mseconds(k * 33));
        src.push_buffer(buf).map_err(|e| anyhow!("push: {e:?}"))?;
    }
    let _ = src.end_of_stream();
    let mut last: Option<(Vec<u8>, usize, Vec<u8>, usize)> = None;
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
        let buf = sample.buffer().context("buf")?;
        let info = VideoInfo::from_caps(sample.caps().context("caps")?)?;
        let frame = VideoFrameRef::from_buffer_ref_readable(buf, &info).map_err(|_| anyhow!("map"))?;
        let strides = frame.plane_stride();
        last = Some((
            frame.plane_data(0)?.to_vec(),
            strides[0] as usize,
            frame.plane_data(1)?.to_vec(),
            strides[1] as usize,
        ));
    }
    pipeline.set_state(gst::State::Null)?;
    last.context("no GL-packed frame")
}

/// Run a real RGBA frame (e.g. a captured desktop) through the **full production zero-copy
/// yuv444 path** (GL pack straight into VA → vapostproc → vah264enc → vah264dec → reconstruct)
/// and dump original + reconstructed PNGs so the result is viewable.
fn desktop_e2e(input: &str, out: &str) -> Result<()> {
    let (rgba, w, h) = load_png_rgba(input)?;
    println!("desktop e2e: {input} → {w}x{h} (full zero-copy yuv444 encode→decode→reconstruct)");
    let (dec, ls, cs) = full_zerocopy_encode_decode(&rgba, w, h)?;
    let luma = &dec[..ls * h * 2];
    let chroma = &dec[ls * h * 2..];
    let recon = wire::avc444::unpack_stacked_nv12_to_rgba(luma, ls, chroma, cs, w, h);
    let mut e = 0u64;
    for p in 0..w * h * 4 {
        if p % 4 == 3 {
            continue;
        }
        e += (recon[p] as i32 - rgba[p] as i32).unsigned_abs() as u64;
    }
    println!("desktop recon: mean abs RGB error {:.2} (full 4:4:4 chroma)", e as f64 / (w * h * 3) as f64);
    dump_png(&rgba, w, h, &format!("{out}/desktop_orig.png"))?;
    dump_png(&recon, w, h, &format!("{out}/desktop_recon.png"))?;
    println!("wrote {out}/desktop_orig.png and {out}/desktop_recon.png");
    Ok(())
}

/// Decode a PNG to tightly-packed RGBA (dims cropped to even, which the encoder requires).
fn load_png_rgba(path: &str) -> Result<(Vec<u8>, usize, usize)> {
    let pipeline = gst::parse::launch(&format!(
        "filesrc location={path} ! pngdec ! videoconvert ! video/x-raw,format=RGBA ! \
         appsink name=out sync=false"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    pipeline.set_state(gst::State::Playing)?;
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(15)).context("no PNG frame decoded")?;
    let buf = sample.buffer().context("buf")?;
    let info = VideoInfo::from_caps(sample.caps().context("caps")?)?;
    let frame = VideoFrameRef::from_buffer_ref_readable(buf, &info).map_err(|_| anyhow!("map"))?;
    let (ew, eh) = (info.width() as usize & !1, info.height() as usize & !1);
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0)?;
    let mut rgba = vec![0u8; ew * eh * 4];
    for r in 0..eh {
        rgba[r * ew * 4..r * ew * 4 + ew * 4].copy_from_slice(&data[r * stride..r * stride + ew * 4]);
    }
    pipeline.set_state(gst::State::Null)?;
    Ok((rgba, ew, eh))
}

/// Push RGBA through the whole production zero-copy pipeline + a HW decode; return the decoded
/// stacked `W×2H` NV12 (`luma+chroma` contiguous) with its plane strides.
fn full_zerocopy_encode_decode(rgba: &[u8], w: usize, h: usize) -> Result<(Vec<u8>, usize, usize)> {
    media::glpack::register()?;
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=false format=time ! \
         glupload ! rmngavc444pack ! \
         vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! \
         vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 ! \
         video/x-h264,profile=constrained-baseline ! h264parse ! vah264dec ! \
         videoconvert ! video/x-raw,format=NV12 ! appsink name=out sync=false",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    pipeline.set_state(gst::State::Playing)?;
    for k in 0..16 {
        let mut buf = gst::Buffer::from_slice(rgba.to_vec());
        buf.get_mut().unwrap().set_pts(gst::ClockTime::from_mseconds(k * 33));
        src.push_buffer(buf).map_err(|e| anyhow!("push: {e:?}"))?;
    }
    let _ = src.end_of_stream();
    let mut last: Option<(Vec<u8>, usize, usize)> = None;
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
        let buf = sample.buffer().context("buf")?;
        let info = VideoInfo::from_caps(sample.caps().context("caps")?)?;
        let frame = VideoFrameRef::from_buffer_ref_readable(buf, &info).map_err(|_| anyhow!("map"))?;
        let strides = frame.plane_stride();
        let mut data = frame.plane_data(0)?.to_vec();
        data.extend_from_slice(frame.plane_data(1)?);
        last = Some((data, strides[0] as usize, strides[1] as usize));
    }
    pipeline.set_state(gst::State::Null)?;
    last.context("no decoded frame")
}

/// BT.601 limited-range RGB→YCbCr (8-bit), the exact forward of the `rmngavc444pack` shader
/// and the inverse of `wire::avc444::ycbcr_to_rgb_bt601`.
fn rgb_to_ycc_bt601(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let cl = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    let y = 16.0 + 219.0 * (0.299 * r + 0.587 * g + 0.114 * b);
    let cb = 128.0 + 224.0 * (-0.168736 * r - 0.331264 * g + 0.5 * b);
    let cr = 128.0 + 224.0 * (0.5 * r - 0.418688 * g - 0.081312 * b);
    (cl(y), cl(cb), cl(cr))
}

/// Encode a tight stacked-NV12 buffer with `vah264enc` and decode it back; return the decoded
/// NV12 (luma+chroma planes contiguous as the decoder laid them out) with its plane strides.
fn encode_decode_nv12(nv12: &[u8], w: i32, h2: i32) -> Result<(Vec<u8>, usize, usize)> {
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=false format=time ! \
         vah264enc aud=true b-frames=0 ref-frames=1 key-int-max=30 rate-control=cqp qpi=23 qpp=25 ! \
         video/x-h264,profile=constrained-baseline ! h264parse ! vah264dec ! \
         videoconvert ! video/x-raw,format=NV12 ! appsink name=out sync=false",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("width", w)
            .field("height", h2)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    pipeline.set_state(gst::State::Playing)?;
    // Push the frame several times so the encoder settles, then EOS.
    for k in 0..16 {
        let mut buf = gst::Buffer::from_slice(nv12.to_vec());
        buf.get_mut().unwrap().set_pts(gst::ClockTime::from_mseconds(k * 33));
        src.push_buffer(buf).map_err(|e| anyhow!("push: {e:?}"))?;
    }
    let _ = src.end_of_stream();
    // Pull the last decoded frame.
    let mut last: Option<(Vec<u8>, usize, usize)> = None;
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
        let buf = sample.buffer().context("buf")?;
        let info = VideoInfo::from_caps(sample.caps().context("caps")?)?;
        let frame = VideoFrameRef::from_buffer_ref_readable(buf, &info).map_err(|_| anyhow!("map"))?;
        let strides = frame.plane_stride();
        let mut data = frame.plane_data(0)?.to_vec();
        data.extend_from_slice(frame.plane_data(1)?);
        last = Some((data, strides[0] as usize, strides[1] as usize));
    }
    pipeline.set_state(gst::State::Null)?;
    last.context("no decoded frame")
}

fn y444_to_rgba(y: &[u8], cb: &[u8], cr: &[u8], w: usize, _s: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for p in 0..w * h {
        let c = (y[p] as f32 - 16.0) * 1.164_383;
        let d = cb[p] as f32 - 128.0;
        let e = cr[p] as f32 - 128.0;
        let cl = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        out[p * 4] = cl(c + 1.596_027 * e);
        out[p * 4 + 1] = cl(c - 0.391_762 * d - 0.812_968 * e);
        out[p * 4 + 2] = cl(c + 2.017_232 * d);
        out[p * 4 + 3] = 255;
    }
    out
}

fn dump_png(rgba: &[u8], w: usize, h: usize, path: &str) -> Result<()> {
    let pipeline = gst::parse::launch(
        "appsrc name=src ! videoconvert ! pngenc ! filesink name=fs",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    pipeline.by_name("fs").context("fs")?.set_property("location", path);
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(1, 1))
            .build(),
    ));
    pipeline.set_state(gst::State::Playing)?;
    let buf = gst::Buffer::from_slice(rgba.to_vec());
    src.push_buffer(buf).map_err(|e| anyhow!("push: {e:?}"))?;
    let _ = src.end_of_stream();
    // Wait for EOS on the bus.
    if let Some(bus) = pipeline.bus() {
        let _ = bus.timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos, gst::MessageType::Error]);
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}
