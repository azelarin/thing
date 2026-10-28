//! vulkan surface ownership and presentation through wgpu.

use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use thing_ui::{PaintFrame, Painter, WGPURenderer};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};
use wgpu::{
    Backends, CompositeAlphaMode, CurrentSurfaceTexture, Device, DeviceDescriptor, Instance,
    InstanceDescriptor, PresentMode, Queue, RequestAdapterOptions, SurfaceCapabilities,
    SurfaceColorSpace, SurfaceConfiguration, TextureFormat, TextureUsages,
};

use crate::render::FrameResult;

const RETRY_DELAY: Duration = Duration::from_millis(16);
const OCCLUDED_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct Graphics {
    instance: Instance,
    adapter: wgpu::Adapter,
    device: Device,
    queue: Queue,
    errors: Arc<Mutex<Option<String>>>,
    connection: Connection,
}

pub struct Window {
    // drop the wgpu surface before its native Wayland handles.
    surface: Option<wgpu::Surface<'static>>,
    configuration: Option<SurfaceConfiguration>,
    needs_reconfigure: bool,
    wayland: WlSurface,
    connection: Connection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SurfaceChoice {
    format: TextureFormat,
    present_mode: PresentMode,
    alpha_mode: CompositeAlphaMode,
}

impl Graphics {
    pub fn new(
        connection: &Connection,
        probe: &WlSurface,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let instance = Instance::new(InstanceDescriptor {
            backends: Backends::VULKAN,
            ..InstanceDescriptor::new_without_display_handle()
        });
        let surface = create_surface(&instance, connection, probe)?;
        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..RequestAdapterOptions::default()
        }))
        .map_err(|error| format!("cannot find a vulkan adapter for Wayland: {error}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("thing vulkan renderer"),
            ..DeviceDescriptor::default()
        }))
        .map_err(|error| format!("cannot create a vulkan device: {error}"))?;
        drop(surface);

        let errors = Arc::new(Mutex::new(None));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let uncaptured = errors.clone();
        let uncaptured_wake = wake.clone();
        device.on_uncaptured_error(Arc::new(move |error| {
            if record_error(&uncaptured, format!("vulkan renderer error: {error}")) {
                uncaptured_wake();
            }
        }));
        let lost = errors.clone();
        let lost_wake = wake.clone();
        device.set_device_lost_callback(move |reason, message| {
            let detail = if message.is_empty() {
                format!("vulkan device lost: {reason:?}")
            } else {
                format!("vulkan device lost ({reason:?}): {message}")
            };
            if record_error(&lost, detail) {
                lost_wake();
            }
        });

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            errors,
            connection: connection.clone(),
        })
    }

    pub fn renderer(&self) -> WGPURenderer {
        WGPURenderer::new(self.device.clone(), self.queue.clone())
    }

    pub fn check_error(&self) -> Result<(), String> {
        match lock(&self.errors).as_ref() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl Window {
    pub fn new(
        graphics: &Graphics,
        wayland: &WlSurface,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let surface = create_surface(&graphics.instance, &graphics.connection, wayland)?;
        let mut window = Self {
            surface: Some(surface),
            configuration: None,
            needs_reconfigure: true,
            wayland: wayland.clone(),
            connection: graphics.connection.clone(),
        };
        window.configure(graphics, width, height)?;
        Ok(window)
    }

    fn ensure_configured(
        &mut self,
        graphics: &Graphics,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let dimensions_changed = self
            .configuration
            .as_ref()
            .is_none_or(|config| config.width != width || config.height != height);
        if self.needs_reconfigure || dimensions_changed {
            self.configure(graphics, width, height)?;
        }
        Ok(())
    }

    fn configure(&mut self, graphics: &Graphics, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Err("vulkan surface dimensions must be positive".into());
        }
        let maximum = graphics.device.limits().max_texture_dimension_2d;
        if width > maximum || height > maximum {
            return Err(format!(
                "vulkan surface dimensions exceed the device limit of {maximum} pixels"
            ));
        }
        let surface = self.surface()?;
        let choice = choose_surface(&surface.get_capabilities(&graphics.adapter))?;
        let configuration = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: choice.format,
            color_space: SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: choice.present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: choice.alpha_mode,
            view_formats: Vec::new(),
        };
        surface.configure(&graphics.device, &configuration);
        graphics.check_error()?;
        self.configuration = Some(configuration);
        self.needs_reconfigure = false;
        Ok(())
    }

    fn recreate(&mut self, graphics: &Graphics, width: u32, height: u32) -> Result<(), String> {
        self.surface.take();
        self.surface = Some(create_surface(
            &graphics.instance,
            &self.connection,
            &self.wayland,
        )?);
        self.configuration = None;
        self.needs_reconfigure = true;
        self.configure(graphics, width, height)
    }

    fn surface(&self) -> Result<&wgpu::Surface<'static>, String> {
        self.surface
            .as_ref()
            .ok_or_else(|| "vulkan surface is unavailable".into())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.surface.take();
    }
}

