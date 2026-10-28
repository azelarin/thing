#[cfg(feature = "vulkan")]
use std::time::Duration;

use thing_core::Node;
use thing_ui::{Layout, PaintFrame, Painter, ScrollState, Size};
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
}

pub enum Window {
    OpenGl(egl::Window),
    #[cfg(feature = "vulkan")]
    Vulkan(vulkan::Window),
}

impl Drop for Render {
    fn drop(&mut self) {
        self.prepare_drop();
    }
}

impl Render {
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
            #[cfg(feature = "vulkan")]
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
        }
    }

    pub fn check_error(&self) -> Result<(), String> {
        match &self.backend {
            Backend::OpenGl { .. } => Ok(()),
            #[cfg(feature = "vulkan")]
            Backend::Vulkan { graphics, .. } => graphics.check_error(),
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
        }
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
