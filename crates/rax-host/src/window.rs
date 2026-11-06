//! A small native window, drawn with GDI.
//!
//! No toolkit, no web view: a Win32 window that shows a pixel buffer and
//! reports input. Must be created and used on one thread.

use std::cell::RefCell;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use crate::{Error, Result};

type Hwnd = *mut c_void;
type Hdc = *mut c_void;

const WS_OVERLAPPEDWINDOW: u32 = 0x00cf_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const CW_USEDEFAULT: i32 = i32::MIN;
const CS_HREDRAW_VREDRAW_OWNDC: u32 = 0x0023;
const IDC_ARROW: usize = 32512;
const SW_SHOW: i32 = 5;
const PM_REMOVE: u32 = 1;

const WM_DESTROY: u32 = 0x0002;
const WM_ACTIVATE: u32 = 0x0006;
const WM_PAINT: u32 = 0x000f;
const WM_CLOSE: u32 = 0x0010;
const WM_DROPFILES: u32 = 0x0233;
const WM_INPUT: u32 = 0x00ff;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;

const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const RAW_HEADER_SIZE: u32 = 24;
const RAW_MOUSE_DX: usize = 36;
const RAW_MOUSE_DY: usize = 40;
const USAGE_PAGE_GENERIC: u16 = 1;
const USAGE_MOUSE: u16 = 2;

const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00cc_0020;
/// Keeps hard pixel edges when the image is scaled.
const COLORONCOLOR: i32 = 3;
const BASE_DPI: u32 = 96;
const SCANCODE_SHIFT: u32 = 16;
const EXTENDED_BIT: isize = 1 << 24;
const REPEAT_BIT: isize = 1 << 30;

