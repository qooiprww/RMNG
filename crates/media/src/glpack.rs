//! `rmngavc444pack` — one custom GstGL element that packs full-resolution RGBA into the
//! AVC444 stacked `W×2H` NV12 layout **and writes it straight into the encoder's VA surface**,
//! entirely on the GPU (zero host transfers).
//!
//! It fuses what used to be three pipeline stages — `glcolorconvert` (RGBA normalize),
//! `rmngavc444pack` (the GL pack into an intermediate NV12 texture) and `rmnggldmabuf` (the
//! GL→VA dmabuf bridge) — into a single pass:
//!
//! - **sink**: RGBA/BGRA `W×H` GLMemory (the captured image, imported by `glupload`).
//! - **src**: NV12 `W×2H` as a VA-allocated `memory:DMABuf` surface (the AMD tiled modifier
//!   `vapostproc`/`vah264enc` require).
//!
//! `transform` acquires the VA dmabuf from the downstream pool, imports each of its planes as a
//! GL texture (`eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)`), and **FBO-renders the two pack
//! fragment shaders directly into them** — luma + main chroma on top, the dropped chroma in the
//! bottom aux band. The shaders do BT.601 limited-range RGB→YCbCr inline (the colour convert,
//! so no `glcolorconvert`) and gather the exact polyphase quadrants
//! [`wire::avc444::pack_y444_to_stacked_nv12`] produces, so the bytes are byte-identical to that
//! oracle (modulo later H.264 quantization). No intermediate NV12 texture, no copy pass: the
//! packed pixels land in the encoder's own surface. The decoder reassembles 4:4:4.
//!
//! Why ownership is inverted (VA allocates, GL writes in place — the g-r-d pattern): the whole
//! VA stack here only imports NV12 dmabufs with the AMD tiled modifier `0x0200000020801b03`, and
//! GstGL's `gldownload` can't *export* AMD's tiled GL textures in any layout VA accepts. The
//! VA→GL dmabuf *import* direction works for that modifier, so we import and render into it.
//! amdgpu's implicit dmabuf fencing orders the GL write before the VA read.
//!
//! Pipeline position (see [`crate::encode`]): `glupload ! rmngavc444pack ! vapostproc ! vah264enc`.
//! Headless GL requires the GBM env set in [`crate::init`].

