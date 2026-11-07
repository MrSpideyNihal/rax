//! Loads virglrenderer and gives it an OpenGL context.
//!
//! virglrenderer turns the guest's graphics commands into host OpenGL. It
//! normally opens its own Linux display, but it can also ask the caller for
//! contexts, which is how it runs on Windows. RAX creates them through WGL.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::path::Path;

use serde::Serialize;

use crate::dylib::Library;
use crate::vm::check_range;
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
const WGL_CONTEXT_FLAGS: c_int = 0x2094;
const WGL_CONTEXT_DEBUG_BIT: c_int = 1;
const GL_DEBUG_OUTPUT: u32 = 0x92e0;
const GL_DEBUG_OUTPUT_SYNCHRONOUS: u32 = 0x8242;
const GL_DEBUG_TYPE_ERROR: u32 = 0x824c;

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
    fn glEnable(capability: u32);
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
}

type CreateContextAttribs = unsafe extern "system" fn(Hdc, Hglrc, *const c_int) -> Hglrc;
type DebugMessage = extern "system" fn(u32, u32, u32, u32, c_int, *const c_char, *const c_void);
type DebugCallback = unsafe extern "system" fn(DebugMessage, *const c_void);

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
    // The renderer asks for its first context unshared and expects later ones
    // to share with it. Sharing them all with RAX's own context gives the
    // same result: resources made in one are visible in the others.
    let share = SHARED.get();
    let attributes = [
        WGL_CONTEXT_MAJOR_VERSION,
        param.major_version,
        WGL_CONTEXT_MINOR_VERSION,
        param.minor_version,
        WGL_CONTEXT_PROFILE_MASK,
        WGL_CONTEXT_CORE_PROFILE_BIT,
        WGL_CONTEXT_FLAGS,
        i32::from(gl_debug_wanted()) * WGL_CONTEXT_DEBUG_BIT,
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
    if unsafe { wglMakeCurrent(DEVICE.get(), context) } == 0 {
        return -1;
    }
    if !context.is_null() && gl_debug_wanted() {
        listen_for_gl_errors();
    }
    0
}

fn gl_debug_wanted() -> bool {
    std::env::var_os("RAX_GL_DEBUG").is_some()
}

extern "system" fn gl_message(
    _source: u32,
    kind: u32,
    _id: u32,
    _severity: u32,
    _length: c_int,
    message: *const c_char,
    _user: *const c_void,
) {
    if kind == GL_DEBUG_TYPE_ERROR {
        // SAFETY: the driver passes a NUL-terminated message.
        let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        eprintln!("gl: {text}");
    }
}

/// Has the driver describe its errors in the current context.
fn listen_for_gl_errors() {
    // SAFETY: a context is current, and the name is NUL-terminated.
    let address = unsafe { wglGetProcAddress(c"glDebugMessageCallback".as_ptr()) };
    if address.is_null() {
        return;
    }
    // SAFETY: the driver returned this address for that name, whose
    // signature matches the alias.
    let set = unsafe { std::mem::transmute::<*const c_void, DebugCallback>(address) };
    // SAFETY: a context is current, and `gl_message` lives forever.
    unsafe {
        glEnable(GL_DEBUG_OUTPUT);
        glEnable(GL_DEBUG_OUTPUT_SYNCHRONOUS);
        set(gl_message, std::ptr::null());
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

fn callbacks() -> Callbacks {
    Callbacks {
        version: CALLBACKS_VERSION,
        write_fence,
        create_gl_context,
        destroy_gl_context,
        make_current,
        get_drm_fd: std::ptr::null(),
        write_context_fence: std::ptr::null(),
        get_server_fd: std::ptr::null(),
        get_egl_display: std::ptr::null(),
    }
}

/// Gives the context callbacks what they need on this thread.
fn hook_up_contexts(window: &GlWindow) -> Result<()> {
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
    Ok(())
}

/// Starts virglrenderer from `folder` and reports what it can do.
pub fn probe(folder: &Path) -> Result<Probe> {
    let window = GlWindow::new()?;
    hook_up_contexts(&window)?;

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

    let mut callbacks = callbacks();
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

/// `struct iovec`, which the renderer's Windows build defines this way.
#[repr(C)]
#[derive(Clone, Copy)]
struct IoVec {
    base: *mut c_void,
    length: usize,
}

/// `struct virgl_box`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferBox {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Front edge.
    pub z: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Depth.
    pub depth: u32,
}

/// `struct virgl_renderer_resource_create_args`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceArgs {
    /// Guest-chosen handle.
    pub handle: u32,
    /// Texture target.
    pub target: u32,
    /// Pixel format.
    pub format: u32,
    /// Usage flags.
    pub bind: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Depth.
    pub depth: u32,
    /// Array layers.
    pub array_size: u32,
    /// Highest mip level.
    pub last_level: u32,
    /// Sample count.
    pub samples: u32,
    /// Resource flags.
    pub flags: u32,
}

/// One copy between a resource's guest memory and the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Resource handle.
    pub resource: u32,
    /// Rendering context, or zero.
    pub context: u32,
    /// Mip level.
    pub level: u32,
    /// Bytes per row, or zero for the default.
    pub stride: u32,
    /// Bytes per layer, or zero for the default.
    pub layer_stride: u32,
    /// Region.
    pub region: TransferBox,
    /// Offset into the guest's memory.
    pub offset: u64,
}

