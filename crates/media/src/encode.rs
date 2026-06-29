//! VA-API H.264 encoder: import a received dmabuf (DMA_DRM) into an appsrc and
//! encode to Annex-B H.264. One encoder per monitor of the selected clone.
//!
//! - [`ChromaMode::Yuv420`] (default): the standard single pipeline
//!   `appsrc → vapostproc → NV12 → vah264enc`, one `W×H` 4:2:0 stream.
//! - [`ChromaMode::Yuv444`]: full chroma via the AVC444 double-height trick, as a **single
//!   zero-copy GPU pipeline**: `appsrc(dmabuf) → glupload → rmngavc444pack →
//!   vapostproc(VAMemory NV12) → vah264enc`. The custom [`crate::glpack`] element packs main+aux
//!   into one stacked `W×2H` NV12 frame and renders it straight into a VA-allocated dmabuf
//!   surface — colour-convert, pack and GL→VA bridge fused into one GPU pass (byte-compatible
//!   with [`wire::avc444`]); only the compressed AU leaves the GPU. The viewer reassembles
//!   4:4:4. Requires the headless-GL env set in [`crate::init`].

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use wire::ChromaMode;

pub struct Encoder {
    /// dmabuf input (both modes feed this).
    appsrc: AppSrc,
    /// The whole encode pipeline (where `vah264enc` lives, so `force_idr` targets it).
    pipeline: gst::Pipeline,
    cur: Mutex<Option<(u32, u64, u32, u32)>>, // fourcc, modifier, w, h — input caps gate
}

/// DRM fourcc (e.g. 0x34325241) → "AR24".
fn fourcc_str(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).trim_end_matches('\0').to_string()
}

/// `vah264enc → h264parse → appsink` tail, shared by both modes.
///
/// `target-usage=1` is deliberate and **counterintuitive**: on this AMD VCN the usage mapping is
/// effectively inverted — `tu=1` (the "quality" preset) encodes the stacked 2880p Yuv444 frame at
/// ~71fps while `tu=7` (the "speed" preset) manages only ~41fps (measured via `avc444_e2e bench`).
/// `tu=1` is what lets the Yuv444 path keep up with the 60Hz capture; Yuv420 has headroom either way.
const ENC_TAIL: &str = "vah264enc name=enc aud=true b-frames=0 ref-frames=1 key-int-max=30 \
       rate-control=cqp qpi=23 qpp=25 target-usage=1 ! \
     video/x-h264,profile=constrained-baseline ! \
     h264parse config-interval=-1 ! \
     video/x-h264,stream-format=byte-stream,alignment=au ! \
     appsink name=out emit-signals=true max-buffers=4 sync=false";