use std::sync::{Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::subclass::BaseTransformMode;
use gstreamer_base::subclass::prelude::*;
use gstreamer_gl as gst_gl;
use gstreamer_gl::prelude::*;
use gstreamer_gl::{GLMemory, GLSLProfile, GLSLStage, GLSLVersion, GLShader, GLVideoFrameRef};
use gstreamer_video::{VideoFormat, VideoInfo, VideoMeta};

/// `GL_FRAGMENT_SHADER` / `GL_VERTEX_SHADER` GLenums (no Rust constants in the crate).
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_VERTEX_SHADER: u32 = 0x8B31;

/// Fullscreen-triangle vertex shader (GLES3, VBO-less via `gl_VertexID`): emits `v_texcoord`
/// in `[0,1]` matching the viewport, so a fragment at framebuffer row 0 samples `v_texcoord.y≈0`.
const PACK_VERT: &str = r#"#version 300 es
out vec2 v_texcoord;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_texcoord = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// Fragment preamble up to (and including) the source fetch's swizzle selector: BT.601-limited
/// RGB→YCbCr (exact inverse of [`wire::avc444::ycbcr_to_rgb_bt601`]) and a texel-center point
/// sampler. The swizzle (`rgb`/`bgr`) follows immediately, then [`PACK_TAIL`], then a main body.
/// Splitting around the swizzle lets us fold `glcolorconvert` in: `glupload` may hand us RGBA or
/// BGRA, and `ycc_at` rewrites it to logical RGB before the matrix.
const PACK_HEADER: &str = r#"#version 300 es
precision highp float;
precision highp int;
in vec2 v_texcoord;
uniform sampler2D tex;
uniform float w;   // source image width  (W)
uniform float h;   // source image height (H)
out vec4 frag;

vec3 rgb_to_ycc(vec3 c) {
    float Y  = (16.0  + 219.0 * ( 0.299    * c.r + 0.587    * c.g + 0.114    * c.b)) / 255.0;
    float Cb = (128.0 + 224.0 * (-0.168736 * c.r - 0.331264 * c.g + 0.5      * c.b)) / 255.0;
    float Cr = (128.0 + 224.0 * ( 0.5      * c.r - 0.418688 * c.g - 0.081312 * c.b)) / 255.0;
    return vec3(Y, Cb, Cr);
}

// Point-sample the source image at integer pixel (sx,sy) and return its YCbCr.
vec3 ycc_at(float sx, float sy) {
    vec3 rgb = texture(tex, vec2((sx + 0.5) / w, (sy + 0.5) / h))."#;

const PACK_TAIL: &str = r#";
    return rgb_to_ycc(rgb);
}
"#;

/// Y-plane (R8, `W×2H`): rows [0,H) = main luma; rows [H,2H) = the 2×2 aux-luma tiling
/// `[Cb01|Cb10 ; Cb11|Cr01]` over `(W/2 × H/2)` blocks. Mirrors `pack_y444_to_stacked_nv12`.
const Y_MAIN: &str = r#"
void main() {
    float px = floor(v_texcoord.x * w);
    float r  = floor(v_texcoord.y * (2.0 * h));
    float val;
    if (r < h) {
        val = ycc_at(px, r).x;                              // main luma Y(px,r)
    } else {
        float ay = r - h;
        float cw = floor(w * 0.5);
        float ch = floor(h * 0.5);
        if (ay < ch) {                                      // top band: [Cb01 | Cb10]
            float yc = ay;
            if (px < cw) {
                val = ycc_at(2.0 * px + 1.0, 2.0 * yc).y;            // Cb01
            } else {
                float xc = px - cw;
                val = ycc_at(2.0 * xc, 2.0 * yc + 1.0).y;           // Cb10
            }
        } else {                                            // bottom band: [Cb11 | Cr01]
            float yc = ay - ch;
            if (px < cw) {
                val = ycc_at(2.0 * px + 1.0, 2.0 * yc + 1.0).y;     // Cb11
            } else {
                float xc = px - cw;
                val = ycc_at(2.0 * xc + 1.0, 2.0 * yc).z;           // Cr01
            }
        }
    }
    frag = vec4(val, 0.0, 0.0, 1.0);
}
"#;

/// UV-plane (RG8, `W/2 × H`, each texel = one interleaved U,V pair): rows [0,H/2) main
/// `(U=Cb00, V=Cr00)`; rows [H/2,H) aux `(U=Cr10, V=Cr11)`. Mirrors the chroma writes of
/// `pack_y444_to_stacked_nv12`.
const UV_MAIN: &str = r#"
void main() {
    float cw = floor(w * 0.5);
    float ch = floor(h * 0.5);
    float cx = floor(v_texcoord.x * cw);   // 0..W/2-1  (= xc)
    float cy = floor(v_texcoord.y * h);    // 0..H-1
    float U, V;
    if (cy < ch) {                         // main: U=Cb00, V=Cr00 at (2xc, 2yc)
        vec3 m = ycc_at(2.0 * cx, 2.0 * cy);
        U = m.y;
        V = m.z;
    } else {                               // aux: U=Cr10, V=Cr11
        float yc = cy - ch;
        U = ycc_at(2.0 * cx,       2.0 * yc + 1.0).z;   // Cr10
        V = ycc_at(2.0 * cx + 1.0, 2.0 * yc + 1.0).z;   // Cr11
    }
    frag = vec4(U, V, 0.0, 1.0);
}
"#;

fn cat() -> &'static gst::DebugCategory {
    static CAT: OnceLock<gst::DebugCategory> = OnceLock::new();
    CAT.get_or_init(|| {
        gst::DebugCategory::new(
            "rmngavc444pack",
            gst::DebugColorFlags::empty(),
            Some("RMNG AVC444 zero-copy GL→VA packer"),
        )
    })
}

/// Logical-RGB swizzle for the source fetch, picked from the negotiated input format. `glupload`
/// imports the capture as RGBA on this hardware, but accept BGRA too so we never need a separate
/// `glcolorconvert`.
fn swizzle_for(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::Bgra | VideoFormat::Bgrx => "bgr",
        _ => "rgb", // RGBA / RGBx
    }
}