#[repr(C)]
struct WndClassExW {
    size: u32,
    style: u32,
    wnd_proc: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize,
    class_extra: i32,
    window_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
    small_icon: *mut c_void,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PointStruct {
    x: i32,
    y: i32,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    point: PointStruct,
    private: u32,
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels: i32,
    y_pels: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct RawInputDevice {
    usage_page: u16,
    usage: u16,
    flags: u32,
    target: Hwnd,
}

const _: () = assert!(size_of::<WndClassExW>() == 80);
const _: () = assert!(size_of::<BitmapInfoHeader>() == 40);

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
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
    fn DefWindowProcW(hwnd: Hwnd, message: u32, w_param: usize, l_param: isize) -> isize;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn GetDC(hwnd: Hwnd) -> Hdc;
    fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
    fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn AdjustWindowRect(rect: *mut Rect, style: u32, menu: i32) -> i32;
    fn ClientToScreen(hwnd: Hwnd, point: *mut PointStruct) -> i32;
    fn ClipCursor(rect: *const Rect) -> i32;
    fn ShowCursor(show: i32) -> i32;
    fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> i32;
    fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetProcessDPIAware() -> i32;
    fn GetDpiForSystem() -> u32;
    fn GetRawInputData(
        handle: *mut c_void,
        command: u32,
        data: *mut c_void,
        size: *mut u32,
        header_size: u32,
    ) -> u32;
    fn RegisterRawInputDevices(devices: *const RawInputDevice, count: u32, size: u32) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn DragAcceptFiles(hwnd: Hwnd, accept: i32);
    fn DragQueryFileW(drop: *mut c_void, index: u32, buffer: *mut u16, size: u32) -> u32;
    fn DragFinish(drop: *mut c_void);
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn SetStretchBltMode(hdc: Hdc, mode: i32) -> i32;
    fn StretchDIBits(
        hdc: Hdc,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        source_x: i32,
        source_y: i32,
        source_width: i32,
        source_height: i32,
        bits: *const c_void,
        info: *const BitmapInfoHeader,
        usage: u32,
        operation: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    fn FreeConsole() -> i32;
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
}

/// Something that happened in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The user closed the window.
    Close,
    /// A key changed. Auto-repeat is not reported.
    Key {
        /// Set 1 scancode.
        scancode: u8,
        /// Whether the key has an 0xE0 prefix.
        extended: bool,
        /// True for press.
        down: bool,
    },
    /// The cursor moved, in client coordinates.
    Pointer {
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
    },
    /// A button changed, at client coordinates.
    Button {
        /// Which button.
        button: Button,
        /// True for press.
        down: bool,
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
    },
    /// Relative mouse motion, unaffected by the screen edge.
    RawMouse {
        /// Horizontal counts.
        dx: i32,
        /// Vertical counts.
        dy: i32,
    },
    /// Window focus changed.
    Focus(bool),
    /// Windows needs the contents drawn again, such as after a restore.
    Repaint,
    /// A file was dragged onto the window.
    Dropped(PathBuf),
}

thread_local! {
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

fn push(event: Event) {
    EVENTS.with_borrow_mut(|events| events.push(event));
}

fn low_word(value: isize) -> i32 {
    i32::from(value as i16)
}

fn high_word(value: isize) -> i32 {
    i32::from((value >> 16) as i16)
}

fn pointer_button(message: u32, l_param: isize) -> Option<Event> {
    let (button, down) = match message {
        WM_LBUTTONDOWN => (Button::Left, true),
        WM_LBUTTONUP => (Button::Left, false),
        WM_RBUTTONDOWN => (Button::Right, true),
        WM_RBUTTONUP => (Button::Right, false),
        WM_MBUTTONDOWN => (Button::Middle, true),
        WM_MBUTTONUP => (Button::Middle, false),
        _ => return None,
    };
    Some(Event::Button {
        button,
        down,
        x: low_word(l_param),
        y: high_word(l_param),
    })
}

fn key_event(message: u32, l_param: isize) -> Option<Event> {
    let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
        return None;
    }
    if down && l_param & REPEAT_BIT != 0 {
        return None;
    }
    Some(Event::Key {
        scancode: ((l_param as u32) >> SCANCODE_SHIFT) as u8,
        extended: l_param & EXTENDED_BIT != 0,
        down,
    })
}

fn read_raw_mouse(l_param: isize) -> Option<Event> {
    let mut buffer = [0_u64; 8];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: `buffer` is writable for `size` bytes, and `l_param` is the
    // HRAWINPUT Windows passed with WM_INPUT.
    let copied = unsafe {
        GetRawInputData(
            l_param as *mut c_void,
            RID_INPUT,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            RAW_HEADER_SIZE,
        )
    };
    if copied == u32::MAX || copied < 48 {
        return None;
    }
    let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_le_bytes()).collect();
    let word =
        |at: usize| i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    (word(0).cast_unsigned() == RIM_TYPEMOUSE).then(|| Event::RawMouse {
        dx: word(RAW_MOUSE_DX),
        dy: word(RAW_MOUSE_DY),
    })
}

unsafe extern "system" fn window_proc(
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    let event = match message {
        WM_CLOSE => {
            push(Event::Close);
            return 0;
        }
        WM_DESTROY => {
            // SAFETY: PostQuitMessage has no preconditions.
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        WM_ACTIVATE => Some(Event::Focus(w_param & 0xffff != 0)),
        WM_PAINT => Some(Event::Repaint),
        WM_DROPFILES => {
            // SAFETY: `w_param` is the HDROP Windows passed with WM_DROPFILES.
            let path = unsafe { dropped_file(w_param as *mut c_void) };
            path.map(Event::Dropped)
        }
        WM_INPUT => read_raw_mouse(l_param),
        WM_MOUSEMOVE => Some(Event::Pointer {
            x: low_word(l_param),
            y: high_word(l_param),
        }),
        _ => pointer_button(message, l_param).or_else(|| key_event(message, l_param)),
    };
    if let Some(event) = event {
        push(event);
        // Alt and F10 would otherwise open the system menu.
        if matches!(message, WM_SYSKEYDOWN | WM_SYSKEYUP) {
            return 0;
        }
    }
    // SAFETY: the arguments are exactly those Windows passed in.
    unsafe { DefWindowProcW(hwnd, message, w_param, l_param) }
}

/// Reads the first dropped file and releases the drop handle.
///
/// # Safety
///
/// `drop` must be a live HDROP, which this call consumes.
unsafe fn dropped_file(drop: *mut c_void) -> Option<PathBuf> {
    let mut buffer = [0_u16; 1024];
    // SAFETY: the caller guarantees `drop`, and the buffer size is passed.
    let length = unsafe { DragQueryFileW(drop, 0, buffer.as_mut_ptr(), buffer.len() as u32) };
    // SAFETY: the handle is live and is released only here.
    unsafe { DragFinish(drop) };
    let length = (length as usize).min(buffer.len());
    (length > 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..length])))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(api: &'static str) -> Error {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    Error::Win32 { api, code }
}

