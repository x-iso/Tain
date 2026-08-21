//! Drawing a decoded Direct3D texture straight from OpenGL.
//!
//! ## What this removes
//!
//! The guest used to move every decoded frame off the GPU and straight back on
//! again. Measured on a release build, and on a real session at 1540x962:
//!
//! | stage | dev machine (AMD) | reported guest |
//! |---|---|---|
//! | send | 0.37 ms | 0.55 ms |
//! | blit (NV12 to RGBA, on the GPU) | 0.38 ms | 0.32 ms |
//! | **read back to a `Vec<u8>`** | **3.07 ms** | **8.83 ms** |
//!
//! On that guest the readback was ninety per cent of the decode, on a *smaller*
//! frame than the dev machine's — some readback paths are simply slow. The UI
//! thread then uploaded the same pixels back to the GPU for egui to draw, and
//! the buffer it read into was reallocated and zero-filled every frame before
//! being completely overwritten.
//!
//! It is not only throughput. The guest acknowledges a frame once it has
//! *decoded* it, and that acknowledgement is what sizes the host's send window —
//! so the readback sat inside the flow-control loop, and shortening it shortens
//! the loop.
//!
//! ## How
//!
//! eframe renders through `glow`, which on Windows is OpenGL over WGL. The only
//! way to hand a Direct3D texture to an OpenGL context is `WGL_NV_DX_interop2`:
//! open the D3D device, register the texture against a GL texture name, and then
//! lock it around each draw. Locked, the GL name refers to the D3D surface's
//! memory; unlocked, D3D owns it again.
//!
//! Despite the name it is not NVIDIA-only, but it is not guaranteed anywhere
//! either — it is not part of any GL version and not implied by being D3D11
//! capable. So everything here is fallible by design and the caller keeps the
//! readback path underneath.
//!
//! ## What is deliberately not done here
//!
//! No egui `TextureId`. `egui_glow::Painter::register_native_texture` needs
//! `&mut Painter`, and a paint callback is handed `&Painter` — so the frame is
//! drawn with a shader of our own over the rectangle egui laid out, rather than
//! going through egui's texture system. Everything drawn *over* the video (the
//! host's pointer, the toolbar) is still ordinary egui on top.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use tracing::*;

use eframe::glow::{self, HasContext};

/// The entry points a zero-copy present needs.
///
/// `wglGetProcAddress` returns null for anything the driver does not implement,
/// so resolving all of these is the whole availability test — there is no
/// version number to compare against.
mod ffi {
    #![allow(non_snake_case)]

    use std::ffi::c_void;

    pub type Handle = *mut c_void;

    /// `WGL_ACCESS_READ_ONLY_NV`. The frame is written by D3D and only read by
    /// GL, which is the access the driver can optimise hardest.
    pub const ACCESS_READ_ONLY: u32 = 0x0000_0000;

    pub type OpenDevice = unsafe extern "system" fn(*mut c_void) -> Handle;
    pub type CloseDevice = unsafe extern "system" fn(Handle) -> i32;
    pub type RegisterObject = unsafe extern "system" fn(
        Handle,
        *mut c_void,
        u32, // GL object name
        u32, // GL target
        u32, // access
    ) -> Handle;
    pub type UnregisterObject = unsafe extern "system" fn(Handle, Handle) -> i32;
    pub type LockObjects = unsafe extern "system" fn(Handle, i32, *mut Handle) -> i32;
    pub type UnlockObjects = unsafe extern "system" fn(Handle, i32, *mut Handle) -> i32;

    /// Every entry point, resolved together or not at all.
    pub struct Entry {
        pub open_device: OpenDevice,
        pub close_device: CloseDevice,
        pub register_object: RegisterObject,
        pub unregister_object: UnregisterObject,
        pub lock_objects: LockObjects,
        pub unlock_objects: UnlockObjects,
    }