mod imp {
    use super::*;
    use gstreamer_allocators::DmaBufMemory;

    #[derive(Default)]
    pub struct Avc444Pack {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        /// Negotiated sink (RGBA/BGRA `W×H`) video info, for the GL frame sync map + uniforms.
        in_info: Option<VideoInfo>,
        /// Logical-RGB swizzle for the source fetch (`rgb`/`bgr`), from the input format.
        swizzle: &'static str,
        /// Per-plane pack shaders `(Y, UV)`, compiled lazily once the GL context exists.
        shaders: Option<(GLShader, GLShader)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Avc444Pack {
        const NAME: &'static str = "RmngAvc444Pack";
        type Type = super::Avc444Pack;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for Avc444Pack {}
    impl GstObjectImpl for Avc444Pack {}

    impl ElementImpl for Avc444Pack {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static M: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(M.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "RMNG AVC444 GL→VA packer",
                    "Filter/Converter/Video/GL",
                    "Packs RGBA into the AVC444 stacked W×2H NV12 layout, rendering straight into a \
                     VA-allocated dmabuf surface (zero-copy GL→VA)",
                    "RMNG",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static T: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
            T.get_or_init(|| {
                let range = || gst::IntRange::<i32>::new(1, i32::MAX);
                // Sink: GLMemory RGBA/BGRA (the formats glupload yields from the capture dmabuf).
                let mut sink_caps = gst::Caps::new_empty();
                {
                    let c = sink_caps.get_mut().unwrap();
                    for f in ["RGBA", "BGRA", "RGBx", "BGRx"] {
                        let s = gst::Structure::builder("video/x-raw")
                            .field("format", f)
                            .field("width", range())
                            .field("height", range())
                            .field("texture-target", "2D")
                            .build();
                        c.append_structure_full(s, Some(gst::CapsFeatures::new(["memory:GLMemory"])));
                    }
                }
                // Src: a VA-allocated NV12 W×2H dmabuf with the AMD tiled modifier.
                let src_caps = gst::Caps::builder("video/x-raw")
                    .features(["memory:DMABuf"])
                    .field("format", "DMA_DRM")
                    .field("drm-format", drm_nv12())
                    .field("width", range())
                    .field("height", range())
                    .build();
                vec![
                    gst::PadTemplate::new("sink", gst::PadDirection::Sink, gst::PadPresence::Always, &sink_caps)
                        .unwrap(),
                    gst::PadTemplate::new("src", gst::PadDirection::Src, gst::PadPresence::Always, &src_caps)
                        .unwrap(),
                ]
            })
            .as_slice()
        }
    }

    impl BaseTransformImpl for Avc444Pack {
        // The output frame is twice as tall and a different memory type — never in place.
        const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        // sink (RGBA W×H GLMemory) ↔ src (NV12 W×2H DMA_DRM): swap format/feature, scale height.
        fn transform_caps(
            &self,
            direction: gst::PadDirection,
            caps: &gst::Caps,
            filter: Option<&gst::Caps>,
        ) -> Option<gst::Caps> {
            let to_dmabuf = matches!(direction, gst::PadDirection::Sink);
            let mut out = gst::Caps::new_empty();
            {
                let out = out.get_mut().unwrap();
                for i in 0..caps.size() {
                    let mut s = caps.structure(i).unwrap().to_owned();
                    if to_dmabuf {
                        scale_height(&mut s, 2, 1);
                        s.set("format", "DMA_DRM");
                        s.set("drm-format", drm_nv12());
                        s.remove_field("texture-target"); // GL-only
                        out.append_structure_full(s, Some(gst::CapsFeatures::new(["memory:DMABuf"])));
                    } else {
                        scale_height(&mut s, 1, 2);
                        s.remove_field("format"); // the sink template fills the RGBA/BGRA list
                        s.remove_field("drm-format");
                        s.set("texture-target", "2D");
                        out.append_structure_full(s, Some(gst::CapsFeatures::new(["memory:GLMemory"])));
                    }
                }
            }
            match filter {
                Some(f) => Some(f.intersect_with_mode(&out, gst::CapsIntersectMode::First)),
                None => Some(out),
            }
        }

        fn unit_size(&self, caps: &gst::Caps) -> Option<usize> {
            // GLMemory RGBA caps parse directly; DMA_DRM caps don't, so fall back to the NV12
            // size from width/height (W×2H frame → W·2H·3/2).
            if let Ok(info) = VideoInfo::from_caps(caps) {
                return Some(info.size());
            }
            let s = caps.structure(0)?;
            let w = s.get::<i32>("width").ok()? as usize;
            let h = s.get::<i32>("height").ok()? as usize;
            Some(w * h * 3 / 2)
        }

        fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let in_info =
                VideoInfo::from_caps(incaps).map_err(|_| gst::loggable_error!(cat(), "bad input caps"))?;
            let sw = swizzle_for(in_info.format());
            let mut st = self.state.lock().unwrap();
            if st.swizzle != sw {
                st.shaders = None; // rebuild with the new source swizzle
            }
            st.swizzle = sw;
            st.in_info = Some(in_info);
            Ok(())
        }

        // `outbuf` is a VA-allocated NV12 dmabuf (acquired from the downstream pool by the default
        // prepare_output_buffer). Import its planes into GL and render the pack shaders straight
        // into them, sampling the input RGBA texture — the packed data lands directly in the
        // encoder's surface, no host copy and no intermediate texture.
        fn transform(
            &self,
            inbuf: &gst::Buffer,
            outbuf: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let mut st = self.state.lock().unwrap();
            let in_info = st.in_info.clone().ok_or(gst::FlowError::NotNegotiated)?;
            let (w, h) = (in_info.width() as i32, in_info.height() as i32); // source RGBA W×H
            let h2 = w_h2(h); // packed luma height = 2H
            let swizzle = if st.swizzle.is_empty() { "rgb" } else { st.swizzle };

            // Input: the captured image as a single RGBA GL texture (from glupload).
            let in_mem = inbuf
                .memory(0)
                .and_then(|m| m.downcast_memory::<GLMemory>().ok())
                .ok_or(gst::FlowError::Error)?;
            let ctx = in_mem.context();
            let in_tex = in_mem.texture_id();

            // Build the two pack shaders once, on the GL thread (GLSL attach needs the context).
            if st.shaders.is_none() {
                let built: Mutex<Option<(GLShader, GLShader)>> = Mutex::new(None);
                ctx.thread_add(|c| {
                    let pair = (|| Ok::<_, gst::LoggableError>((
                        build_pack_shader(c, Y_MAIN, swizzle)?,
                        build_pack_shader(c, UV_MAIN, swizzle)?,
                    )))();
                    match pair {
                        Ok(p) => *built.lock().unwrap() = Some(p),
                        Err(e) => eprintln!("[pack] shader build failed: {e}"),
                    }
                });
                st.shaders = built.into_inner().unwrap();
                if st.shaders.is_none() {
                    return Err(gst::FlowError::Error);
                }
            }
            let (y_shader, uv_shader) = st.shaders.as_ref().unwrap();
            let (y_shader, uv_shader) = (y_shader.clone(), uv_shader.clone());
            drop(st);

            // Output: the VA dmabuf. Read each plane's fd/offset/stride (from the VideoMeta if
            // present, else the default NV12 layout) so we import exactly what VA allocated.
            let vmeta = outbuf.meta::<VideoMeta>();
            let n_mem = outbuf.n_memory();
            let plane_fd = |plane: usize| -> Option<i32> {
                let idx = if n_mem >= 2 { plane } else { 0 };
                outbuf.memory(idx)?.downcast_memory_ref::<DmaBufMemory>().map(|m| m.fd())
            };
            let (off, stride) = match &vmeta {
                Some(m) => (
                    [m.offset()[0], m.offset().get(1).copied().unwrap_or((w * h2) as usize)],
                    [m.stride()[0], m.stride().get(1).copied().unwrap_or(w)],
                ),
                None => ([0usize, (w * h2) as usize], [w, w]),
            };
            // With per-plane memories the offsets are within each fd (typically 0).
            let plane_off = |plane: usize| if n_mem >= 2 { 0 } else { off[plane] as i32 };
            let (Some(y_fd), Some(uv_fd)) = (plane_fd(0), plane_fd(1)) else {
                return Err(gst::FlowError::Error);
            };

            let planes = [
                // Y plane: R8, W×2H, packed by the luma shader.
                PlaneRender { shader: y_shader, fd: y_fd, offset: plane_off(0), stride: stride[0],
                              fourcc: DRM_FOURCC_R8, w, h: h2 },
                // UV plane: GR88, W/2×H, packed by the chroma shader.
                PlaneRender { shader: uv_shader, fd: uv_fd, offset: plane_off(1), stride: stride[1],
                              fourcc: DRM_FOURCC_GR88, w: w / 2, h },
            ];
            let modifier = drm_modifier();
            let (src_w, src_h) = (w as f32, h as f32);

            // Sync the input GL texture (READ|GL), then import the VA planes + render on the GL
            // thread. amdgpu's implicit dmabuf fencing orders this write before the VA read.
            let frame =
                GLVideoFrameRef::from_buffer_ref_readable(inbuf, &in_info).map_err(|_| gst::FlowError::Error)?;
            let ok = Mutex::new(false);
            ctx.thread_add(|c| {
                *ok.lock().unwrap() = unsafe { pack_into_va(c, in_tex, src_w, src_h, &planes, modifier) };
            });
            drop(frame);

            if ok.into_inner().unwrap() {
                Ok(gst::FlowSuccess::Ok)
            } else {
                Err(gst::FlowError::Error)
            }
        }
    }

