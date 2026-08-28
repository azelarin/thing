//! egl ownership boundary. every graphics object stays on the event-loop thread.

use std::{
    ffi::{c_char, c_int, c_uint, c_void},
    ptr,
    rc::Rc,
};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

type Handle = *mut c_void;
const NONE: c_int = 0x3038;
const PLATFORM_WAYLAND: c_uint = 0x31D8;
const OPENGL_ES_API: c_uint = 0x30A0;
const SURFACE_TYPE: c_int = 0x3033;
const WINDOW_BIT: c_int = 0x0004;
const PBUFFER_BIT: c_int = 0x0001;
const RENDERABLE_TYPE: c_int = 0x3040;
const OPENGL_ES2_BIT: c_int = 0x0004;
const RED_SIZE: c_int = 0x3024;
const GREEN_SIZE: c_int = 0x3023;
const BLUE_SIZE: c_int = 0x3022;
const ALPHA_SIZE: c_int = 0x3021;
const STENCIL_SIZE: c_int = 0x3026;
const CONTEXT_CLIENT_VERSION: c_int = 0x3098;
const WIDTH: c_int = 0x3057;
const HEIGHT: c_int = 0x3056;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetPlatformDisplay(platform: c_uint, native: Handle, attributes: *const isize) -> Handle;
    fn eglInitialize(display: Handle, major: *mut c_int, minor: *mut c_int) -> c_uint;
    fn eglTerminate(display: Handle) -> c_uint;
    fn eglBindAPI(api: c_uint) -> c_uint;
    fn eglChooseConfig(
        display: Handle,
        attributes: *const c_int,
        configs: *mut Handle,
        count: c_int,
        found: *mut c_int,
    ) -> c_uint;
    fn eglCreateContext(
        display: Handle,
        config: Handle,
        shared: Handle,
        attributes: *const c_int,
    ) -> Handle;
    fn eglDestroyContext(display: Handle, context: Handle) -> c_uint;
    fn eglCreateWindowSurface(
        display: Handle,
        config: Handle,
        window: Handle,
        attributes: *const c_int,
    ) -> Handle;
    fn eglCreatePbufferSurface(display: Handle, config: Handle, attributes: *const c_int)
    -> Handle;
    fn eglDestroySurface(display: Handle, surface: Handle) -> c_uint;
    fn eglMakeCurrent(display: Handle, draw: Handle, read: Handle, context: Handle) -> c_uint;
    fn eglSwapBuffers(display: Handle, surface: Handle) -> c_uint;
    fn eglSwapInterval(display: Handle, interval: c_int) -> c_uint;
    fn eglGetError() -> c_int;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}

#[link(name = "wayland-egl")]
unsafe extern "C" {
    fn wl_egl_window_create(surface: Handle, width: c_int, height: c_int) -> Handle;
    fn wl_egl_window_resize(window: Handle, width: c_int, height: c_int, dx: c_int, dy: c_int);
    fn wl_egl_window_destroy(window: Handle);
}

struct Display {
    raw: Handle,
    // keep libwayland's display alive until egl has released it.
    _connection: Connection,
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: this is the last owner; all egl surfaces and contexts are gone.
        unsafe {
            eglTerminate(self.raw);
        }
    }
}

pub struct Graphics {
    display: Rc<Display>,
    config: Handle,
    context: Handle,
    // a 1×1 surface keeps cleanup valid even after the last window disappears.
    parking: Handle,
}

pub struct Window {
    display: Rc<Display>,
    surface: Handle,
    window: Handle,
    _wayland: WlSurface,
}

impl Graphics {
    pub fn new(connection: &Connection) -> Result<Self, String> {
        // SAFETY: the connection uses wayland-client's system backend. its clone
        // remains owned by `Display` for every call involving this native pointer.
        unsafe {
            let raw = eglGetPlatformDisplay(
                PLATFORM_WAYLAND,
                connection.backend().display_ptr().cast(),
                ptr::null(),
            );
            if raw.is_null() || eglInitialize(raw, ptr::null_mut(), ptr::null_mut()) == 0 {
                return Err(error("initialize EGL"));
            }
            let display = Rc::new(Display {
                raw,
                _connection: connection.clone(),
            });
            if eglBindAPI(OPENGL_ES_API) == 0 {
                return Err(error("select OpenGL ES"));
            }
            let attributes = [
                SURFACE_TYPE,
                WINDOW_BIT | PBUFFER_BIT,
                RENDERABLE_TYPE,
                OPENGL_ES2_BIT,
                RED_SIZE,
                8,
                GREEN_SIZE,
                8,
                BLUE_SIZE,
                8,
                ALPHA_SIZE,
                8, // RGBA
                STENCIL_SIZE,
                8, // stencil buffer for vector clipping
                NONE,
            ];
            let mut config = ptr::null_mut();
            let mut found = 0;
            if eglChooseConfig(raw, attributes.as_ptr(), &mut config, 1, &mut found) == 0
                || found == 0
            {
                return Err(error("choose an RGBA OpenGL ES configuration"));
            }
            let context = eglCreateContext(
                raw,
                config,
                ptr::null_mut(),
                [CONTEXT_CLIENT_VERSION, 2, NONE].as_ptr(),
            );
            if context.is_null() {
                return Err(error("create an OpenGL ES context"));
            }
            let parking =
                eglCreatePbufferSurface(raw, config, [WIDTH, 1, HEIGHT, 1, NONE].as_ptr());
            if parking.is_null() {
                let failure = error("create the EGL cleanup surface");
                eglDestroyContext(raw, context);
                return Err(failure);
            }
            let result = Self {
                display,
                config,
                context,
                parking,
            };
            result.park()?;
            Ok(result)
        }
    }

