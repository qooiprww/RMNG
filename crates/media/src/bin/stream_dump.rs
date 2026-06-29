//! Tap the control-server's port-1 video stream, decode monitor 0's first frame, dump it as
//! PNG — a quick way to grab a **real desktop frame** from the live stream.
//!
//! Usage: `RMNG_VIDEO=127.0.0.1:9001 RMNG_DUMP=/tmp/desk.png [RMNG_MON=0] [RMNG_444=1] stream-dump`
//! - default (Yuv420): `videoconvert ! pngenc`s the decoded frame directly.
//! - `RMNG_444=1` (Yuv444/AVC444): decodes the stacked `W×2H` NV12 and reconstructs 4:4:4 RGBA
//!   via [`wire::avc444`] before encoding the PNG.

use std::io::Read;
use std::net::TcpStream;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::prelude::VideoFrameExt;
use gstreamer_video::{VideoFrameRef, VideoInfo};

fn main() -> Result<()> {
    media::init()?;
    let addr = std::env::var("RMNG_VIDEO").unwrap_or_else(|_| "127.0.0.1:9001".into());
    let out = std::env::var("RMNG_DUMP").unwrap_or_else(|_| "/tmp/desk.png".into());
    let target_mon: u32 = std::env::var("RMNG_MON").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let avc444 = std::env::var_os("RMNG_444").is_some();
    // `RMNG_COUNT=<secs>` (with RMNG_444): no PNG — decode+unpack every frame and report the
    // client's reconstruct fps, to verify the full e2e (decode + 4:4:4 unpack) sustains 60.
    let count_secs: Option<u64> = std::env::var("RMNG_COUNT").ok().and_then(|s| s.parse().ok());

    // In AVC444 mode decode to raw NV12 (we reconstruct 4:4:4 ourselves); else straight to PNG.
    let desc = if avc444 {
        "appsrc name=src is-live=true format=time do-timestamp=true ! h264parse ! vah264dec ! \
         videoconvert ! video/x-raw,format=NV12 ! appsink name=out emit-signals=true max-buffers=2 sync=false"
    } else {
        "appsrc name=src is-live=true format=time do-timestamp=true ! h264parse ! vah264dec ! \
         videoconvert ! pngenc ! appsink name=out emit-signals=true max-buffers=2 sync=false"
    };
    let pipeline = gst::parse::launch(desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?;
    let src: AppSrc = pipeline.by_name("src").context("src")?.downcast().map_err(|_| anyhow!("appsrc"))?;
    let sink: AppSink = pipeline.by_name("out").context("out")?.downcast().map_err(|_| anyhow!("appsink"))?;
    src.set_caps(Some(
        &gst::Caps::builder("video/x-h264").field("stream-format", "byte-stream").field("alignment", "au").build(),
    ));
    // Count mode: decode + unpack every frame, report reconstruct fps each second, exit after N.
    if let Some(secs) = count_secs.filter(|_| avc444) {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        let count = Arc::new(AtomicU64::new(0));
        {
            let count = count.clone();
            std::thread::spawn(move || {
                let mut last = 0u64;
                for i in 1..=secs {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let now = count.load(Ordering::Relaxed);
                    println!("[{i:2}s] client reconstruct fps: {}", now - last);
                    last = now;
                }
                println!("--- {} frames decoded+unpacked in {secs}s = {:.1} fps avg ---",
                    count.load(Ordering::Relaxed), count.load(Ordering::Relaxed) as f64 / secs as f64);
                std::process::exit(0);
            });
        }
        // RMNG_NOUNPACK: count decode+download only (skip the CPU unpack) to isolate the bottleneck.
        let nounpack = std::env::var_os("RMNG_NOUNPACK").is_some();
        let count = count.clone();
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    // Full client work: decode (done) → 4:4:4 unpack (discard the RGBA).
                    if nounpack {
                        let _ = sample.buffer().and_then(|b| b.map_readable().ok()); // force the download
                        count.fetch_add(1, Ordering::Relaxed);
                    } else if reconstruct(&sample).is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    } else {
        let outp = out.clone();
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if avc444 {
                        if let Err(e) = reconstruct_and_write(&sample, &outp) {
                            eprintln!("reconstruct failed: {e}");
                            std::process::exit(1);
                        }
                    } else if let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) {
                        std::fs::write(&outp, map.as_slice()).ok();
                        println!("wrote {outp} ({} bytes)", map.size());
                    }
                    std::process::exit(0);
                })
                .build(),
        );
    }
    pipeline.set_state(gst::State::Playing)?;

    let mut stream = TcpStream::connect(&addr).context("connect")?;
    stream.set_nodelay(true).ok();
    println!("connected to {addr}; waiting for monitor {target_mon} frames (avc444={avc444})…");
    let mut tag = [0u8; 1];
    while stream.read_exact(&mut tag).is_ok() {
        // tags 1..=4 are [u32 len][json] sidebands; skip them.
        if matches!(tag[0], 1 | 2 | 3 | 4) {
            let mut lb = [0u8; 4];
            stream.read_exact(&mut lb)?;
            let mut skip = vec![0u8; u32::from_be_bytes(lb) as usize];
            stream.read_exact(&mut skip)?;
            continue;
        }
        // video frame: [u32 monitor_id][u32 len][AnnexB]
        let mut hdr = [0u8; 8];
        stream.read_exact(&mut hdr)?;
        let mon = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        let len = u32::from_be_bytes(hdr[4..8].try_into().unwrap()) as usize;
        let mut au = vec![0u8; len];
        stream.read_exact(&mut au)?;
        if mon == target_mon {
            src.push_buffer(gst::Buffer::from_mut_slice(au)).ok();
        }
    }
    Err(anyhow!("stream ended before a frame decoded"))
}

/// Reconstruct 4:4:4 RGBA from a decoded stacked `W×2H` NV12 sample. Returns `(rgba, w, h)`.
fn reconstruct(sample: &gst::Sample) -> Result<(Vec<u8>, usize, usize)> {
    let buf = sample.buffer().context("buf")?;
    let info = VideoInfo::from_caps(sample.caps().context("caps")?)?;
    let frame = VideoFrameRef::from_buffer_ref_readable(buf, &info).map_err(|_| anyhow!("map"))?;
    let (w, h) = (info.width() as usize, info.height() as usize / 2);
    let strides = frame.plane_stride();
    let rgba = wire::avc444::unpack_stacked_nv12_to_rgba(
        frame.plane_data(0)?, strides[0] as usize, frame.plane_data(1)?, strides[1] as usize, w, h,
    );
    Ok((rgba, w, h))
}

/// Reconstruct 4:4:4 RGBA from a decoded stacked sample and write a PNG.
fn reconstruct_and_write(sample: &gst::Sample, path: &str) -> Result<()> {
    let (rgba, w, h) = reconstruct(sample)?;
    write_rgba_png(&rgba, w, h, path)?;
    println!("reconstructed {w}x{h} 4:4:4 → {path}");
    Ok(())
}

fn write_rgba_png(rgba: &[u8], w: usize, h: usize, path: &str) -> Result<()> {
    let pipeline = gst::parse::launch("appsrc name=src ! videoconvert ! pngenc ! filesink name=fs")?
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
    src.push_buffer(gst::Buffer::from_slice(rgba.to_vec())).map_err(|e| anyhow!("push: {e:?}"))?;
    let _ = src.end_of_stream();
    if let Some(bus) = pipeline.bus() {
        let _ = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}