type ResourceCreate = unsafe extern "C" fn(*mut ResourceArgs, *mut IoVec, u32) -> c_int;
type ResourceUnref = unsafe extern "C" fn(u32);
type AttachIov = unsafe extern "C" fn(c_int, *mut IoVec, c_int) -> c_int;
type DetachIov = unsafe extern "C" fn(c_int, *mut *mut IoVec, *mut c_int);
type ContextCreate = unsafe extern "C" fn(u32, u32, *const c_char) -> c_int;
type ContextDestroy = unsafe extern "C" fn(u32);
type ContextResource = unsafe extern "C" fn(c_int, c_int);
type Submit = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type TransferRead = unsafe extern "C" fn(
    u32,
    u32,
    u32,
    u32,
    u32,
    *mut TransferBox,
    u64,
    *mut IoVec,
    c_int,
) -> c_int;
type TransferWrite = unsafe extern "C" fn(
    u32,
    u32,
    c_int,
    u32,
    u32,
    *mut TransferBox,
    u64,
    *mut IoVec,
    u32,
) -> c_int;
type NoArguments = unsafe extern "C" fn();
type CreateFence = unsafe extern "C" fn(c_int, u32) -> c_int;
type FillCaps = unsafe extern "C" fn(u32, u32, *mut c_void);

/// The renderer's entry points.
struct Functions {
    resource_create: ResourceCreate,
    resource_unref: ResourceUnref,
    attach_iov: AttachIov,
    detach_iov: DetachIov,
    context_create: ContextCreate,
    context_destroy: ContextDestroy,
    context_attach: ContextResource,
    context_detach: ContextResource,
    submit: Submit,
    transfer_read: TransferRead,
    transfer_write: TransferWrite,
    poll: NoArguments,
    force_context_zero: NoArguments,
    create_fence: CreateFence,
    get_cap_set: GetCapSet,
    fill_caps: FillCaps,
    cleanup: Cleanup,
}

/// Joins neighbouring guest ranges, which spares the renderer many small
/// copies, and turns them into host pointers. `None` if any range leaves RAM.
fn host_ranges(ram: *mut u8, ram_bytes: usize, entries: &[(u64, u32)]) -> Option<Vec<IoVec>> {
    let mut ranges: Vec<(u64, usize)> = Vec::with_capacity(entries.len());
    for &(gpa, length) in entries {
        let length = length as usize;
        check_range(gpa, length, ram_bytes).ok()?;
        match ranges.last_mut() {
            Some((start, held)) if *start + *held as u64 == gpa => *held += length,
            _ => ranges.push((gpa, length)),
        }
    }
    let to_host = |(gpa, length): (u64, usize)| IoVec {
        // The offset was proved to lie inside RAM, and wrapping addition
        // never dereferences anything.
        base: ram.wrapping_add(gpa as usize).cast(),
        length,
    };
    Some(ranges.into_iter().map(to_host).collect())
}