pub fn draw(
    graphics: &mut Graphics,
    painter: &mut Painter<WGPURenderer>,
    window: &mut Window,
    frame: PaintFrame<'_>,
    before_present: impl FnOnce(),
) -> Result<FrameResult, String> {
    graphics.check_error()?;
    window.ensure_configured(graphics, frame.physical.0, frame.physical.1)?;

    let (surface_texture, suboptimal) = match window.surface()?.get_current_texture() {
        CurrentSurfaceTexture::Success(texture) => (texture, false),
        CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
        CurrentSurfaceTexture::Timeout => return Ok(FrameResult::RetryAfter(RETRY_DELAY)),
        CurrentSurfaceTexture::Occluded => {
            return Ok(FrameResult::RetryAfter(OCCLUDED_RETRY_DELAY));
        }
        CurrentSurfaceTexture::Outdated => {
            window.needs_reconfigure = true;
            window.ensure_configured(graphics, frame.physical.0, frame.physical.1)?;
            return Ok(FrameResult::RetryAfter(RETRY_DELAY));
        }
        CurrentSurfaceTexture::Lost => {
            window.recreate(graphics, frame.physical.0, frame.physical.1)?;
            return Ok(FrameResult::RetryAfter(RETRY_DELAY));
        }
        CurrentSurfaceTexture::Validation => {
            return graphics
                .check_error()
                .and_then(|()| Err("vulkan rejected the surface while acquiring a frame".into()));
        }
    };

    let output = thing_ui::WGPURenderOutput::from(&surface_texture.texture);
    painter.paint_to_output(frame, output, |commands| {
        let _submission = graphics.queue.submit(commands);
    })?;
    graphics.check_error()?;
    before_present();
    graphics.queue.present(surface_texture);
    graphics.check_error()?;

    if suboptimal {
        window.needs_reconfigure = true;
    }
    Ok(FrameResult::Presented { redraw: suboptimal })
}

fn create_surface(
    instance: &Instance,
    connection: &Connection,
    surface: &WlSurface,
) -> Result<wgpu::Surface<'static>, String> {
    let display = NonNull::new(connection.backend().display_ptr().cast::<c_void>())
        .ok_or("the Wayland display is unavailable")?;
    let window = NonNull::new(surface.id().as_ptr().cast::<c_void>())
        .ok_or("the Wayland surface has been destroyed")?;
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            display,
        ))),
        raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
    };
    // SAFETY: callers keep both proxies live until the returned surface is
    // dropped. Window stores clones and drops its surface first; initialization
    // drops the temporary probe surface before its borrowed proxies.
    unsafe { instance.create_surface_unsafe(target) }
        .map_err(|error| format!("cannot create a vulkan Wayland surface: {error}"))
}

fn choose_surface(capabilities: &SurfaceCapabilities) -> Result<SurfaceChoice, String> {
    if !capabilities
        .usages
        .contains(TextureUsages::RENDER_ATTACHMENT)
    {
        return Err("vulkan surface cannot be used as a render target".into());
    }
    let format = capabilities
        .formats
        .iter()
        .copied()
        .find(|format| {
            matches!(
                format,
                TextureFormat::Rgba8Unorm | TextureFormat::Bgra8Unorm
            )
        })
        .ok_or("vulkan surface does not expose an 8-bit linear RGBA or BGRA format")?;
    let alpha_mode = capabilities
        .alpha_modes
        .contains(&CompositeAlphaMode::PreMultiplied)
        .then_some(CompositeAlphaMode::PreMultiplied)
        .ok_or("vulkan surface does not support premultiplied alpha")?;
    let present_mode = if capabilities.present_modes.contains(&PresentMode::Mailbox) {
        PresentMode::Mailbox
    } else if capabilities.present_modes.contains(&PresentMode::Fifo) {
        PresentMode::Fifo
    } else {
        return Err("vulkan surface does not support mailbox or fifo presentation".into());
    };
    Ok(SurfaceChoice {
        format,
        present_mode,
        alpha_mode,
    })
}

fn record_error(errors: &Mutex<Option<String>>, error: String) -> bool {
    let mut errors = lock(errors);
    if errors.is_none() {
        *errors = Some(error);
        true
    } else {
        false
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> SurfaceCapabilities {
        SurfaceCapabilities {
            formats: vec![TextureFormat::Rgba8Unorm],
            present_modes: vec![PresentMode::Mailbox, PresentMode::Fifo],
            alpha_modes: vec![CompositeAlphaMode::PreMultiplied],
            ..SurfaceCapabilities::default()
        }
    }

    #[test]
    fn surface_selection_prefers_linear_format_mailbox_and_premultiplied_alpha() {
        assert_eq!(
            choose_surface(&capabilities()).unwrap(),
            SurfaceChoice {
                format: TextureFormat::Rgba8Unorm,
                present_mode: PresentMode::Mailbox,
                alpha_mode: CompositeAlphaMode::PreMultiplied,
            }
        );
    }

    #[test]
    fn surface_selection_accepts_bgra_and_fifo() {
        let mut capabilities = capabilities();
        capabilities.formats = vec![TextureFormat::Bgra8Unorm];
        capabilities.present_modes = vec![PresentMode::Fifo];
        let choice = choose_surface(&capabilities).unwrap();
        assert_eq!(choice.format, TextureFormat::Bgra8Unorm);
        assert_eq!(choice.present_mode, PresentMode::Fifo);
    }

    #[test]
    fn surface_selection_rejects_incompatible_format_alpha_and_present_mode() {
        let mut unsupported = capabilities();
        unsupported.formats = vec![TextureFormat::Bgra8UnormSrgb];
        assert!(choose_surface(&unsupported).unwrap_err().contains("linear"));

        let mut unsupported = capabilities();
        unsupported.alpha_modes = vec![CompositeAlphaMode::Opaque];
        assert!(
            choose_surface(&unsupported)
                .unwrap_err()
                .contains("premultiplied alpha")
        );

        let mut unsupported = capabilities();
        unsupported.present_modes = vec![PresentMode::Immediate];
        assert!(
            choose_surface(&unsupported)
                .unwrap_err()
                .contains("mailbox or fifo")
        );
    }
}