    /// Packed luma height (2·H) — named for the comments that reference `h2`.
    fn w_h2(h: i32) -> i32 {
        h * 2
    }

    fn scale_height(s: &mut gst::Structure, mul: i64, div: i64) {
        let clamp = |v: i64| v.clamp(1, i32::MAX as i64) as i32;
        if let Ok(hv) = s.get::<i32>("height") {
            s.set("height", clamp(hv as i64 * mul / div));
        } else if let Ok(r) = s.get::<gst::IntRange<i32>>("height") {
            s.set(
                "height",
                gst::IntRange::<i32>::new(clamp(r.min() as i64 * mul / div), clamp(r.max() as i64 * mul / div)),
            );
        }
    }
}

glib::wrapper! {
    pub struct Avc444Pack(ObjectSubclass<imp::Avc444Pack>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

// ---- raw EGL/GL: import a VA dmabuf plane as a GL texture and render the pack shader into it ---

const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TRIANGLES: u32 = 0x0004;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_NEAREST: i32 = 0x2600;
const DRM_FOURCC_R8: i32 = 0x20203852; // 'R','8',' ',' '
const DRM_FOURCC_GR88: i32 = 0x38385247; // 'G','R','8','8'

/// The DMA_DRM `drm-format` for the VA-allocated NV12 surface (AMD tiled modifier, as
/// advertised by `vapostproc`/`vah264enc`). Overridable via `RMNG_DRM_MOD` for experiments.
fn drm_nv12() -> &'static str {
    static M: OnceLock<String> = OnceLock::new();
    M.get_or_init(|| std::env::var("RMNG_DRM_MOD").unwrap_or_else(|_| "NV12:0x0200000020801b03".into()))
}

/// The DRM modifier from [`drm_nv12`] (hex after the `:`), as a u64.
fn drm_modifier() -> u64 {
    drm_nv12()
        .split_once(':')
        .and_then(|(_, m)| u64::from_str_radix(m.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0x0200000020801b03)
}

type EglGetCurrentDisplay = unsafe extern "C" fn() -> *mut std::ffi::c_void;
type EglCreateImage = unsafe extern "C" fn(
    dpy: *mut std::ffi::c_void,
    ctx: *mut std::ffi::c_void,
    target: u32,
    buffer: *mut std::ffi::c_void,
    attrib_list: *const i32,
) -> *mut std::ffi::c_void;
type EglDestroyImage =
    unsafe extern "C" fn(dpy: *mut std::ffi::c_void, image: *mut std::ffi::c_void) -> u32;
type GlGenTextures = unsafe extern "C" fn(n: i32, textures: *mut u32);
type GlBindTexture = unsafe extern "C" fn(target: u32, texture: u32);
type GlDeleteTextures = unsafe extern "C" fn(n: i32, textures: *const u32);
type GlEglImageTarget = unsafe extern "C" fn(target: u32, image: *mut std::ffi::c_void);
type GlGenFramebuffers = unsafe extern "C" fn(n: i32, fbs: *mut u32);
type GlBindFramebuffer = unsafe extern "C" fn(target: u32, fb: u32);
type GlFramebufferTexture2D = unsafe extern "C" fn(target: u32, attachment: u32, textarget: u32, tex: u32, level: i32);
type GlDeleteFramebuffers = unsafe extern "C" fn(n: i32, fbs: *const u32);
type GlViewport = unsafe extern "C" fn(x: i32, y: i32, w: i32, h: i32);
type GlActiveTexture = unsafe extern "C" fn(unit: u32);
type GlTexParameteri = unsafe extern "C" fn(target: u32, pname: u32, param: i32);
type GlDrawArrays = unsafe extern "C" fn(mode: u32, first: i32, count: i32);
type GlFlush = unsafe extern "C" fn();

unsafe fn egl_fn<T>(ctx: &gst_gl::GLContext, name: &str) -> Option<T> {
    let p = ctx.proc_address(name);
    if p == 0 { None } else { Some(unsafe { std::mem::transmute_copy::<usize, T>(&p) }) }
}

/// Build one pack shader: the gl_VertexID fullscreen triangle + a fragment assembled from
/// [`PACK_HEADER`] + `swizzle` + [`PACK_TAIL`] + `main` (the per-plane body). Run on the GL thread.
fn build_pack_shader(ctx: &gst_gl::GLContext, main: &str, swizzle: &str) -> Result<GLShader, gst::LoggableError> {
    let shader = GLShader::new(ctx);
    let v = GLSLStage::with_strings(ctx, GL_VERTEX_SHADER, GLSLVersion::None, GLSLProfile::ES, &[PACK_VERT]);
    shader.compile_attach_stage(&v).map_err(|e| gst::loggable_error!(cat(), "pack vert: {e}"))?;
    let f = GLSLStage::with_strings(
        ctx,
        GL_FRAGMENT_SHADER,
        GLSLVersion::None,
        GLSLProfile::ES,
        &[PACK_HEADER, swizzle, PACK_TAIL, main],
    );
    shader.compile_attach_stage(&f).map_err(|e| gst::loggable_error!(cat(), "pack frag: {e}"))?;
    shader.link().map_err(|e| gst::loggable_error!(cat(), "pack link: {e}"))?;
    Ok(shader)
}

/// One destination plane of the VA dmabuf + the shader that packs into it.
struct PlaneRender {
    shader: GLShader,
    fd: i32,
    offset: i32,
    stride: i32,
    fourcc: i32,
    /// Plane texel dims = render viewport (Y: W×2H; UV: W/2×H).
    w: i32,
    h: i32,
}

/// Import each VA dmabuf plane as a GL texture and FBO-render its pack shader, sampling the single
/// `input_tex` (the captured RGBA image, `src_w×src_h`). **Must run on the GL thread with the
/// context current** (call inside `thread_add`).
unsafe fn pack_into_va(
    ctx: &gst_gl::GLContext,
    input_tex: u32,
    src_w: f32,
    src_h: f32,
    planes: &[PlaneRender],
    modifier: u64,
) -> bool {
    unsafe {
        let (Some(get_dpy), Some(create), Some(destroy)) = (
            egl_fn::<EglGetCurrentDisplay>(ctx, "eglGetCurrentDisplay"),
            egl_fn::<EglCreateImage>(ctx, "eglCreateImageKHR"),
            egl_fn::<EglDestroyImage>(ctx, "eglDestroyImageKHR"),
        ) else {
            return false;
        };
        let (Some(gen_tex), Some(bind_tex), Some(del_tex), Some(egl_target)) = (
            egl_fn::<GlGenTextures>(ctx, "glGenTextures"),
            egl_fn::<GlBindTexture>(ctx, "glBindTexture"),
            egl_fn::<GlDeleteTextures>(ctx, "glDeleteTextures"),
            egl_fn::<GlEglImageTarget>(ctx, "glEGLImageTargetTexture2DOES"),
        ) else {
            return false;
        };
        let (Some(gen_fb), Some(bind_fb), Some(fb_tex), Some(del_fb), Some(viewport), Some(active_tex), Some(draw), Some(flush), Some(tex_param)) = (
            egl_fn::<GlGenFramebuffers>(ctx, "glGenFramebuffers"),
            egl_fn::<GlBindFramebuffer>(ctx, "glBindFramebuffer"),
            egl_fn::<GlFramebufferTexture2D>(ctx, "glFramebufferTexture2D"),
            egl_fn::<GlDeleteFramebuffers>(ctx, "glDeleteFramebuffers"),
            egl_fn::<GlViewport>(ctx, "glViewport"),
            egl_fn::<GlActiveTexture>(ctx, "glActiveTexture"),
            egl_fn::<GlDrawArrays>(ctx, "glDrawArrays"),
            egl_fn::<GlFlush>(ctx, "glFlush"),
            egl_fn::<GlTexParameteri>(ctx, "glTexParameteri"),
        ) else {
            return false;
        };

        let dpy = get_dpy();
        let (mod_lo, mod_hi) = ((modifier & 0xffff_ffff) as i32, (modifier >> 32) as i32);
        let mut fb = 0u32;
        gen_fb(1, &mut fb);
        let mut ok = true;
        for p in planes {
            let attribs = [
                EGL_WIDTH, p.w,
                EGL_HEIGHT, p.h,
                EGL_LINUX_DRM_FOURCC_EXT, p.fourcc,
                EGL_DMA_BUF_PLANE0_FD_EXT, p.fd,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT, p.offset,
                EGL_DMA_BUF_PLANE0_PITCH_EXT, p.stride,
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, mod_lo,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, mod_hi,
                EGL_NONE,
            ];
            let img = create(dpy, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
            if img.is_null() {
                ok = false;
                break;
            }
            // Wrap the imported dmabuf plane as a GL texture (render target).
            let mut dst = 0u32;
            gen_tex(1, &mut dst);
            bind_tex(GL_TEXTURE_2D, dst);
            egl_target(GL_TEXTURE_2D, img);
            bind_tex(GL_TEXTURE_2D, 0);

            // FBO-render the pack shader into it, point-sampling the source image (exact bytes).
            bind_fb(GL_FRAMEBUFFER, fb);
            fb_tex(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, dst, 0);
            viewport(0, 0, p.w, p.h);
            p.shader.use_();
            p.shader.set_uniform_1i("tex", 0);
            p.shader.set_uniform_1f("w", src_w);
            p.shader.set_uniform_1f("h", src_h);
            active_tex(GL_TEXTURE0);
            bind_tex(GL_TEXTURE_2D, input_tex);
            tex_param(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            tex_param(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            draw(GL_TRIANGLES, 0, 3);

            fb_tex(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            bind_tex(GL_TEXTURE_2D, 0);
            del_tex(1, &dst);
            destroy(dpy, img);
        }
        bind_fb(GL_FRAMEBUFFER, 0);
        del_fb(1, &fb);
        // Submit the GL work (installs the write fence on the VA dmabuf) but DON'T block the CPU
        // waiting for completion — amdgpu's implicit dmabuf fencing orders the downstream VA
        // encode's read after this write on the GPU. A blocking glFinish here serialized the pack
        // and the encode per frame (~47fps at 2560×2880); glFlush lets them pipeline.
        flush();
        ok
    }
}

/// Register the single `rmngavc444pack` GL→VA packer in-process (idempotent). Call once before
/// building a pipeline that references it by name.
pub fn register() -> anyhow::Result<()> {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    let mut result = Ok(());
    REGISTER.call_once(|| {
        result = gst::Element::register(None, "rmngavc444pack", gst::Rank::NONE, Avc444Pack::static_type())
            .map_err(|e| anyhow::anyhow!("register rmngavc444pack: {e}"));
    });
    result
}
