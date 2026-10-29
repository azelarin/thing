#[cfg(feature = "vulkan")]
use std::time::Duration;

use thing_core::Node;
#[cfg(test)]
use thing_ui::Measurer;
use thing_ui::{Layout, PaintFrame, Painter, ScrollState, Size};
#[cfg(test)]
use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, protocol::wl_surface::WlSurface};

use crate::{Renderer, egl};

#[cfg(feature = "vulkan")]
use crate::skia_vulkan as vulkan;

pub enum FrameResult {
    Presented {
        redraw: bool,
    },
    #[cfg(feature = "vulkan")]
    RetryAfter(Duration),
}

pub struct Render {
    backend: Backend,
}

enum Backend {
    OpenGl {
        graphics: egl::Graphics,
        painter: Option<Box<Painter>>,
    },
    #[cfg(feature = "vulkan")]
    Vulkan {
        graphics: vulkan::Graphics,
        painter: Option<Box<Painter>>,
    },
    #[cfg(test)]
    Test {
        buffer: wl_buffer::WlBuffer,
        size: (u32, u32),
    },
}

pub enum Window {
    OpenGl(egl::Window),
    #[cfg(feature = "vulkan")]
    Vulkan(vulkan::Window),
    #[cfg(test)]
    Test(WlSurface),
}

impl Drop for Render {
    fn drop(&mut self) {
        self.prepare_drop();
    }
}

impl Render {
    #[cfg(test)]
    pub(crate) fn new_test(buffer: wl_buffer::WlBuffer, size: (u32, u32)) -> Self {
        Self {
            backend: Backend::Test { buffer, size },
        }
    }

    pub fn new(
        renderer: Renderer,
        connection: &Connection,
        probe: Option<&WlSurface>,
        wake: impl Fn() + Send + 'static,
        gpu_wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        match renderer {
            Renderer::OpenGl => {
                let _ = gpu_wake;
                let graphics = egl::Graphics::new(connection)?;
                let painter = graphics.painter(wake)?;
                Ok(Self {
                    backend: Backend::OpenGl {
                        graphics,
                        painter: Some(Box::new(painter)),
                    },
                })
            }
            Renderer::Vulkan => {
                #[cfg(feature = "vulkan")]
                {
                    let probe = probe.ok_or("vulkan initialization needs a Wayland surface")?;
                    let (graphics, context) = vulkan::Graphics::new(connection, probe, gpu_wake)?;
                    let painter = Painter::new_vulkan(context, wake)?;
                    Ok(Self {
                        backend: Backend::Vulkan {
                            graphics,
                            painter: Some(Box::new(painter)),
                        },
                    })
                }
                #[cfg(not(feature = "vulkan"))]
                {
                    let _ = (connection, probe, wake, gpu_wake);
                    Err("vulkan support is not included in this build; rebuild with --features vulkan"
                        .into())
                }
            }
        }
    }