    impl Entry {
        /// Resolve against the current GL context, or `None` if any is missing.
        ///
        /// # Safety
        ///
        /// The caller must have a current OpenGL context on this thread.
        /// `wglGetProcAddress` answers for the current context and returns null
        /// when there is not one, which is indistinguishable from "unsupported".
        pub unsafe fn resolve() -> Option<Self> {
            use windows::Win32::Graphics::OpenGL::wglGetProcAddress;

            let get = |name: &str| -> Option<*const c_void> {
                let c_name = std::ffi::CString::new(name).ok()?;
                // SAFETY: a valid null-terminated string that outlives the call.
                let f = unsafe {
                    wglGetProcAddress(windows::core::PCSTR(c_name.as_ptr() as *const u8))
                }?;
                Some(f as *const c_void)
            };

            // SAFETY: each pointer came from `wglGetProcAddress` for the name of
            // the function it is being transmuted to, and the signatures below
            // are the ones the extension defines.
            unsafe {
                Some(Self {
                    open_device: std::mem::transmute::<*const c_void, OpenDevice>(get(
                        "wglDXOpenDeviceNV",
                    )?),
                    close_device: std::mem::transmute::<*const c_void, CloseDevice>(get(
                        "wglDXCloseDeviceNV",
                    )?),
                    register_object: std::mem::transmute::<*const c_void, RegisterObject>(get(
                        "wglDXRegisterObjectNV",
                    )?),
                    unregister_object: std::mem::transmute::<*const c_void, UnregisterObject>(
                        get("wglDXUnregisterObjectNV")?,
                    ),
                    lock_objects: std::mem::transmute::<*const c_void, LockObjects>(get(
                        "wglDXLockObjectsNV",
                    )?),
                    unlock_objects: std::mem::transmute::<*const c_void, UnlockObjects>(get(
                        "wglDXUnlockObjectsNV",
                    )?),
                })
            }
        }
    }
}

/// A D3D11 device opened for sharing with this thread's GL context.
///
/// Lives on the render thread and nowhere else: every call here is against the
/// current GL context, which is that thread's alone.
pub struct InteropDevice {
    entry: ffi::Entry,
    device: ffi::Handle,
    gl: Arc<glow::Context>,
    /// Registered textures, keyed by the D3D pointer they were made from.
    ///
    /// Registration is not free and the decoder cycles through a fixed, small
    /// set of textures, so they are registered once and kept — but only while
    /// `generation` says those textures still exist.
    registered: Vec<Registered>,
    /// The decoder's converter generation these registrations belong to.
    ///
    /// # Why the pointer alone was a use-after-free
    ///
    /// The decoder rebuilds its colour converter on every resolution change,
    /// freeing three textures and allocating three more — and an allocator
    /// reuses addresses. Keyed on the pointer alone, this cache handed back a
    /// registration for a texture that had been freed, and the driver
    /// dereferenced it.
    ///
    /// Not a theory: a real session changed resolution five times in two
    /// minutes and the guest died, on a machine that had reported the extension
    /// as available.
    generation: u64,
    /// Consecutive failures. Past [`FAILURE_BUDGET`] this device gives up.
    failures: u32,
}

/// How many failed locks or registrations before the GPU path is abandoned.
///
/// Small, and permanent for the session. A driver that reports the extension and
/// then cannot use it is not going to start working, and a stream that shows
/// nothing is worse than one that costs a readback.
const FAILURE_BUDGET: u32 = 3;

struct Registered {
    d3d: *mut std::ffi::c_void,
    object: ffi::Handle,
    texture: glow::Texture,
}

/// Whether there is a current OpenGL context on this thread.
///
/// Every GL call here is only valid with one, and teardown can run at moments
/// when there is not — a session closing as the window goes away, or the whole
/// app shutting down after eframe has already destroyed the context. Calling
/// into a dead context is an access violation, and leaking a handful of GL
/// objects on the way out is the better trade.
fn context_is_current() -> bool {
    use windows::Win32::Graphics::OpenGL::wglGetCurrentContext;
    // SAFETY: takes no arguments and returns a handle.
    !unsafe { wglGetCurrentContext() }.is_invalid()
}