/// A running virglrenderer.
///
/// The guest's commands are untrusted, and virglrenderer is what checks them.
/// This type keeps RAX's side of the contract: pointers stay inside guest
/// RAM, and the renderer stays on the thread that owns its OpenGL context,
/// which the raw pointers below enforce by making this type `!Send`.
pub struct Renderer<'vm> {
    functions: Functions,
    ram: *mut u8,
    ram_bytes: usize,
    /// Lists the renderer holds pointers into until they are detached.
    backing: HashMap<u32, Vec<IoVec>>,
    next_fence: u32,
    cookie: Box<u8>,
    _callbacks: Box<Callbacks>,
    _library: Library,
    _window: GlWindow,
    /// Guest RAM must outlive the renderer.
    _vm: PhantomData<&'vm ()>,
}

impl Renderer<'_> {
    /// Starts the renderer from `folder`. `ram` must be valid for
    /// `ram_bytes` for as long as the returned value exists.
    pub(crate) fn start(folder: &Path, ram: *mut u8, ram_bytes: usize) -> Result<Self> {
        let window = GlWindow::new()?;
        hook_up_contexts(&window)?;
        // Windows only looks beside a DLL for its dependencies given a full
        // path. A folder that cannot be resolved then fails to load below.
        let folder = std::path::absolute(folder).unwrap_or_else(|_| folder.to_owned());
        let library = Library::load_from(&folder.join("libvirglrenderer-1.dll"))?;
        // SAFETY: each alias matches the signature in virglrenderer.h, and
        // `library` is stored beside the pointers.
        let (functions, init) = unsafe {
            (
                Functions {
                    resource_create: library.symbol(c"virgl_renderer_resource_create")?,
                    resource_unref: library.symbol(c"virgl_renderer_resource_unref")?,
                    attach_iov: library.symbol(c"virgl_renderer_resource_attach_iov")?,
                    detach_iov: library.symbol(c"virgl_renderer_resource_detach_iov")?,
                    context_create: library.symbol(c"virgl_renderer_context_create")?,
                    context_destroy: library.symbol(c"virgl_renderer_context_destroy")?,
                    context_attach: library.symbol(c"virgl_renderer_ctx_attach_resource")?,
                    context_detach: library.symbol(c"virgl_renderer_ctx_detach_resource")?,
                    submit: library.symbol(c"virgl_renderer_submit_cmd")?,
                    transfer_read: library.symbol(c"virgl_renderer_transfer_read_iov")?,
                    transfer_write: library.symbol(c"virgl_renderer_transfer_write_iov")?,
                    poll: library.symbol(c"virgl_renderer_poll")?,
                    force_context_zero: library.symbol(c"virgl_renderer_force_ctx_0")?,
                    create_fence: library.symbol(c"virgl_renderer_create_fence")?,
                    get_cap_set: library.symbol(c"virgl_renderer_get_cap_set")?,
                    fill_caps: library.symbol(c"virgl_renderer_fill_caps")?,
                    cleanup: library.symbol(c"virgl_renderer_cleanup")?,
                },
                library.symbol::<RendererInit>(c"virgl_renderer_init")?,
            )
        };
        let mut callbacks = Box::new(callbacks());
        // The renderer refuses a null cookie, although it never reads it.
        let mut cookie = Box::new(0_u8);
        // SAFETY: both boxes are stored in the result, and Drop cleans up
        // before they are released. Flags of zero select caller contexts.
        let code = unsafe { init((&raw mut *cookie).cast(), 0, &raw mut *callbacks) };
        if code != 0 {
            return Err(Error::Hresult {
                api: "virgl_renderer_init",
                code,
            });
        }
        Ok(Self {
            functions,
            ram,
            ram_bytes,
            backing: HashMap::new(),
            next_fence: 0,
            cookie,
            _callbacks: callbacks,
            _library: library,
            _window: window,
            _vm: PhantomData,
        })
    }

    /// The highest version and size of a capability set.
    pub fn capset_info(&mut self, id: u32) -> (u32, u32) {
        let (mut version, mut size) = (0, 0);
        // SAFETY: the renderer is running, and the out pointers are valid.
        unsafe { (self.functions.get_cap_set)(id, &raw mut version, &raw mut size) };
        (version, size)
    }

    /// The contents of a capability set.
    pub fn capset(&mut self, id: u32, version: u32) -> Vec<u8> {
        let (_, size) = self.capset_info(id);
        let mut bytes = vec![0_u8; size as usize];
        // SAFETY: the buffer is the size the renderer gave for this set.
        unsafe { (self.functions.fill_caps)(id, version, bytes.as_mut_ptr().cast()) };
        bytes
    }

    /// Makes the renderer's own context current. Call before each command.
    pub fn prepare(&mut self) {
        // SAFETY: the renderer is running on its own thread.
        unsafe { (self.functions.force_context_zero)() }
    }

    /// Sends everything queued to the GPU, by creating a fence. Without
    /// this the work can sit in the driver, and a guest waiting on a query
    /// result asks again and again for an answer that isn't coming.
    pub fn flush(&mut self) {
        self.next_fence = self.next_fence.wrapping_add(1);
        // SAFETY: the renderer is running on its own thread, and context zero
        // is the one it always has.
        unsafe { (self.functions.create_fence)(self.next_fence as c_int, 0) };
    }

    /// Lets the renderer finish queued work, such as query results.
    pub fn poll(&mut self) {
        // SAFETY: the renderer is running on its own thread.
        unsafe { (self.functions.poll)() }
    }

    /// Creates a resource. Returns the renderer's status.
    pub fn create_resource(&mut self, mut args: ResourceArgs) -> i32 {
        // SAFETY: `args` is valid for the call, and no pointer list is given.
        unsafe { (self.functions.resource_create)(&raw mut args, std::ptr::null_mut(), 0) }
    }

    /// Releases a resource and any memory attached to it.
    pub fn destroy_resource(&mut self, handle: u32) {
        self.detach_backing(handle);
        // SAFETY: unknown handles are ignored by the renderer.
        unsafe { (self.functions.resource_unref)(handle) };
    }

    /// Gives a resource its guest memory. `entries` are `(address, length)`,
    /// and any range outside RAM fails the whole request.
    pub fn attach_backing(&mut self, handle: u32, entries: &[(u64, u32)]) -> i32 {
        let Some(mut list) = host_ranges(self.ram, self.ram_bytes, entries) else {
            return -1;
        };
        // SAFETY: every range lies inside guest RAM, which outlives the
        // renderer, and `list` is kept until it is detached. A Vec's heap
        // block does not move when the Vec itself moves into the map.
        let code = unsafe {
            (self.functions.attach_iov)(handle as c_int, list.as_mut_ptr(), list.len() as c_int)
        };
        if code == 0 {
            self.backing.insert(handle, list);
        }
        code
    }

    /// Takes a resource's guest memory away.
    pub fn detach_backing(&mut self, handle: u32) {
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        // SAFETY: the out pointers are valid for the call.
        unsafe { (self.functions.detach_iov)(handle as c_int, &raw mut list, &raw mut count) };
        self.backing.remove(&handle);
    }

    /// Creates a rendering context.
    pub fn create_context(&mut self, id: u32, name: &[u8]) -> i32 {
        // SAFETY: the renderer reads exactly `name.len()` bytes.
        unsafe { (self.functions.context_create)(id, name.len() as u32, name.as_ptr().cast()) }
    }

    /// Destroys a rendering context.
    pub fn destroy_context(&mut self, id: u32) {
        // SAFETY: unknown contexts are ignored by the renderer.
        unsafe { (self.functions.context_destroy)(id) };
    }

    /// Lets a context use a resource.
    pub fn attach_to_context(&mut self, context: u32, resource: u32) {
        // SAFETY: unknown ids are ignored by the renderer.
        unsafe { (self.functions.context_attach)(context as c_int, resource as c_int) };
    }

    /// Stops a context using a resource.
    pub fn detach_from_context(&mut self, context: u32, resource: u32) {
        // SAFETY: unknown ids are ignored by the renderer.
        unsafe { (self.functions.context_detach)(context as c_int, resource as c_int) };
    }

    /// Runs a guest command buffer.
    pub fn submit(&mut self, context: u32, commands: &mut [u32]) -> i32 {
        // SAFETY: the renderer reads exactly `commands.len()` words.
        unsafe {
            (self.functions.submit)(
                commands.as_mut_ptr().cast(),
                context as c_int,
                commands.len() as c_int,
            )
        }
    }

    /// Copies from a resource's guest memory to the host.
    pub fn transfer_to_host(&mut self, mut transfer: Transfer) -> i32 {
        // SAFETY: a null list selects the backing attached earlier, whose
        // bounds the renderer checks against the box.
        unsafe {
            (self.functions.transfer_write)(
                transfer.resource,
                transfer.context,
                transfer.level as c_int,
                transfer.stride,
                transfer.layer_stride,
                &raw mut transfer.region,
                transfer.offset,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    /// Copies from the host to a resource's guest memory.
    pub fn transfer_from_host(&mut self, mut transfer: Transfer) -> i32 {
        // SAFETY: as for `transfer_to_host`.
        unsafe {
            (self.functions.transfer_read)(
                transfer.resource,
                transfer.context,
                transfer.level,
                transfer.stride,
                transfer.layer_stride,
                &raw mut transfer.region,
                transfer.offset,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    /// Reads a resource's top level into `pixels`, four bytes each.
    pub fn read_pixels(
        &mut self,
        resource: u32,
        width: u32,
        height: u32,
        pixels: &mut [u8],
    ) -> i32 {
        let stride = width.saturating_mul(4);
        let needed = (stride as usize).saturating_mul(height as usize);
        if needed > pixels.len() {
            return -1;
        }
        let mut list = IoVec {
            base: pixels.as_mut_ptr().cast(),
            length: needed,
        };
        let mut region = TransferBox {
            width,
            height,
            depth: 1,
            ..TransferBox::default()
        };
        // SAFETY: `list` covers `needed` writable bytes for the whole call,
        // and the renderer holds no pointer to it afterwards.
        unsafe {
            (self.functions.transfer_read)(
                resource,
                0,
                0,
                stride,
                0,
                &raw mut region,
                0,
                &raw mut list,
                1,
            )
        }
    }
}

impl Drop for Renderer<'_> {
    fn drop(&mut self) {
        // SAFETY: the renderer started with this cookie, and everything it
        // holds pointers to is released only after this call.
        unsafe { (self.functions.cleanup)((&raw mut *self.cookie).cast()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: usize = 0x1_0000;

    fn ranges(entries: &[(u64, u32)]) -> Option<Vec<(usize, usize)>> {
        let list = host_ranges(std::ptr::null_mut(), RAM, entries)?;
        Some(list.iter().map(|i| (i.base as usize, i.length)).collect())
    }

    #[test]
    fn neighbouring_ranges_are_joined() {
        assert_eq!(
            ranges(&[(0x1000, 0x1000), (0x2000, 0x1000), (0x8000, 0x10)]),
            Some(vec![(0x1000, 0x2000), (0x8000, 0x10)])
        );
    }

    #[test]
    fn ranges_outside_ram_are_refused() {
        assert_eq!(ranges(&[(RAM as u64 - 1, 2)]), None);
        assert_eq!(ranges(&[(u64::MAX, 2)]), None);
        assert_eq!(ranges(&[(0, 1), (RAM as u64, 1)]), None);
    }

    #[test]
    fn a_range_may_end_exactly_at_the_top_of_ram() {
        assert_eq!(ranges(&[(RAM as u64 - 1, 1)]), Some(vec![(RAM - 1, 1)]));
    }
}
