//! Loads virglrenderer and gives it an OpenGL context.
//!
//! virglrenderer turns the guest's graphics commands into host OpenGL. It
//! normally opens its own Linux display, but it can also ask the caller for
//! contexts, which is how it runs on Windows. RAX creates them through WGL.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;

use serde::Serialize;

use crate::dylib::Library;
use crate::{Error, Result};

type Hwnd = *mut c_void;
type Hdc = *mut c_void;
type Hglrc = *mut c_void;

const CALLBACKS_VERSION: c_int = 4;
const CAPSET_VIRGL: u32 = 1;
const CAPSET_VIRGL2: u32 = 2;

const PFD_SUPPORT_OPENGL: u32 = 0x20;
const PFD_DOUBLEBUFFER: u32 = 0x01;
const PFD_DRAW_TO_WINDOW: u32 = 0x04;
const GL_VENDOR: u32 = 0x1f00;
const GL_RENDERER: u32 = 0x1f01;
const GL_VERSION: u32 = 0x1f02;
const WGL_CONTEXT_MAJOR_VERSION: c_int = 0x2091;
const WGL_CONTEXT_MINOR_VERSION: c_int = 0x2092;
const WGL_CONTEXT_PROFILE_MASK: c_int = 0x9126;
const WGL_CONTEXT_CORE_PROFILE_BIT: c_int = 1;

#[repr(C)]
struct PixelFormatDescriptor {
    size: u16,
    version: u16,
    flags: u32,
    pixel_type: u8,
    color_bits: u8,
    reserved: [u8; 16],
    depth_bits: u8,
    stencil_bits: u8,
    tail: [u8; 12],
}

const _: () = assert!(size_of::<PixelFormatDescriptor>() == 40);

#[repr(C)]
struct GlContextParam {
    version: c_int,
    shared: bool,
    major_version: c_int,
    minor_version: c_int,
    compatibility: c_int,
}

/// `struct virgl_renderer_callbacks`, up to version 4.
#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: extern "C" fn(*mut c_void, u32),
    create_gl_context: extern "C" fn(*mut c_void, c_int, *mut GlContextParam) -> *mut c_void,
    destroy_gl_context: extern "C" fn(*mut c_void, *mut c_void),
    make_current: extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int,
    get_drm_fd: *const c_void,
    write_context_fence: *const c_void,
    get_server_fd: *const c_void,
    get_egl_display: *const c_void,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> Hwnd;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn GetDC(hwnd: Hwnd) -> Hdc;
    fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn ChoosePixelFormat(hdc: Hdc, descriptor: *const PixelFormatDescriptor) -> i32;
    fn SetPixelFormat(hdc: Hdc, format: i32, descriptor: *const PixelFormatDescriptor) -> i32;
}

#[link(name = "opengl32")]
unsafe extern "system" {
    fn wglCreateContext(hdc: Hdc) -> Hglrc;
    fn wglDeleteContext(context: Hglrc) -> i32;
    fn wglMakeCurrent(hdc: Hdc, context: Hglrc) -> i32;
    fn wglGetProcAddress(name: *const c_char) -> *const c_void;
    fn glGetString(name: u32) -> *const c_char;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
}

type CreateContextAttribs = unsafe extern "system" fn(Hdc, Hglrc, *const c_int) -> Hglrc;

thread_local! {
    /// The device context virglrenderer's callbacks draw through.
    static DEVICE: Cell<Hdc> = const { Cell::new(std::ptr::null_mut()) };
    /// The context new contexts share objects with.
    static SHARED: Cell<Hglrc> = const { Cell::new(std::ptr::null_mut()) };
    /// Resolved `wglCreateContextAttribsARB`.
    static CREATE: Cell<Option<CreateContextAttribs>> = const { Cell::new(None) };
}

extern "C" fn write_fence(_cookie: *mut c_void, _fence: u32) {}