impl Encoder {
    /// `on_au(annexb, is_idr)` is called from a GStreamer thread per access unit.
    pub fn new<F: FnMut(Vec<u8>, bool) + Send + 'static>(
        chroma: ChromaMode,
        on_au: F,
    ) -> Result<Self> {
        match chroma {
            ChromaMode::Yuv420 => Self::new_yuv420(on_au),
            ChromaMode::Yuv444 => Self::new_yuv444(on_au),
        }
    }

    fn new_yuv420<F: FnMut(Vec<u8>, bool) + Send + 'static>(on_au: F) -> Result<Self> {
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true ! \
             vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {ENC_TAIL}"
        );
        let pipeline = launch_pipeline(&desc)?;
        let appsrc = by_name_appsrc(&pipeline, "src")?;
        attach_au_sink(&pipeline, on_au)?;
        pipeline.set_state(gst::State::Playing).context("encoder PLAYING")?;
        Ok(Self { appsrc, pipeline, cur: Mutex::new(None) })
    }

    fn new_yuv444<F: FnMut(Vec<u8>, bool) + Send + 'static>(on_au: F) -> Result<Self> {
        // Register our GPU packer before referencing it by name.
        crate::glpack::register()?;
        // Single zero-copy GPU pipeline: `glupload` imports the capture dmabuf as a GL texture,
        // then `rmngavc444pack` (colour-convert + AVC444 pack + GL→VA bridge fused into one pass)
        // renders the packed stacked-NV12 planes straight into a VA-allocated dmabuf surface —
        // GstGL's gldownload can't export AMD's tiled GL textures in a layout VA accepts, so we
        // invert ownership (VA allocates, GL writes in place), which vapostproc/vah264enc consume.
        // No per-frame host transfers. (push() attaches a VideoMeta so glupload can EGL-import the
        // bare compositor dmabuf zero-copy — without it glupload derives 0 planes and CPU-mmaps.)
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true ! \
             glupload ! rmngavc444pack ! \
             vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! {ENC_TAIL}"
        );
        let pipeline = launch_pipeline(&desc)?;
        let appsrc = by_name_appsrc(&pipeline, "src")?;
        attach_au_sink(&pipeline, on_au)?;
        pipeline.set_state(gst::State::Playing).context("encoder PLAYING")?;
        Ok(Self { appsrc, pipeline, cur: Mutex::new(None) })
    }

    /// Force the next encoded frame to be an IDR (keyframe). The event reaches `vah264enc`
    /// in the single pipeline.
    pub fn force_idr(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.pipeline.send_event(ev);
    }

    /// Push one captured dmabuf frame. `fd` is consumed (the GstMemory owns it).
    pub fn push(&self, fd: OwnedFd, fourcc: u32, modifier: u64, w: u32, h: u32) -> Result<()> {
        {
            let mut cur = self.cur.lock().unwrap();
            if *cur != Some((fourcc, modifier, w, h)) {
                // GStreamer's drm-format modifier is `0x` + 16 zero-padded hex digits; a
                // non-padded value (`{:#x}`) is a different *string* and fails caps matching.
                let drm = format!("{}:{:#018x}", fourcc_str(fourcc), modifier);
                let caps = gst::Caps::builder("video/x-raw")
                    .features(["memory:DMABuf"])
                    .field("format", "DMA_DRM")
                    .field("drm-format", drm.as_str())
                    .field("width", w as i32)
                    .field("height", h as i32)
                    .build();
                self.appsrc.set_caps(Some(&caps));
                *cur = Some((fourcc, modifier, w, h));
            }
        }
        // Size of the underlying dmabuf (lseek SEEK_END is the canonical query).
        let raw = fd.as_raw_fd();
        let size = nix::unistd::lseek(raw, 0, nix::unistd::Whence::SeekEnd).context("lseek dmabuf")? as usize;
        let allocator = gstreamer_allocators::DmaBufAllocator::new();
        // SAFETY: `fd` is a unique owned dmabuf fd; the GstMemory takes ownership.
        let mem = unsafe { allocator.alloc(fd, size) }.map_err(|e| anyhow!("dmabuf alloc: {e}"))?;
        let mut buffer = gst::Buffer::new();
        {
            let b = buffer.get_mut().unwrap();
            b.append_memory(mem);
            // Attach plane metadata so GL import (glupload, Yuv444 path) works: the bare
            // dmabuf buffer carries none, so glupload derives 0 planes and falls back to a
            // failing CPU mmap. The capture is single-plane 32bpp; stride = width·4 (the
            // nominal pitch — the AMD tiling is conveyed by the modifier in the caps). VA
            // (Yuv420 path) ignores the meta and derives the layout itself, so this is safe
            // for both modes.
            let vfmt = match fourcc_str(fourcc).as_str() {
                "AB24" | "XB24" => gstreamer_video::VideoFormat::Rgba,
                _ => gstreamer_video::VideoFormat::Bgra, // AR24/XR24 (ARGB/xRGB) and default
            };
            let _ = gstreamer_video::VideoMeta::add_full(
                b,
                gstreamer_video::VideoFrameFlags::empty(),
                vfmt,
                w,
                h,
                &[0],
                &[(w * 4) as i32],
            );
        }
        self.appsrc.push_buffer(buffer).map_err(|e| anyhow!("push_buffer: {e:?}"))?;
        Ok(())
    }
}

fn launch_pipeline(desc: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))
}

fn by_name_appsrc(p: &gst::Pipeline, name: &str) -> Result<AppSrc> {
    p.by_name(name).with_context(|| format!("appsrc {name}"))?.downcast().map_err(|_| anyhow!("not appsrc"))
}

/// Wire the `out` appsink to call `on_au(annexb, is_idr)` per access unit.
fn attach_au_sink<F: FnMut(Vec<u8>, bool) + Send + 'static>(p: &gst::Pipeline, mut on_au: F) -> Result<()> {
    let appsink: AppSink =
        p.by_name("out").context("appsink out")?.downcast().map_err(|_| anyhow!("not appsink"))?;
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    let idr = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    if let Ok(map) = buf.map_readable() {
                        on_au(map.as_slice().to_vec(), idr);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}