    pub fn reset_painter(&mut self, wake: impl Fn() + Send + 'static) -> Result<(), String> {
        match &mut self.backend {
            Backend::OpenGl { graphics, painter } => {
                graphics.park()?;
                painter
                    .as_deref_mut()
                    .ok_or("OpenGL painter is unavailable")?
                    .reset(wake)?;
            }
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, painter } => {
                graphics.wait_idle()?;
                painter
                    .as_deref_mut()
                    .ok_or("vulkan painter is unavailable")?
                    .reset(wake)?;
            }
            #[cfg(test)]
            Backend::Test { .. } => {
                let _ = wake;
            }
        }
        Ok(())
    }

    pub fn create_window(
        &self,
        surface: &WlSurface,
        width: u32,
        height: u32,
    ) -> Result<Window, String> {
        match &self.backend {
            Backend::OpenGl { graphics, .. } => {
                graphics.window(surface, width, height).map(Window::OpenGl)
            }
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, .. } => {
                vulkan::Window::new(graphics, surface, width, height).map(Window::Vulkan)
            }
            #[cfg(test)]
            Backend::Test { size, .. } => {
                if *size != (width, height) {
                    return Err("test buffer does not match the lock surface size".into());
                }
                Ok(Window::Test(surface.clone()))
            }
        }
    }

    pub fn layout(
        &mut self,
        root: &Node,
        size: Size,
        scroll: &mut ScrollState,
    ) -> Result<Layout, String> {
        match &mut self.backend {
            Backend::OpenGl { painter, .. } => painter
                .as_deref_mut()
                .ok_or("OpenGL painter is unavailable")?
                .layout(root, size, scroll),
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { painter, .. } => painter
                .as_deref_mut()
                .ok_or("vulkan painter is unavailable")?
                .layout(root, size, scroll),
            #[cfg(test)]
            Backend::Test { .. } => {
                Layout::compute_with_scroll(root, size, scroll, &mut TestMeasurer)
            }
        }
    }

    pub fn draw(
        &mut self,
        window: &mut Window,
        frame: PaintFrame<'_>,
        before_present: impl FnOnce(),
    ) -> Result<FrameResult, String> {
        match (&mut self.backend, window) {
            (Backend::OpenGl { graphics, painter }, Window::OpenGl(window)) => {
                window.resize(frame.physical.0, frame.physical.1)?;
                graphics.make_current(window)?;
                painter
                    .as_deref_mut()
                    .ok_or("OpenGL painter is unavailable")?
                    .paint(frame, graphics.framebuffer())?;
                before_present();
                window.present()?;
                Ok(FrameResult::Presented { redraw: false })
            }
            #[cfg(feature = "vulkan")]
            (Backend::Vulkan { graphics, painter }, Window::Vulkan(window)) => vulkan::draw(
                graphics,
                painter
                    .as_deref_mut()
                    .ok_or("vulkan painter is unavailable")?,
                window,
                frame,
                before_present,
            ),
            #[cfg(test)]
            (Backend::Test { buffer, size }, Window::Test(surface)) => {
                if frame.physical != *size {
                    return Err("test frame does not match its shared-memory buffer".into());
                }
                before_present();
                surface.attach(Some(buffer), 0, 0);
                surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                surface.commit();
                Ok(FrameResult::Presented { redraw: false })
            }
            #[cfg(any(feature = "vulkan", test))]
            _ => Err("render surface belongs to a different graphics backend".into()),
        }
    }

    pub fn park(&mut self) -> Result<(), String> {
        match &mut self.backend {
            Backend::OpenGl { graphics, .. } => graphics.park(),
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, painter } => {
                graphics.wait_idle()?;
                painter
                    .as_deref_mut()
                    .ok_or("vulkan painter is unavailable")?
                    .context_mut()
                    .free_gpu_resources();
                Ok(())
            }
            #[cfg(test)]
            Backend::Test { .. } => Ok(()),
        }
    }

    pub fn check_error(&self) -> Result<(), String> {
        match &self.backend {
            Backend::OpenGl { .. } => Ok(()),
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, .. } => graphics.check_error(),
            #[cfg(test)]
            Backend::Test { .. } => Ok(()),
        }
    }

    pub fn prepare_drop(&mut self) {
        match &mut self.backend {
            Backend::OpenGl { graphics, painter } => prepare_open_gl_drop(graphics, painter),
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, painter } => {
                if painter.is_some()
                    && graphics.wait_idle().is_err()
                    && let Some(painter) = painter.as_deref_mut()
                {
                    painter.abandon();
                }
                painter.take();
            }
            #[cfg(test)]
            Backend::Test { .. } => {}
        }
    }
}

#[cfg(test)]
struct TestMeasurer;

#[cfg(test)]
impl Measurer for TestMeasurer {
    fn measure(&mut self, _: &Node, _: Option<f32>) -> Size {
        Size::default()
    }
}

fn prepare_open_gl_drop(graphics: &egl::Graphics, painter: &mut Option<Box<Painter>>) {
    if graphics.park().is_err()
        && let Some(painter) = painter.as_deref_mut()
    {
        painter.abandon();
    }
    drop(painter.take());
}