extern "C" fn create_gl_context(
    _cookie: *mut c_void,
    _scanout: c_int,
    param: *mut GlContextParam,
) -> *mut c_void {
    // SAFETY: virglrenderer passes a valid parameter block for this call.
    let param = unsafe { &*param };
    let share = if param.shared {
        SHARED.get()
    } else {
        std::ptr::null_mut()
    };
    let attributes = [
        WGL_CONTEXT_MAJOR_VERSION,
        param.major_version,
        WGL_CONTEXT_MINOR_VERSION,
        param.minor_version,
        WGL_CONTEXT_PROFILE_MASK,
        WGL_CONTEXT_CORE_PROFILE_BIT,
        0,
    ];
    match CREATE.get() {
        // SAFETY: the function was resolved for this thread's context, the
        // device context is live, and the attribute list ends with zero.
        Some(create) => unsafe { create(DEVICE.get(), share, attributes.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

extern "C" fn destroy_gl_context(_cookie: *mut c_void, context: *mut c_void) {
    // SAFETY: `context` came from `create_gl_context`.
    unsafe {
        wglDeleteContext(context);
    }
}

extern "C" fn make_current(_cookie: *mut c_void, _scanout: c_int, context: *mut c_void) -> c_int {
    // SAFETY: the device context is live, and a null context is allowed.
    if unsafe { wglMakeCurrent(DEVICE.get(), context) } != 0 {
        0
    } else {
        -1
    }
}

fn gl_string(name: u32) -> String {
    // SAFETY: a context is current, and the result is null or a C string.
    let text = unsafe { glGetString(name) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: checked non-null, and OpenGL owns a NUL-terminated string.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// What the probe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Probe {
    /// `GL_VENDOR`.
    pub vendor: String,
    /// `GL_RENDERER`, which names the GPU in use.
    pub renderer: String,
    /// `GL_VERSION`.
    pub version: String,
    /// Result of `virgl_renderer_init`.
    pub init_result: i32,
    /// Highest version of the older capability set.
    pub virgl_max_version: u32,
    /// Size in bytes of the older capability set.
    pub virgl_max_size: u32,
    /// Highest version of the newer capability set.
    pub virgl2_max_version: u32,
    /// Size in bytes of the newer capability set.
    pub virgl2_max_size: u32,
}

/// A hidden window whose device context can hold a current OpenGL context.
struct GlWindow {
    hwnd: Hwnd,
    hdc: Hdc,
    context: Hglrc,
}

impl GlWindow {
    fn new() -> Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: the class name is a NUL-terminated system class, and null
        // handles are valid for a parentless window with no menu.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                16,
                16,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(Error::Win32 {
                api: "CreateWindowExW",
                code,
            });
        }
        // SAFETY: `hwnd` is the live window just created.
        let hdc = unsafe { GetDC(hwnd) };
        let descriptor = PixelFormatDescriptor {
            size: 40,
            version: 1,
            flags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
            pixel_type: 0,
            color_bits: 32,
            reserved: [0; 16],
            depth_bits: 24,
            stencil_bits: 8,
            tail: [0; 12],
        };
        // SAFETY: `hdc` is live and `descriptor` is fully initialised.
        let format = unsafe { ChoosePixelFormat(hdc, &raw const descriptor) };
        // SAFETY: as above, and `format` was just chosen for this device.
        unsafe { SetPixelFormat(hdc, format, &raw const descriptor) };
        // SAFETY: `hdc` has a pixel format now.
        let context = unsafe { wglCreateContext(hdc) };
        let window = Self { hwnd, hdc, context };
        // SAFETY: both handles are live.
        if context.is_null() || unsafe { wglMakeCurrent(hdc, context) } == 0 {
            // SAFETY: GetLastError has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(Error::Win32 {
                api: "wglMakeCurrent",
                code,
            });
        }
        Ok(window)
    }
}

impl Drop for GlWindow {
    fn drop(&mut self) {
        // SAFETY: the handles are live and are released only here, and
        // clearing the current context first is always allowed.
        unsafe {
            wglMakeCurrent(std::ptr::null_mut(), std::ptr::null_mut());
            wglDeleteContext(self.context);
            ReleaseDC(self.hwnd, self.hdc);
            DestroyWindow(self.hwnd);
        }
    }
}

/// Asks hybrid laptops for the discrete GPU. NVIDIA and AMD drivers look for
/// these names exported from the executable when OpenGL starts, which keeps
/// the choice out of the registry and inside the program.
#[unsafe(no_mangle)]
#[used]
pub static NvOptimusEnablement: u32 = 1;
/// The AMD equivalent of [`NvOptimusEnablement`].
#[unsafe(no_mangle)]
#[used]
pub static AmdPowerXpressRequestHighPerformance: u32 = 1;

/// Linker directives that export the two values above. An executable exports
/// nothing by default, and this section works with both the GNU and MSVC
/// linkers while exporting only these names.
#[unsafe(link_section = ".drectve")]
#[used]
static EXPORT_GPU_PREFERENCE: [u8; 84] =
    *b" -export:NvOptimusEnablement,data -export:AmdPowerXpressRequestHighPerformance,data ";

type RendererInit = unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int;
type GetCapSet = unsafe extern "C" fn(u32, *mut u32, *mut u32);
type Cleanup = unsafe extern "C" fn(*mut c_void);

/// Starts virglrenderer from `folder` and reports what it can do.
pub fn probe(folder: &Path) -> Result<Probe> {
    let window = GlWindow::new()?;
    DEVICE.set(window.hdc);
    SHARED.set(window.context);

    // SAFETY: a context is current, and the name is NUL-terminated.
    let address = unsafe { wglGetProcAddress(c"wglCreateContextAttribsARB".as_ptr()) };
    if address.is_null() {
        return Err(Error::SymbolMissing {
            library: "opengl32.dll".to_owned(),
            symbol: "wglCreateContextAttribsARB".to_owned(),
        });
    }
    // SAFETY: the driver returned this address for that name, whose
    // signature matches the alias.
    let create = unsafe { std::mem::transmute::<*const c_void, CreateContextAttribs>(address) };
    CREATE.set(Some(create));

    let library = Library::load_from(&folder.join("libvirglrenderer-1.dll"))?;
    // SAFETY: each alias matches the signature in virglrenderer.h, and
    // `library` outlives the calls below.
    let (init, get_cap_set, cleanup) = unsafe {
        (
            library.symbol::<RendererInit>(c"virgl_renderer_init")?,
            library.symbol::<GetCapSet>(c"virgl_renderer_get_cap_set")?,
            library.symbol::<Cleanup>(c"virgl_renderer_cleanup")?,
        )
    };

    let mut callbacks = Callbacks {
        version: CALLBACKS_VERSION,
        write_fence,
        create_gl_context,
        destroy_gl_context,
        make_current,
        get_drm_fd: std::ptr::null(),
        write_context_fence: std::ptr::null(),
        get_server_fd: std::ptr::null(),
        get_egl_display: std::ptr::null(),
    };
    // The renderer refuses a null cookie, although it never reads this value.
    let mut cookie = 0_u8;
    let cookie: *mut c_void = (&raw mut cookie).cast();
    // SAFETY: `callbacks` and `cookie` outlive the renderer, which is cleaned
    // up below. Flags of zero select the caller-provided contexts above.
    let init_result = unsafe { init(cookie, 0, &raw mut callbacks) };

    let mut found = Probe {
        vendor: String::new(),
        renderer: String::new(),
        version: String::new(),
        init_result,
        virgl_max_version: 0,
        virgl_max_size: 0,
        virgl2_max_version: 0,
        virgl2_max_size: 0,
    };
    if init_result == 0 {
        // SAFETY: the renderer started, and the out pointers are valid.
        unsafe {
            get_cap_set(
                CAPSET_VIRGL,
                &raw mut found.virgl_max_version,
                &raw mut found.virgl_max_size,
            );
            get_cap_set(
                CAPSET_VIRGL2,
                &raw mut found.virgl2_max_version,
                &raw mut found.virgl2_max_size,
            );
        }
    }
    found.vendor = gl_string(GL_VENDOR);
    found.renderer = gl_string(GL_RENDERER);
    found.version = gl_string(GL_VERSION);
    if init_result == 0 {
        // SAFETY: the renderer started, with this same cookie.
        unsafe { cleanup(cookie) };
    }
    Ok(found)
}