// SAFETY: the handles are only ever used on the thread that created them, which
// is the render thread. This bound exists so the device can be stored beside the
// rest of the session's render state.
unsafe impl Send for InteropDevice {}

impl InteropDevice {
    /// Open a D3D11 device for sharing, or `None` where the driver cannot.
    ///
    /// # Safety
    ///
    /// Must be called on the render thread with the GL context current, and
    /// `d3d_device` must be a live `ID3D11Device`.
    pub unsafe fn open(gl: Arc<glow::Context>, d3d_device: *mut std::ffi::c_void) -> Option<Self> {
        // SAFETY: the caller promises a current context.
        let entry = unsafe { ffi::Entry::resolve() }?;
        // SAFETY: the caller promises a live device.
        let device = unsafe { (entry.open_device)(d3d_device) };
        if device.is_null() {
            warn!("wglDXOpenDeviceNV refused this D3D11 device — frames will be read back");
            return None;
        }
        info!("Drawing decoded frames directly from the GPU (WGL_NV_DX_interop2)");
        Some(Self { entry, device, gl, registered: Vec::new(), generation: 0, failures: 0 })
    }

    /// The GL texture standing for a D3D texture, registering it if new.
    ///
    /// # Safety
    ///
    /// `d3d_texture` must be a live `ID3D11Texture2D` on the device this was
    /// opened with, and must outlive every draw that uses the returned name.
    unsafe fn gl_texture_for(
        &mut self,
        d3d_texture: *mut std::ffi::c_void,
        generation: u64,
    ) -> Option<usize> {
        // A new generation means every texture this cache knows about has been
        // freed. Nothing may be reused, and the entries have to go before
        // anything looks one up by address.
        if generation != self.generation {
            debug!(
                "Decoder rebuilt its converter — dropping {} registrations",
                self.registered.len()
            );
            // SAFETY: forwarded from this function's contract.
            unsafe { self.release_all() };
            self.generation = generation;
        }
        if let Some(index) = self.registered.iter().position(|r| r.d3d == d3d_texture) {
            return Some(index);
        }

        // SAFETY: creating a GL texture name on the current context.
        let texture = unsafe { self.gl.create_texture() }.ok()?;
        // SAFETY: the caller promises a live texture on the right device. The
        // name is fresh and unbound, which is what registration requires.
        let object = unsafe {
            (self.entry.register_object)(
                self.device,
                d3d_texture,
                texture.0.get(),
                glow::TEXTURE_2D,
                ffi::ACCESS_READ_ONLY,
            )
        };
        if object.is_null() {
            self.failures += 1;
            warn!(
                "wglDXRegisterObjectNV refused a decoded texture ({} of {})",
                self.failures, FAILURE_BUDGET
            );
            // SAFETY: the name was created above and is not registered.
            unsafe { self.gl.delete_texture(texture) };
            return None;
        }
        self.registered.push(Registered { d3d: d3d_texture, object, texture });
        Some(self.registered.len() - 1)
    }