/// Detaches the console window when this process alone owns it, which means
/// it was started by double-clicking rather than from a terminal.
pub fn hide_own_console() {
    let mut processes = [0_u32; 2];
    // SAFETY: the buffer is writable for the two entries passed.
    let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) };
    if count == 1 {
        // SAFETY: FreeConsole has no preconditions.
        unsafe { FreeConsole() };
    }
}

/// An open window. Not `Send`: Win32 windows belong to their thread.
pub struct Window {
    hwnd: Hwnd,
    hdc: Hdc,
    captured: bool,
}

impl Window {
    /// Opens a window with a client area of `width` by `height`.
    pub fn new(title: &str, width: i32, height: i32) -> Result<Self> {
        // Without this, Windows would stretch and blur the window on a
        // scaled display.
        // SAFETY: neither call has preconditions.
        let scale = unsafe {
            SetProcessDPIAware();
            GetDpiForSystem()
        };
        let scale = (scale / BASE_DPI).max(1) as i32 + i32::from(scale % BASE_DPI >= BASE_DPI / 2);
        let (width, height) = (width * scale, height * scale);
        let class_name = wide("RaxWindow");
        let title = wide(title);
        // SAFETY: a null name asks for this executable's own handle.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = WndClassExW {
            size: size_of::<WndClassExW>() as u32,
            style: CS_HREDRAW_VREDRAW_OWNDC,
            wnd_proc: window_proc,
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: std::ptr::null_mut(),
            // SAFETY: IDC_ARROW is a predefined cursor id, passed the
            // documented way as an integer in place of a name pointer.
            cursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16) },
            background: std::ptr::null_mut(),
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
            small_icon: std::ptr::null_mut(),
        };
        // SAFETY: `class` is fully initialised and its strings outlive the call.
        // A repeat registration fails harmlessly, and creation below checks.
        unsafe { RegisterClassExW(&raw const class) };

        let mut rect = Rect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        // SAFETY: `rect` is a valid in-out pointer.
        unsafe { AdjustWindowRect(&raw mut rect, WS_OVERLAPPEDWINDOW, 0) };
        // SAFETY: the strings are NUL-terminated and outlive the call, and
        // null parent and menu are valid for a top-level window.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(last_error("CreateWindowExW"));
        }
        // SAFETY: `hwnd` is the live window just created.
        let hdc = unsafe { GetDC(hwnd) };
        // SAFETY: `hdc` is the live device context just obtained.
        unsafe { SetStretchBltMode(hdc, COLORONCOLOR) };
        // SAFETY: as above.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            DragAcceptFiles(hwnd, 1);
        }
        let device = RawInputDevice {
            usage_page: USAGE_PAGE_GENERIC,
            usage: USAGE_MOUSE,
            flags: 0,
            target: hwnd,
        };
        // SAFETY: one valid device is passed, with its true size.
        unsafe {
            RegisterRawInputDevices(&raw const device, 1, size_of::<RawInputDevice>() as u32);
        }
        Ok(Self {
            hwnd,
            hdc,
            captured: false,
        })
    }

    /// Returns everything that has happened since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        // SAFETY: Msg is plain data, so all-zero is a valid value.
        let mut msg: Msg = unsafe { std::mem::zeroed() };
        // SAFETY: `msg` is a valid out pointer. The loop dispatches every
        // message to this thread's window procedure.
        while unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            // SAFETY: `msg` was just filled by PeekMessageW.
            unsafe { DispatchMessageW(&raw const msg) };
        }
        EVENTS.with_borrow_mut(std::mem::take)
    }

    /// The client area size.
    #[must_use]
    pub fn client_size(&self) -> (i32, i32) {
        let mut rect = Rect::default();
        // SAFETY: `hwnd` is live and `rect` is a valid out pointer.
        unsafe { GetClientRect(self.hwnd, &raw mut rect) };
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    /// Changes the title bar text.
    pub fn set_title(&self, title: &str) {
        let text = wide(title);
        // SAFETY: `hwnd` is live and `text` is NUL-terminated.
        unsafe { SetWindowTextW(self.hwnd, text.as_ptr()) };
    }

    /// Shows a BGRX image, stretched to fill the window.
    ///
    /// # Panics
    ///
    /// If `pixels` is smaller than `width * height * 4`.
    pub fn present(&self, pixels: &[u8], width: i32, height: i32) {
        assert!(pixels.len() >= (width * height * 4) as usize);
        let (client_width, client_height) = self.client_size();
        let info = BitmapInfoHeader {
            size: size_of::<BitmapInfoHeader>() as u32,
            width,
            height: -height,
            planes: 1,
            bit_count: 32,
            compression: 0,
            size_image: 0,
            x_pels: 0,
            y_pels: 0,
            clr_used: 0,
            clr_important: 0,
        };
        // SAFETY: the assertion proves `pixels` holds the whole image that
        // `info` describes, and `hdc` belongs to this live window.
        unsafe {
            StretchDIBits(
                self.hdc,
                0,
                0,
                client_width,
                client_height,
                0,
                0,
                width,
                height,
                pixels.as_ptr().cast(),
                &raw const info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }

    /// Confines and hides the cursor, or releases it.
    pub fn set_capture(&mut self, capture: bool) {
        if self.captured == capture {
            return;
        }
        self.captured = capture;
        if capture {
            let (width, height) = self.client_size();
            let mut origin = PointStruct::default();
            // SAFETY: `hwnd` is live and `origin` is a valid in-out pointer.
            unsafe { ClientToScreen(self.hwnd, &raw mut origin) };
            let area = Rect {
                left: origin.x,
                top: origin.y,
                right: origin.x + width,
                bottom: origin.y + height,
            };
            // SAFETY: `area` is a valid rectangle for the duration of the call.
            unsafe {
                ClipCursor(&raw const area);
                ShowCursor(0);
            }
        } else {
            // SAFETY: a null rectangle releases the cursor.
            unsafe {
                ClipCursor(std::ptr::null());
                ShowCursor(1);
            }
        }
    }

    /// Whether the mouse is captured.
    #[must_use]
    pub fn is_captured(&self) -> bool {
        self.captured
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.set_capture(false);
        // SAFETY: `hdc` and `hwnd` are live and released only here.
        unsafe {
            ReleaseDC(self.hwnd, self.hdc);
            DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancode_and_flags_decode_from_l_param() {
        let l_param = (0x48 << SCANCODE_SHIFT) | EXTENDED_BIT;
        assert_eq!(
            key_event(WM_KEYDOWN, l_param),
            Some(Event::Key {
                scancode: 0x48,
                extended: true,
                down: true
            })
        );
    }

    #[test]
    fn auto_repeat_is_dropped() {
        assert_eq!(key_event(WM_KEYDOWN, REPEAT_BIT), None);
    }

    #[test]
    fn releases_are_reported_even_with_the_previous_state_bit() {
        assert!(matches!(
            key_event(WM_KEYUP, REPEAT_BIT),
            Some(Event::Key { down: false, .. })
        ));
    }

    #[test]
    fn other_messages_are_not_keys() {
        assert_eq!(key_event(WM_MOUSEMOVE, 0), None);
    }

    #[test]
    fn pointer_coordinates_are_signed() {
        let l_param = ((-3_i16 as u16 as isize) << 16) | 7;
        assert_eq!(
            pointer_button(WM_LBUTTONDOWN, l_param),
            Some(Event::Button {
                button: Button::Left,
                down: true,
                x: 7,
                y: -3
            })
        );
    }
}