    pub fn renderer(&self) -> Result<thing_ui::OpenGl, String> {
        self.park()?;
        // SAFETY: park made our live opengl es context current on this thread.
        // egl provides the matching function pointers, valid for its lifetime.
        unsafe { thing_ui::OpenGl::new_from_function_cstr(|name| eglGetProcAddress(name.as_ptr())) }
            .map_err(|error| error.to_string())
    }

    pub fn window(&self, surface: &WlSurface, width: u32, height: u32) -> Result<Window, String> {
        let (width, height) = dimensions(width, height)?;
        let pointer = surface.id().as_ptr();
        if pointer.is_null() {
            return Err("the Wayland surface has been destroyed".into());
        }
        // SAFETY: the system-backend proxy is live and cloned into the returned
        // object. dimensions are positive and fit the c api's signed integers.
        unsafe {
            let window = wl_egl_window_create(pointer.cast(), width, height);
            if window.is_null() {
                return Err("could not allocate a Wayland EGL window".into());
            }
            let egl_surface =
                eglCreateWindowSurface(self.display.raw, self.config, window, [NONE].as_ptr());
            if egl_surface.is_null() {
                let failure = error("create an EGL window surface");
                wl_egl_window_destroy(window);
                return Err(failure);
            }
            Ok(Window {
                display: self.display.clone(),
                surface: egl_surface,
                window,
                _wayland: surface.clone(),
            })
        }
    }

    pub fn make_current(&self, window: &Window) -> Result<(), String> {
        // SAFETY: the surface and context belong to the same live `Display`. `Rc`
        // makes these objects non-`Send`, keeping context use on one thread.
        unsafe {
            if eglMakeCurrent(
                self.display.raw,
                window.surface,
                window.surface,
                self.context,
            ) == 0
            {
                return Err(error("make the EGL context current"));
            }
            // wayland frame callbacks schedule drawing. avoid an additional
            // blocking vsync wait in the ui event loop.
            eglSwapInterval(self.display.raw, 0);
        }
        Ok(())
    }

    pub fn park(&self) -> Result<(), String> {
        // SAFETY: the parking surface and context are owned by this object.
        if unsafe { eglMakeCurrent(self.display.raw, self.parking, self.parking, self.context) }
            == 0
        {
            return Err(error("activate the EGL cleanup surface"));
        }
        Ok(())
    }
}

impl Window {
    pub fn resize(&self, width: u32, height: u32) -> Result<(), String> {
        let (width, height) = dimensions(width, height)?;
        // SAFETY: window is live and dimensions have been range checked.
        unsafe {
            wl_egl_window_resize(self.window, width, height, 0, 0);
        }
        Ok(())
    }

    pub fn present(&self) -> Result<(), String> {
        // SAFETY: the host has made this live surface current before painting.
        if unsafe { eglSwapBuffers(self.display.raw, self.surface) } == 0 {
            return Err(error("present the EGL buffer"));
        }
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: destroy the egl wrapper before its native window. `Display` and
        // `WlSurface` owners remain live throughout this destructor.
        unsafe {
            eglDestroySurface(self.display.raw, self.surface);
            wl_egl_window_destroy(self.window);
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        // SAFETY: the host drops its painter first. detach the context before
        // releasing it and the cleanup surface; `Display` is still owned here.
        unsafe {
            eglMakeCurrent(
                self.display.raw,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            eglDestroySurface(self.display.raw, self.parking);
            eglDestroyContext(self.display.raw, self.context);
        }
    }
}

fn dimensions(width: u32, height: u32) -> Result<(i32, i32), String> {
    if width == 0 || height == 0 || width > 32768 || height > 32768 {
        return Err("EGL surface dimensions must be between 1 and 32,768 pixels".into());
    }
    Ok((width as i32, height as i32))
}

fn error(action: &str) -> String {
    // SAFETY: `eglGetError` reads and clears this thread's egl error state.
    format!("failed to {action}: EGL error 0x{:04x}", unsafe {
        eglGetError()
    })
}