    /// Run `draw` with the D3D texture's contents available as a GL texture.
    ///
    /// The lock is what transfers ownership of the surface from D3D to GL, and
    /// it is released before returning whatever `draw` produced — including on
    /// the path where `draw` panics is *not* covered, deliberately: a panic here
    /// is a bug in the renderer, and leaving a lock held is a smaller problem
    /// than pretending to have unwound cleanly through a driver.
    ///
    /// # Safety
    ///
    /// As [`Self::gl_texture_for`].
    pub unsafe fn with_locked<T>(
        &mut self,
        d3d_texture: *mut std::ffi::c_void,
        generation: u64,
        draw: impl FnOnce(&glow::Context, glow::Texture) -> T,
    ) -> Option<T> {
        if self.failures >= FAILURE_BUDGET {
            return None;
        }
        // SAFETY: forwarded from this function's own contract.
        let index = unsafe { self.gl_texture_for(d3d_texture, generation) }?;
        let mut object = self.registered[index].object;
        let texture = self.registered[index].texture;

        // SAFETY: one object, registered on this device, not currently locked.
        let locked = unsafe { (self.entry.lock_objects)(self.device, 1, &mut object) } != 0;
        if !locked {
            self.failures += 1;
            warn!("wglDXLockObjectsNV failed ({} of {})", self.failures, FAILURE_BUDGET);
            return None;
        }
        let result = draw(&self.gl, texture);
        // A frame that drew is proof the path works, so a later hiccup should
        // not be charged against a budget the first frames already earned back.
        self.failures = 0;
        // SAFETY: balances the lock above, exactly once.
        unsafe { (self.entry.unlock_objects)(self.device, 1, &mut object) };
        Some(result)
    }
}

impl InteropDevice {
    /// Unregister and delete every texture this device knows about.
    ///
    /// # Safety
    ///
    /// Render thread, GL context current.
    unsafe fn release_all(&mut self) {
        for entry in std::mem::take(&mut self.registered) {
            let mut object = entry.object;
            // SAFETY: each was registered on this device and is released once.
            // Unlocking first is harmless if it was not locked.
            unsafe {
                (self.entry.unlock_objects)(self.device, 1, &mut object);
                (self.entry.unregister_object)(self.device, object);
                self.gl.delete_texture(entry.texture);
            }
        }
    }
}

impl Drop for InteropDevice {
    fn drop(&mut self) {
        // Teardown can happen with no context current — a session closing as its
        // window goes away, or the app shutting down after eframe has destroyed
        // the context. Calling in anyway is an access violation; leaking is not.
        if !context_is_current() {
            warn!("No GL context at teardown — leaving interop objects to the driver");
            return;
        }
        // SAFETY: checked for a current context immediately above.
        unsafe { self.release_all() };
        // SAFETY: opened in `open`, closed once.
        unsafe { (self.entry.close_device)(self.device) };
    }
}

/// Draws one texture over a rectangle. Nothing more.
///
/// Deliberately minimal: no transform, no filtering choice, no colour work. The
/// video processor already produced RGBA in the display's colour space and egui
/// already worked out where the picture goes, so all that is left is to put the
/// one on the other.
pub struct QuadPainter {
    program: glow::Program,
    vao: glow::VertexArray,
}

/// The quad, generated from the vertex index rather than read from a buffer.
///
/// There is no vertex buffer and no attribute, deliberately: four corners is
/// less data than the call that would upload them, and an unbound attribute is
/// undefined input rather than a compile error — the kind of mistake that draws
/// nothing and says nothing about why.
///
/// Indices 0..3 with a triangle strip give (0,0), (1,0), (0,1), (1,1), which is
/// the unit square. `uv` flips vertically because a D3D texture's origin is the
/// top-left corner and OpenGL's is the bottom-left; without it the picture
/// arrives upside down.
const VERTEX_SHADER: &str = r#"#version 330 core
out vec2 uv;
void main() {
    vec2 p = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 330 core
in vec2 uv;
uniform sampler2D frame;
out vec4 colour;
void main() {
    colour = vec4(texture(frame, uv).rgb, 1.0);
}
"#;

impl QuadPainter {
    /// # Safety
    ///
    /// Must be called on the render thread with the GL context current.
    pub unsafe fn new(gl: &glow::Context) -> Option<Self> {
        // SAFETY: ordinary GL object creation on the current context. Every
        // shader is deleted once linked, and the program outlives them.
        unsafe {
            let program = gl.create_program().ok()?;
            let mut shaders = Vec::new();
            for (kind, source) in
                [(glow::VERTEX_SHADER, VERTEX_SHADER), (glow::FRAGMENT_SHADER, FRAGMENT_SHADER)]
            {
                let shader = gl.create_shader(kind).ok()?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    error!("Frame shader would not compile: {}", gl.get_shader_info_log(shader));
                    return None;
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                error!("Frame shader would not link: {}", gl.get_program_info_log(program));
                return None;
            }
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }

            // The quad's corners live in the shader's own coordinate space, so
            // there is no buffer to upload — only somewhere to hang the state.
            let vao = gl.create_vertex_array().ok()?;
            Some(Self { program, vao })
        }
    }

    /// Draw `texture` over the current viewport.
    ///
    /// # Safety
    ///
    /// Render thread, current context, and `texture` locked for GL access.
    pub unsafe fn draw(&self, gl: &glow::Context, texture: glow::Texture) {
        // SAFETY: the program and array were created on this context and the
        // texture is locked by the caller.
        unsafe {
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // The interop surface arrives with no sampler state of its own, and
            // a texture with no mip chain and the default MIPMAP filter samples
            // as black — which looks exactly like the frame never arriving.
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            if let Some(location) = gl.get_uniform_location(self.program, "frame") {
                gl.uniform_1_i32(Some(&location), 0);
            }
            // Blending off: the frame is opaque and egui leaves it on for its
            // own drawing, which would otherwise blend the video against
            // whatever was behind it.
            gl.disable(glow::BLEND);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.enable(glow::BLEND);
        }
    }
}

/// The one OpenGL context this process has, captured at startup.
///
/// A global because there is exactly one, it belongs to the render thread, and
/// the alternative is threading an `Arc<glow::Context>` through every layer
/// between eframe's creation callback and the one place that draws a frame.
static GL: std::sync::OnceLock<Arc<glow::Context>> = std::sync::OnceLock::new();

/// Remember the render thread's GL context. Called once, from eframe's setup.
pub fn remember_context(gl: Arc<glow::Context>) {
    let _ = GL.set(gl);
}

/// Build a renderer for a decoder's D3D11 device, if this machine can.
///
/// # Safety
///
/// Render thread, GL context current, `d3d_device` a live `ID3D11Device`.
pub unsafe fn renderer_for(d3d_device: *mut std::ffi::c_void) -> Option<SharedRenderer> {
    let gl = GL.get()?.clone();
    // SAFETY: forwarded from this function's contract.
    unsafe { Renderer::new(gl, d3d_device) }
}

/// Everything the render thread needs to draw a decoded frame.
///
/// Behind a `Mutex` because an egui paint callback must be `Send + Sync +
/// 'static` and this needs `&mut` to lock a texture. It is never contended: the
/// only thing that touches it is the render thread, one callback at a time.
pub struct Renderer {
    device: InteropDevice,
    quad: QuadPainter,
}

pub type SharedRenderer = Arc<std::sync::Mutex<Renderer>>;

impl Renderer {
    /// Open the decoder's device for sharing and build the shader.
    ///
    /// # Safety
    ///
    /// Render thread, GL context current, and `d3d_device` a live
    /// `ID3D11Device`.
    pub unsafe fn new(
        gl: Arc<glow::Context>,
        d3d_device: *mut std::ffi::c_void,
    ) -> Option<SharedRenderer> {
        // SAFETY: forwarded from this function's contract.
        let quad = unsafe { QuadPainter::new(&gl) }?;
        // SAFETY: as above.
        let device = unsafe { InteropDevice::open(gl, d3d_device) }?;
        Some(Arc::new(std::sync::Mutex::new(Self { device, quad })))
    }

    /// Lock one decoded texture and draw it over the current viewport.
    ///
    /// A method rather than two calls at the call site because the lock borrows
    /// the device mutably while the shader is borrowed immutably, and only
    /// something that owns both can hand them to each other.
    ///
    /// # Safety
    ///
    /// As [`InteropDevice::with_locked`].
    unsafe fn draw(&mut self, texture: *mut std::ffi::c_void, generation: u64) -> Option<()> {
        let quad = &self.quad;
        // SAFETY: forwarded from this function's contract.
        unsafe {
            self.device.with_locked(texture, generation, |gl, tex| {
                // SAFETY: the texture is locked for GL for the length of this
                // call, which is the contract `draw` asks for.
                quad.draw(gl, tex);
            })
        }
    }
}

/// Draw one decoded texture over `rect`.
///
/// `texture` is the `ID3D11Texture2D` pointer the decoder reported, carried as a
/// `usize` because it crossed a thread to get here.
pub fn paint_callback(
    renderer: SharedRenderer,
    texture: usize,
    generation: u64,
    rect: egui::Rect,
) -> egui::PaintCallback {
    egui::PaintCallback {
        rect,
        callback: std::sync::Arc::new(eframe::egui_glow::CallbackFn::new(
            move |info, painter| {
                let Ok(mut renderer) = renderer.lock() else { return };
                let gl = painter.gl();

                // egui has already set a viewport for its own drawing; this
                // callback owns a rectangle of its own, and the shader's unit
                // quad is mapped to whatever the viewport says. Setting it here
                // rather than trusting the caller keeps the two independent.
                let area = info.viewport_in_pixels();
                // SAFETY: plain state setting on the current context.
                unsafe {
                    gl.viewport(
                        area.left_px,
                        area.from_bottom_px,
                        area.width_px,
                        area.height_px,
                    );
                }

                // SAFETY: the pointer came from the decoder's converter, whose
                // textures live as long as the converter and are only replaced
                // on a size change — which produces a new pointer and a fresh
                // registration rather than reusing this one.
                let drawn = unsafe { renderer.draw(texture as *mut _, generation) };
                if drawn.is_none() {
                    // Nothing drawn beats something wrong. The reason has
                    // already been named by whichever call refused.
                    debug!("Frame {texture:#x} was not drawn");
                }
            },
        )),
    }
}

/// What the probe found, for the log line at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interop {
    /// Every entry point resolved. A decoded texture can be drawn directly.
    Available,
    /// At least one is missing, so this machine keeps the readback path.
    Unavailable,
    /// Asked without a current OpenGL context, which proves nothing either way.
    NotAsked,
}

/// Ask the driver whether it can share Direct3D textures with OpenGL.
///
/// Must be called on the thread that owns the GL context, while that context is
/// current. Reported separately from "unsupported" when there is no context,
/// because writing a machine off for being asked on the wrong thread would look
/// exactly like a driver problem.
pub fn probe() -> Interop {
    use windows::Win32::Graphics::OpenGL::wglGetCurrentContext;

    // SAFETY: takes no arguments and returns a handle.
    if unsafe { wglGetCurrentContext() }.is_invalid() {
        debug!("No current OpenGL context — cannot ask about D3D interop here");
        return Interop::NotAsked;
    }
    // SAFETY: there is a current context, checked above.
    match unsafe { ffi::Entry::resolve() } {
        Some(_) => {
            info!(
                "GPU: this driver supports WGL_NV_DX_interop2 — decoded frames are drawn \
                 without a readback"
            );
            Interop::Available
        }
        None => {
            info!(
                "GPU: no WGL_NV_DX_interop2 on this driver — decoded frames are read back \
                 to the CPU"
            );
            Interop::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a GL context the answer is "don't know", not "no".
    #[test]
    fn asking_off_the_render_thread_is_not_an_answer() {
        assert_eq!(probe(), Interop::NotAsked);
    }

    /// Read-only is what the extension calls zero, and the value is the API's.
    ///
    /// Pinned because it is a magic number in a header we do not include: a
    /// wrong access flag does not fail, it produces a lock the driver services
    /// far more expensively than it needs to.
    #[test]
    fn the_access_flag_is_the_one_the_extension_defines() {
        assert_eq!(ffi::ACCESS_READ_ONLY, 0);
    }
}
