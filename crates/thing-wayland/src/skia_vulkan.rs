//! private ash and ganesh integration for the skia vulkan renderer.

use std::{
    ffi::{CStr, c_void},
    mem::ManuallyDrop,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ash::{
    Entry,
    vk::{self, Handle},
};
use skia_safe::{
    ColorType, Surface,
    gpu::{
        self, FlushInfo, SemaphoresSubmitted, SurfaceOrigin, backend_render_targets,
        direct_contexts, surfaces, vk as skia_vk,
    },
};
use thing_ui::{PaintFrame, Painter};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

use crate::render::FrameResult;

const API_VERSION: u32 = vk::API_VERSION_1_1;
const ACQUIRE_TIMEOUT_NS: u64 = 0;
const RETRY_DELAY: Duration = Duration::from_millis(16);

fn required_image_usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

pub struct Graphics {
    shared: Arc<DeviceState>,
    connection: Connection,
}

pub struct Window {
    generation: Option<SwapchainGeneration>,
    surface: Option<vk::SurfaceKHR>,
    requested: vk::Extent2D,
    needs_reconfigure: bool,
    surface_lost: bool,
    shared: Arc<DeviceState>,
    wayland: WlSurface,
    connection: Connection,
}

struct DeviceState {
    entry: ManuallyDrop<Entry>,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    wayland_loader: ash::khr::wayland_surface::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    swapchain_loader: ash::khr::swapchain::Device,
    maintenance_loader: ash::ext::swapchain_maintenance1::Device,
    queue: vk::Queue,
    queue_family: u32,
    abandoned: AtomicBool,
}

struct SwapchainGeneration {
    shared: Arc<DeviceState>,
    handle: vk::SwapchainKHR,
    extent: vk::Extent2D,
    format: vk::Format,
    targets: Vec<FrameTarget>,
    present_fences: Vec<PresentFence>,
    acquire_fences: Vec<AcquireFence>,
}

struct FrameTarget {
    image: vk::Image,
    render_complete: vk::Semaphore,
    initialized: bool,
}

struct PresentFence {
    handle: vk::Fence,
    pending: bool,
}

struct AcquireFence {
    handle: vk::Fence,
    pending: bool,
    retained: Option<RetainedAcquire>,
}

struct RetainedAcquire {
    semaphore: vk::Semaphore,
    index: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SurfacePlan {
    format: vk::Format,
    color_space: vk::ColorSpaceKHR,
    present_mode: vk::PresentModeKHR,
    alpha: vk::CompositeAlphaFlagsKHR,
    transform: vk::SurfaceTransformFlagsKHR,
    extent: vk::Extent2D,
    image_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Recovery {
    Retry,
    Reconfigure,
    RecreateSurface,
    Fatal(vk::Result),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentOutcome {
    Queued { suboptimal: bool },
    Rejected(Recovery),
    Failed(vk::Result),
}

struct InstanceGuard {
    entry: Option<Entry>,
    instance: Option<ash::Instance>,
}

struct SurfaceGuard {
    loader: ash::khr::surface::Instance,
    handle: vk::SurfaceKHR,
}

impl Graphics {
    pub fn new(
        connection: &Connection,
        probe: &WlSurface,
        _wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<(Self, gpu::DirectContext), String> {
        // SAFETY: loading the process Vulkan loader does not create Vulkan objects.
        let entry = unsafe { Entry::load() }
            .map_err(|error| format!("cannot load the Vulkan loader: {error}"))?;
        // SAFETY: the loaded entry remains alive for this loader query.
        let supported = unsafe { entry.try_enumerate_instance_version() }
            .map_err(|error| format!("cannot query the Vulkan instance version: {error}"))?
            .unwrap_or(vk::API_VERSION_1_0);
        if supported < API_VERSION {
            return Err("Skia requires a Vulkan 1.1 loader".into());
        }
        ensure_instance_extensions(&entry)?;

        let application = vk::ApplicationInfo::default()
            .application_name(c"thing")
            .application_version(0)
            .engine_name(c"thing")
            .engine_version(0)
            .api_version(API_VERSION);
        let extension_names = [
            ash::khr::surface::NAME.as_ptr(),
            ash::khr::wayland_surface::NAME.as_ptr(),
            ash::khr::get_surface_capabilities2::NAME.as_ptr(),
            ash::ext::surface_maintenance1::NAME.as_ptr(),
        ];
        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&application)
            .enabled_extension_names(&extension_names);
        // SAFETY: create_info only points to data retained for this call.
        let instance = unsafe { entry.create_instance(&create_info, None) }
            .map_err(|error| format!("cannot create the Vulkan instance: {error}"))?;
        let mut instance_guard = InstanceGuard {
            entry: Some(entry),
            instance: Some(instance),
        };
        let entry = instance_guard
            .entry
            .as_ref()
            .expect("instance guard owns the entry");
        let instance = instance_guard
            .instance
            .as_ref()
            .expect("instance guard owns the instance");
        let surface_loader = ash::khr::surface::Instance::new(entry, instance);
        let wayland_loader = ash::khr::wayland_surface::Instance::new(entry, instance);
        let probe_surface = create_wayland_surface(&wayland_loader, connection, probe)?;
        let probe_guard = SurfaceGuard {
            loader: ash::khr::surface::Instance::new(entry, instance),
            handle: probe_surface,
        };
        let display = display_ptr(connection)?;
        let adapter = choose_adapter(
            instance,
            &surface_loader,
            &wayland_loader,
            probe_surface,
            display,
        )?;

        let priorities = [1.0];
        let queue = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(adapter.queue_family)
            .queue_priorities(&priorities);
        let device_extensions = [
            ash::khr::swapchain::NAME.as_ptr(),
            ash::ext::swapchain_maintenance1::NAME.as_ptr(),
        ];
        let mut maintenance = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default()
            .swapchain_maintenance1(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue))
            .enabled_extension_names(&device_extensions)
            .push_next(&mut maintenance);
        // SAFETY: the selected queue family and extension were checked above.
        let device = unsafe { instance.create_device(adapter.physical_device, &device_info, None) }
            .map_err(|error| format!("cannot create the Vulkan device: {error}"))?;
        // SAFETY: the device was created with one queue in this family.
        let queue = unsafe { device.get_device_queue(adapter.queue_family, 0) };

        drop(probe_guard);
        let entry = instance_guard
            .entry
            .take()
            .expect("instance guard owns the entry");
        let instance = instance_guard
            .instance
            .take()
            .expect("instance guard owns the instance");
        let entry = ManuallyDrop::new(entry);
        let shared = Arc::new(DeviceState {
            surface_loader: ash::khr::surface::Instance::new(&entry, &instance),
            wayland_loader: ash::khr::wayland_surface::Instance::new(&entry, &instance),
            swapchain_loader: ash::khr::swapchain::Device::new(&instance, &device),
            maintenance_loader: ash::ext::swapchain_maintenance1::Device::new(&instance, &device),
            entry,
            instance,
            physical_device: adapter.physical_device,
            device,
            queue,
            queue_family: adapter.queue_family,
            abandoned: AtomicBool::new(false),
        });
        let context = create_skia_context(&shared)?;
        Ok((
            Self {
                shared,
                connection: connection.clone(),
            },
            context,
        ))
    }

    pub fn check_error(&self) -> Result<(), String> {
        if self.shared.is_abandoned() {
            Err("the Vulkan device is unavailable".into())
        } else {
            Ok(())
        }
    }

    pub fn wait_idle(&self) -> Result<(), String> {
        self.shared.wait_idle()
    }
}

impl Window {
    pub fn new(
        graphics: &Graphics,
        wayland: &WlSurface,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        validate_dimensions(width, height)?;
        let surface = create_wayland_surface(
            &graphics.shared.wayland_loader,
            &graphics.connection,
            wayland,
        )?;
        let mut window = Self {
            generation: None,
            surface: Some(surface),
            requested: vk::Extent2D { width, height },
            needs_reconfigure: true,
            surface_lost: false,
            shared: graphics.shared.clone(),
            wayland: wayland.clone(),
            connection: graphics.connection.clone(),
        };
        window.ensure_configured(width, height)?;
        Ok(window)
    }

    fn ensure_configured(&mut self, width: u32, height: u32) -> Result<(), String> {
        validate_dimensions(width, height)?;
        let requested = vk::Extent2D { width, height };
        if self.surface_lost {
            self.recreate_surface()?;
        }
        if self.needs_reconfigure || self.requested != requested || self.generation.is_none() {
            self.shared.wait_idle()?;
            let surface = self.surface.ok_or("the Vulkan surface is unavailable")?;
            let old_swapchain = self
                .generation
                .as_ref()
                .map_or(vk::SwapchainKHR::null(), |generation| generation.handle);
            let replacement =
                SwapchainGeneration::new(self.shared.clone(), surface, requested, old_swapchain)?;
            self.generation = Some(replacement);
            if self.shared.is_abandoned() {
                return Err("the Vulkan device is unavailable".into());
            }
            self.requested = requested;
            self.needs_reconfigure = false;
        }
        Ok(())
    }

    fn requires_reconfigure(&self, width: u32, height: u32) -> bool {
        self.surface_lost
            || self.needs_reconfigure
            || self.requested != vk::Extent2D { width, height }
            || self.generation.is_none()
    }

    fn recreate_surface(&mut self) -> Result<(), String> {
        self.shared.wait_idle()?;
        self.generation.take();
        if self.shared.is_abandoned() {
            return Err("the Vulkan device is unavailable".into());
        }
        if let Some(surface) = self.surface.take() {
            // SAFETY: no swapchain refers to this surface after generation was dropped.
            unsafe { self.shared.surface_loader.destroy_surface(surface, None) };
        }
        self.surface = Some(create_wayland_surface(
            &self.shared.wayland_loader,
            &self.connection,
            &self.wayland,
        )?);
        self.surface_lost = false;
        self.needs_reconfigure = true;
        Ok(())
    }
}

impl SwapchainGeneration {
    fn new(
        shared: Arc<DeviceState>,
        surface: vk::SurfaceKHR,
        requested: vk::Extent2D,
        old_swapchain: vk::SwapchainKHR,
    ) -> Result<Self, String> {
        // SAFETY: the physical device and surface belong to shared.instance.
        let capabilities = unsafe {
            shared
                .surface_loader
                .get_physical_device_surface_capabilities(shared.physical_device, surface)
        }
        .map_err(|error| format!("cannot query Vulkan surface capabilities: {error}"))?;
        // SAFETY: the physical device and surface belong to shared.instance.
        let formats = unsafe {
            shared
                .surface_loader
                .get_physical_device_surface_formats(shared.physical_device, surface)
        }
        .map_err(|error| format!("cannot query Vulkan surface formats: {error}"))?;
        // SAFETY: the physical device and surface belong to shared.instance.
        let present_modes = unsafe {
            shared
                .surface_loader
                .get_physical_device_surface_present_modes(shared.physical_device, surface)
        }
        .map_err(|error| format!("cannot query Vulkan present modes: {error}"))?;
        let plan = choose_surface(&capabilities, &formats, &present_modes, requested)?;
        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(plan.image_count)
            .image_format(plan.format)
            .image_color_space(plan.color_space)
            .image_extent(plan.extent)
            .image_array_layers(1)
            .image_usage(required_image_usage())
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(plan.transform)
            .composite_alpha(plan.alpha)
            .present_mode(plan.present_mode)
            .old_swapchain(old_swapchain)
            .clipped(true);
        // SAFETY: every field was selected from the queried surface capabilities.
        let handle = unsafe { shared.swapchain_loader.create_swapchain(&create_info, None) }
            .map_err(|error| format!("cannot create the Vulkan swapchain: {error}"))?;
        let mut result = Self {
            shared,
            handle,
            extent: plan.extent,
            format: plan.format,
            targets: Vec::new(),
            present_fences: Vec::new(),
            acquire_fences: Vec::new(),
        };
        // SAFETY: handle was created by this loader and remains live in result.
        let images = unsafe { result.shared.swapchain_loader.get_swapchain_images(handle) }
            .map_err(|error| format!("cannot enumerate Vulkan swapchain images: {error}"))?;
        result.targets.reserve(images.len());
        for image in images {
            let semaphore = result.shared.create_semaphore().map_err(|error| {
                format!("cannot create a Vulkan presentation semaphore: {error}")
            })?;
            let present_complete = match result.shared.create_fence() {
                Ok(fence) => fence,
                Err(error) => {
                    result.shared.destroy_semaphore(semaphore);
                    return Err(format!(
                        "cannot create a Vulkan presentation fence: {error}"
                    ));
                }
            };
            let acquire_complete = match result.shared.create_fence() {
                Ok(fence) => fence,
                Err(error) => {
                    result.shared.destroy_fence(present_complete);
                    result.shared.destroy_semaphore(semaphore);
                    return Err(format!("cannot create a Vulkan acquisition fence: {error}"));
                }
            };
            result.targets.push(FrameTarget {
                image,
                render_complete: semaphore,
                initialized: false,
            });
            result.present_fences.push(PresentFence {
                handle: present_complete,
                pending: false,
            });
            result.acquire_fences.push(AcquireFence {
                handle: acquire_complete,
                pending: false,
                retained: None,
            });
        }
        Ok(result)
    }

    fn available_present_fence(&mut self) -> Result<Option<(usize, vk::Fence)>, String> {
        for fence in &mut self.present_fences {
            if !fence.pending {
                continue;
            }
            // SAFETY: every pending fence belongs to this device and remains
            // live while its status is queried.
            let complete = unsafe { self.shared.device.get_fence_status(fence.handle) }
                .map_err(|error| format!("cannot query Vulkan presentation completion: {error}"))?;
            if !complete {
                continue;
            }
            // SAFETY: this fence's presentation operation has completed.
            unsafe {
                self.shared
                    .device
                    .reset_fences(std::slice::from_ref(&fence.handle))
            }
            .map_err(|error| format!("cannot reset a Vulkan presentation fence: {error}"))?;
            fence.pending = false;
        }
        Ok(self
            .present_fences
            .iter()
            .enumerate()
            .find(|(_, fence)| !fence.pending)
            .map(|(index, fence)| (index, fence.handle)))
    }

    fn available_acquire_fence(&mut self) -> Result<Option<(usize, vk::Fence)>, String> {
        for fence in &mut self.acquire_fences {
            if !fence.pending || fence.retained.is_some() {
                continue;
            }
            // SAFETY: every pending fence belongs to this device and remains
            // live while its status is queried.
            let complete = unsafe { self.shared.device.get_fence_status(fence.handle) }
                .map_err(|error| format!("cannot query Vulkan acquisition completion: {error}"))?;
            if !complete {
                continue;
            }
            // SAFETY: the acquisition associated with this fence has completed.
            unsafe {
                self.shared
                    .device
                    .reset_fences(std::slice::from_ref(&fence.handle))
            }
            .map_err(|error| format!("cannot reset a Vulkan acquisition fence: {error}"))?;
            fence.pending = false;
        }
        Ok(self
            .acquire_fences
            .iter()
            .enumerate()
            .find(|(_, fence)| !fence.pending && fence.retained.is_none())
            .map(|(index, fence)| (index, fence.handle)))
    }

    fn discard_acquire(
        &mut self,
        slot: usize,
        semaphore: vk::Semaphore,
        index: Option<u32>,
    ) -> Result<(), String> {
        let fence = self
            .acquire_fences
            .get_mut(slot)
            .ok_or("the Vulkan acquisition fence slot is unavailable")?;
        fence.retained = Some(RetainedAcquire { semaphore, index });
        // SAFETY: this fence was passed to the successful acquire operation
        // and remains owned by this slot.
        if let Err(error) = unsafe {
            self.shared
                .device
                .wait_for_fences(std::slice::from_ref(&fence.handle), true, u64::MAX)
        } {
            self.shared.abandon();
            return Err(format!("cannot finish Vulkan acquisition cleanup: {error}"));
        }
        let retained = fence
            .retained
            .take()
            .expect("the acquisition cleanup retains its semaphore");
        self.shared.destroy_semaphore(retained.semaphore);
        let release = retained
            .index
            .map(|index| self.shared.release_image(self.handle, index))
            .transpose();
        // SAFETY: the acquisition fence is signaled and no longer in use.
        let reset = unsafe {
            self.shared
                .device
                .reset_fences(std::slice::from_ref(&fence.handle))
        }
        .map_err(|error| format!("cannot reset a Vulkan acquisition fence: {error}"));
        if reset.is_ok() {
            fence.pending = false;
        }
        release.and(reset)
    }
}

impl Drop for SwapchainGeneration {
    fn drop(&mut self) {
        if self.shared.is_abandoned() {
            return;
        }
        let mut pending: Vec<_> = self
            .present_fences
            .iter()
            .filter(|fence| fence.pending)
            .map(|fence| fence.handle)
            .collect();
        pending.extend(
            self.acquire_fences
                .iter()
                .filter(|fence| fence.pending)
                .map(|fence| fence.handle),
        );
        if !pending.is_empty() {
            // SAFETY: every pending fence belongs to this device and remains
            // live until the wait and resource destruction below complete.
            if unsafe { self.shared.device.wait_for_fences(&pending, true, u64::MAX) }.is_err() {
                self.shared.abandon();
                return;
            }
        }
        if self.shared.wait_idle().is_err() {
            return;
        }
        for fence in &mut self.acquire_fences {
            if let Some(retained) = fence.retained.take() {
                self.shared.destroy_semaphore(retained.semaphore);
                if let Some(index) = retained.index {
                    let _ = self.shared.release_image(self.handle, index);
                }
            }
        }
        for target in self.targets.drain(..) {
            self.shared.destroy_semaphore(target.render_complete);
        }
        for fence in self.present_fences.drain(..) {
            self.shared.destroy_fence(fence.handle);
        }
        for fence in self.acquire_fences.drain(..) {
            self.shared.destroy_fence(fence.handle);
        }
        // SAFETY: all image wrappers and semaphores for this generation are gone.
        unsafe {
            self.shared
                .swapchain_loader
                .destroy_swapchain(self.handle, None);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.generation.take();
        if self.shared.is_abandoned() {
            return;
        }
        if let Some(surface) = self.surface.take() {
            // SAFETY: the swapchain generation was destroyed first.
            unsafe { self.shared.surface_loader.destroy_surface(surface, None) };
        }
    }
}

impl DeviceState {
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }

    fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    fn create_semaphore(&self) -> Result<vk::Semaphore, vk::Result> {
        // SAFETY: the device is live and the create info has no borrowed extensions.
        unsafe {
            self.device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        }
    }

    fn destroy_semaphore(&self, semaphore: vk::Semaphore) {
        // SAFETY: callers transfer one semaphore owned by this device and ensure
        // it is not in use before destruction.
        unsafe { self.device.destroy_semaphore(semaphore, None) };
    }

    fn create_fence(&self) -> Result<vk::Fence, vk::Result> {
        // SAFETY: the device is live and the create info has no borrowed extensions.
        unsafe {
            self.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
    }

    fn destroy_fence(&self, fence: vk::Fence) {
        // SAFETY: callers provide a fence owned by this device after its work
        // has completed or before it has been submitted.
        unsafe { self.device.destroy_fence(fence, None) };
    }

    fn release_image(&self, swapchain: vk::SwapchainKHR, index: u32) -> Result<(), String> {
        let indices = [index];
        let release = vk::ReleaseSwapchainImagesInfoEXT::default()
            .swapchain(swapchain)
            .image_indices(&indices);
        // SAFETY: the image index was returned by acquire_next_image for this
        // swapchain and has not been submitted for presentation.
        unsafe { self.maintenance_loader.release_swapchain_images(&release) }
            .map_err(|error| format!("cannot release the acquired Vulkan image: {error}"))
    }

    fn release_submitted_image(
        &self,
        swapchain: vk::SwapchainKHR,
        index: u32,
    ) -> Result<(), String> {
        self.wait_idle()?;
        self.release_image(swapchain, index)
    }

    fn wait_idle(&self) -> Result<(), String> {
        if self.is_abandoned() {
            return Err("the Vulkan device is unavailable".into());
        }
        // SAFETY: the device remains live for the duration of the call. This is
        // used only for configuration changes and teardown, never per frame.
        unsafe { self.device.device_wait_idle() }.map_err(|error| {
            self.abandon();
            format!("cannot idle the Vulkan device: {error}")
        })
    }
}

impl Drop for DeviceState {
    fn drop(&mut self) {
        if self.abandoned.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: this is the final shared owner, so no new submissions can begin.
        if unsafe { self.device.device_wait_idle() }.is_err() {
            return;
        }
        // SAFETY: DeviceState is the final owner after all windows and the Skia
        // DirectContext have been released by Render.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
            ManuallyDrop::drop(&mut self.entry);
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.take() {
            // SAFETY: the guard owns this partially initialized instance.
            unsafe { instance.destroy_instance(None) };
        }
    }
}

impl Drop for SurfaceGuard {
    fn drop(&mut self) {
        // SAFETY: the guard exclusively owns this temporary probe surface.
        unsafe { self.loader.destroy_surface(self.handle, None) };
    }
}

pub fn draw(
    graphics: &mut Graphics,
    painter: &mut Painter,
    window: &mut Window,
    frame: PaintFrame<'_>,
    before_present: impl FnOnce(),
) -> Result<FrameResult, String> {
    graphics.check_error()?;
    if window.requires_reconfigure(frame.physical.0, frame.physical.1)
        && window.generation.is_some()
    {
        graphics.wait_idle()?;
        painter.context_mut().free_gpu_resources();
    }
    window.ensure_configured(frame.physical.0, frame.physical.1)?;
    let generation = window
        .generation
        .as_mut()
        .ok_or("the Vulkan swapchain is unavailable")?;
    let Some((present_slot, present_fence)) = generation.available_present_fence()? else {
        return Ok(FrameResult::RetryAfter(RETRY_DELAY));
    };
    let Some((acquire_slot, acquire_fence)) = generation.available_acquire_fence()? else {
        return Ok(FrameResult::RetryAfter(RETRY_DELAY));
    };
    let acquire = generation
        .shared
        .create_semaphore()
        .map_err(|error| format!("cannot create a Vulkan acquire semaphore: {error}"))?;
    // SAFETY: the swapchain and semaphore belong to the same live device. A
    // zero timeout keeps acquisition from blocking the event thread.
    let acquired = unsafe {
        generation.shared.swapchain_loader.acquire_next_image(
            generation.handle,
            ACQUIRE_TIMEOUT_NS,
            acquire,
            acquire_fence,
        )
    };
    let (index, acquire_suboptimal) = match acquired {
        Ok(result) => {
            generation.acquire_fences[acquire_slot].pending = true;
            result
        }
        Err(error) => {
            generation.shared.destroy_semaphore(acquire);
            match acquire_recovery(error) {
                Recovery::Retry => return Ok(FrameResult::RetryAfter(RETRY_DELAY)),
                Recovery::Reconfigure => {
                    window.needs_reconfigure = true;
                    return Ok(FrameResult::RetryAfter(RETRY_DELAY));
                }
                Recovery::RecreateSurface => {
                    window.surface_lost = true;
                    return Ok(FrameResult::RetryAfter(RETRY_DELAY));
                }
                Recovery::Fatal(error) => {
                    return Err(format!("cannot acquire a Vulkan swapchain image: {error}"));
                }
            }
        }
    };
    let target_index = index as usize;
    if target_index >= generation.targets.len() {
        let cleanup = generation.discard_acquire(acquire_slot, acquire, None);
        let detail = cleanup
            .err()
            .map_or(String::new(), |error| format!("; {error}"));
        return Err(format!(
            "Vulkan returned an invalid swapchain image index{detail}"
        ));
    }
    let mut surface = match wrap_surface(
        painter,
        &generation.targets[target_index],
        generation.extent,
        generation.format,
        generation.shared.queue_family,
    ) {
        Ok(surface) => surface,
        Err(error) => {
            let cleanup = generation.discard_acquire(acquire_slot, acquire, Some(index));
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; {cleanup}"),
            });
        }
    };
    // SAFETY: acquire is a live Vulkan semaphore owned by the ash device.
    let wait = unsafe { gpu::backend_semaphores::make_vk(acquire.as_raw() as skia_vk::Semaphore) };
    if !surface.wait(std::slice::from_ref(&wait), true) {
        generation.discard_acquire(acquire_slot, acquire, Some(index))?;
        return Err("Skia rejected the Vulkan acquire semaphore".into());
    }

    let drawn = painter.draw_surface(&mut surface, frame);
    // SAFETY: render_complete remains live through flush, submit, and present.
    let mut signals = [unsafe {
        gpu::backend_semaphores::make_vk(
            generation.targets[target_index].render_complete.as_raw() as skia_vk::Semaphore
        )
    }];
    let mut flush_info = FlushInfo::default();
    // SAFETY: signals remains borrowed until the flush call below returns.
    unsafe {
        flush_info.set_signal_semaphores(&mut signals);
    }
    let present_state = skia_vk::mutable_texture_states::new_vulkan(
        skia_vk::ImageLayout::PRESENT_SRC_KHR,
        generation.shared.queue_family,
    );
    let flushed = painter.context_mut().flush_surface_with_texture_state(
        &mut surface,
        &flush_info,
        Some(&present_state),
    );
    let submitted = painter.context_mut().submit(gpu::SubmitInfo {
        sync: gpu::SyncCpu::No,
        ..gpu::SubmitInfo::default()
    });
    drop(surface);
    painter.release_frame_resources();

    match flushed {
        Ok(SemaphoresSubmitted::Yes) if submitted => {}
        Ok(_) => {
            generation.shared.abandon();
            painter.abandon();
            return Err("Skia did not submit the Vulkan presentation semaphore".into());
        }
        Err(error) => {
            generation.shared.abandon();
            painter.abandon();
            return Err(format!("cannot flush the Skia Vulkan frame: {error}"));
        }
    }
    generation.targets[target_index].initialized = true;
    if let Err(error) = drawn {
        let release = generation
            .shared
            .release_submitted_image(generation.handle, index);
        return Err(match release {
            Ok(()) => error,
            Err(release) => format!("{error}; {release}"),
        });
    }
    drop(signals);
    before_present();
    let waits = [generation.targets[target_index].render_complete];
    let swapchains = [generation.handle];
    let indices = [index];
    let fences = [present_fence];
    let mut fence_info = vk::SwapchainPresentFenceInfoEXT::default().fences(&fences);
    let present_info = vk::PresentInfoKHR::default()
        .wait_semaphores(&waits)
        .swapchains(&swapchains)
        .image_indices(&indices)
        .push_next(&mut fence_info);
    // SAFETY: the queue, swapchain, semaphore, and image index all belong to
    // this live generation, and the arrays outlive the call.
    let present = unsafe {
        generation
            .shared
            .swapchain_loader
            .queue_present(generation.shared.queue, &present_info)
    };
    let present_suboptimal = match classify_present(present) {
        PresentOutcome::Queued { suboptimal } => {
            generation.present_fences[present_slot].pending = true;
            suboptimal
        }
        PresentOutcome::Rejected(recovery) => {
            generation.present_fences[present_slot].pending = true;
            match recovery {
                Recovery::Reconfigure => window.needs_reconfigure = true,
                Recovery::RecreateSurface => window.surface_lost = true,
                Recovery::Retry | Recovery::Fatal(_) => {
                    unreachable!("presentation rejection must schedule WSI recovery")
                }
            }
            return Ok(FrameResult::RetryAfter(RETRY_DELAY));
        }
        PresentOutcome::Failed(error) => {
            generation
                .shared
                .release_submitted_image(generation.handle, index)?;
            return Err(format!("cannot present the Vulkan frame: {error}"));
        }
    };
    let redraw = acquire_suboptimal || present_suboptimal;
    if redraw {
        window.needs_reconfigure = true;
    }
    Ok(FrameResult::Presented { redraw })
}

fn acquire_recovery(error: vk::Result) -> Recovery {
    match error {
        vk::Result::TIMEOUT | vk::Result::NOT_READY => Recovery::Retry,
        vk::Result::ERROR_OUT_OF_DATE_KHR => Recovery::Reconfigure,
        vk::Result::ERROR_SURFACE_LOST_KHR => Recovery::RecreateSurface,
        error => Recovery::Fatal(error),
    }
}

fn present_recovery(error: vk::Result) -> Recovery {
    match error {
        vk::Result::ERROR_OUT_OF_DATE_KHR => Recovery::Reconfigure,
        vk::Result::ERROR_SURFACE_LOST_KHR => Recovery::RecreateSurface,
        error => Recovery::Fatal(error),
    }
}

fn classify_present(result: Result<bool, vk::Result>) -> PresentOutcome {
    match result {
        Ok(suboptimal) => PresentOutcome::Queued { suboptimal },
        Err(error @ (vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::ERROR_SURFACE_LOST_KHR)) => {
            PresentOutcome::Rejected(present_recovery(error))
        }
        Err(error) => PresentOutcome::Failed(error),
    }
}

fn wrap_surface(
    painter: &mut Painter,
    target: &FrameTarget,
    extent: vk::Extent2D,
    format: vk::Format,
    queue_family: u32,
) -> Result<Surface, String> {
    let (skia_format, color_type) = skia_format(format)?;
    let layout = if target.initialized {
        skia_vk::ImageLayout::PRESENT_SRC_KHR
    } else {
        skia_vk::ImageLayout::UNDEFINED
    };
    // SAFETY: the swapchain image outlives the temporary backend render target
    // and Surface created from this ImageInfo.
    let mut image_info = unsafe {
        skia_vk::ImageInfo::new(
            target.image.as_raw() as skia_vk::Image,
            skia_vk::Alloc::default(),
            skia_vk::ImageTiling::OPTIMAL,
            layout,
            skia_format,
            1,
            Some(queue_family),
            None,
            None,
            Some(skia_vk::SharingMode::EXCLUSIVE),
        )
    };
    image_info.image_usage_flags = required_image_usage().as_raw();
    let width = i32::try_from(extent.width).map_err(|error| error.to_string())?;
    let height = i32::try_from(extent.height).map_err(|error| error.to_string())?;
    let target = backend_render_targets::make_vk((width, height), &image_info);
    surfaces::wrap_backend_render_target(
        painter.context_mut(),
        &target,
        SurfaceOrigin::TopLeft,
        color_type,
        None,
        None,
    )
    .ok_or("Skia cannot wrap the Vulkan swapchain image".into())
}

fn create_skia_context(shared: &Arc<DeviceState>) -> Result<gpu::DirectContext, String> {
    let get_proc = |request: skia_vk::GetProcOf| {
        // SAFETY: Skia supplies handles from the live backend context and a
        // null-terminated function name for the duration of this lookup.
        let function = unsafe {
            match request {
                skia_vk::GetProcOf::Instance(instance, name) => shared
                    .entry
                    .get_instance_proc_addr(vk::Instance::from_raw(instance as _), name),
                skia_vk::GetProcOf::Device(device, name) => shared
                    .instance
                    .get_device_proc_addr(vk::Device::from_raw(device as _), name),
            }
        };
        function.map_or(ptr::null(), |function| {
            function as *const () as *const c_void
        })
    };
    // SAFETY: every raw handle comes from shared and outlives the returned
    // DirectContext. The enabled extension lists match ash initialization.
    let backend = unsafe {
        skia_vk::BackendContext::new_builder(
            shared.instance.handle().as_raw() as _,
            shared.physical_device.as_raw() as _,
            shared.device.handle().as_raw() as _,
            (shared.queue.as_raw() as _, shared.queue_family as usize),
            &get_proc,
            Some(skia_vk::Version::new(1, 1, 0)),
        )
        .with_extensions(
            &[
                "VK_KHR_surface",
                "VK_KHR_wayland_surface",
                "VK_KHR_get_surface_capabilities2",
                "VK_EXT_surface_maintenance1",
            ],
            &["VK_KHR_swapchain", "VK_EXT_swapchain_maintenance1"],
        )
        .build()
    };
    direct_contexts::make_vulkan(&backend, None)
        .ok_or("cannot create the Skia Vulkan context".into())
}

#[derive(Clone, Copy)]
struct AdapterChoice {
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    score: u8,
}

fn choose_adapter(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    wayland_loader: &ash::khr::wayland_surface::Instance,
    surface: vk::SurfaceKHR,
    display: *mut vk::wl_display,
) -> Result<AdapterChoice, String> {
    // SAFETY: instance is live and owns every returned physical-device handle.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|error| format!("cannot enumerate Vulkan devices: {error}"))?;
    let mut best = None;
    for physical_device in devices {
        // SAFETY: physical_device was returned by this instance.
        let properties = unsafe { instance.get_physical_device_properties(physical_device) };
        if properties.api_version < API_VERSION
            || !supports_device_extension(instance, physical_device, ash::khr::swapchain::NAME)?
            || !supports_device_extension(
                instance,
                physical_device,
                ash::ext::swapchain_maintenance1::NAME,
            )?
            || !supports_swapchain_maintenance(instance, physical_device)
        {
            continue;
        }
        if !supports_surface_contract(surface_loader, physical_device, surface) {
            continue;
        }
        // SAFETY: physical_device was returned by this instance.
        let queues =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        for (index, queue) in queues.iter().enumerate() {
            let index = index as u32;
            if queue.queue_count == 0 || !queue.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                continue;
            }
            // SAFETY: every handle belongs to the same live instance.
            let surface_support = unsafe {
                surface_loader.get_physical_device_surface_support(physical_device, index, surface)
            }
            .map_err(|error| format!("cannot query Vulkan surface support: {error}"))?;
            // SAFETY: display is the live connection pointer retained by the
            // caller, and the function pointer came from this instance.
            let wayland_support = unsafe {
                (wayland_loader
                    .fp()
                    .get_physical_device_wayland_presentation_support_khr)(
                    physical_device,
                    index,
                    display,
                ) > 0
            };
            if !surface_support || !wayland_support {
                continue;
            }
            let score = match properties.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 5,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 4,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 3,
                vk::PhysicalDeviceType::CPU => 2,
                _ => 1,
            };
            let candidate = AdapterChoice {
                physical_device,
                queue_family: index,
                score,
            };
            if best.is_none_or(|choice: AdapterChoice| candidate.score > choice.score) {
                best = Some(candidate);
            }
        }
    }
    best.ok_or(
        "cannot find a Vulkan 1.1 device with Wayland presentation and swapchain maintenance support"
            .into(),
    )
}

fn supports_surface_contract(
    loader: &ash::khr::surface::Instance,
    physical_device: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> bool {
    // SAFETY: the physical device and surface belong to loader.instance().
    let capabilities =
        unsafe { loader.get_physical_device_surface_capabilities(physical_device, surface) };
    // SAFETY: the physical device and surface belong to loader.instance().
    let formats = unsafe { loader.get_physical_device_surface_formats(physical_device, surface) };
    // SAFETY: the physical device and surface belong to loader.instance().
    let modes =
        unsafe { loader.get_physical_device_surface_present_modes(physical_device, surface) };
    match (capabilities, formats, modes) {
        (Ok(capabilities), Ok(formats), Ok(modes)) => choose_surface(
            &capabilities,
            &formats,
            &modes,
            vk::Extent2D {
                width: 1,
                height: 1,
            },
        )
        .is_ok(),
        _ => false,
    }
}

fn supports_device_extension(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    expected: &CStr,
) -> Result<bool, String> {
    // SAFETY: physical_device was returned by this instance.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .map_err(|error| format!("cannot enumerate Vulkan device extensions: {error}"))?;
    Ok(extensions.iter().any(|extension| {
        // SAFETY: Vulkan guarantees a null-terminated extension_name array.
        let name = unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) };
        name == expected
    }))
}

fn supports_swapchain_maintenance(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> bool {
    let mut maintenance = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default();
    let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut maintenance);
    // SAFETY: physical_device was returned by this instance, and every pNext
    // structure remains live for the query.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features) };
    maintenance.swapchain_maintenance1 == vk::TRUE
}

fn ensure_instance_extensions(entry: &Entry) -> Result<(), String> {
    // SAFETY: the loaded entry remains live for this loader query.
    let extensions = unsafe { entry.enumerate_instance_extension_properties(None) }
        .map_err(|error| format!("cannot enumerate Vulkan instance extensions: {error}"))?;
    for expected in [
        ash::khr::surface::NAME,
        ash::khr::wayland_surface::NAME,
        ash::khr::get_surface_capabilities2::NAME,
        ash::ext::surface_maintenance1::NAME,
    ] {
        if !extensions.iter().any(|extension| {
            // SAFETY: Vulkan guarantees a null-terminated extension_name array.
            let name = unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) };
            name == expected
        }) {
            return Err(format!(
                "the Vulkan loader does not expose {}",
                expected.to_string_lossy()
            ));
        }
    }
    Ok(())
}

fn create_wayland_surface(
    loader: &ash::khr::wayland_surface::Instance,
    connection: &Connection,
    surface: &WlSurface,
) -> Result<vk::SurfaceKHR, String> {
    let display = display_ptr(connection)?;
    let wayland_surface = surface.id().as_ptr().cast::<vk::wl_surface>();
    if wayland_surface.is_null() {
        return Err("the Wayland surface has been destroyed".into());
    }
    let create_info = vk::WaylandSurfaceCreateInfoKHR::default()
        .display(display)
        .surface(wayland_surface);
    // SAFETY: the connection and WlSurface are retained by the caller for the
    // lifetime of the returned VkSurfaceKHR.
    unsafe { loader.create_wayland_surface(&create_info, None) }
        .map_err(|error| format!("cannot create a Vulkan Wayland surface: {error}"))
}

fn display_ptr(connection: &Connection) -> Result<*mut vk::wl_display, String> {
    let display = connection.backend().display_ptr().cast::<vk::wl_display>();
    (!display.is_null())
        .then_some(display)
        .ok_or("the Wayland display is unavailable".into())
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        Err("Vulkan surface dimensions must be positive".into())
    } else if width > i32::MAX as u32 || height > i32::MAX as u32 {
        Err("Vulkan surface dimensions exceed renderer limits".into())
    } else {
        Ok(())
    }
}

fn choose_surface(
    capabilities: &vk::SurfaceCapabilitiesKHR,
    formats: &[vk::SurfaceFormatKHR],
    present_modes: &[vk::PresentModeKHR],
    requested: vk::Extent2D,
) -> Result<SurfacePlan, String> {
    if !capabilities
        .supported_usage_flags
        .contains(required_image_usage())
    {
        return Err(
            "Vulkan surface does not support Skia's color-attachment and transfer usage".into(),
        );
    }
    let format = formats
        .iter()
        .copied()
        .find(|format| {
            matches!(
                format.format,
                vk::Format::R8G8B8A8_UNORM | vk::Format::B8G8R8A8_UNORM
            ) && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| {
            formats
                .iter()
                .find(|format| {
                    format.format == vk::Format::UNDEFINED
                        && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
                .map(|format| vk::SurfaceFormatKHR {
                    format: vk::Format::B8G8R8A8_UNORM,
                    color_space: format.color_space,
                })
        })
        .ok_or("Vulkan surface does not expose a linear 8-bit RGBA or BGRA format")?;
    let alpha = capabilities
        .supported_composite_alpha
        .contains(vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED)
        .then_some(vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED)
        .ok_or("Vulkan surface does not support premultiplied alpha")?;
    let present_mode = if present_modes.contains(&vk::PresentModeKHR::FIFO) {
        vk::PresentModeKHR::FIFO
    } else if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        return Err("Vulkan surface does not support fifo or mailbox presentation".into());
    };
    let extent = if capabilities.current_extent.width != u32::MAX {
        capabilities.current_extent
    } else {
        vk::Extent2D {
            width: requested.width.clamp(
                capabilities.min_image_extent.width,
                capabilities.max_image_extent.width,
            ),
            height: requested.height.clamp(
                capabilities.min_image_extent.height,
                capabilities.max_image_extent.height,
            ),
        }
    };
    validate_dimensions(extent.width, extent.height)?;
    let mut image_count = capabilities.min_image_count.saturating_add(1).max(2);
    if capabilities.max_image_count != 0 {
        image_count = image_count.min(capabilities.max_image_count);
    }
    Ok(SurfacePlan {
        format: format.format,
        color_space: format.color_space,
        present_mode,
        alpha,
        transform: capabilities.current_transform,
        extent,
        image_count,
    })
}

fn skia_format(format: vk::Format) -> Result<(skia_vk::Format, ColorType), String> {
    match format {
        vk::Format::R8G8B8A8_UNORM => Ok((skia_vk::Format::R8G8B8A8_UNORM, ColorType::RGBA8888)),
        vk::Format::B8G8R8A8_UNORM => Ok((skia_vk::Format::B8G8R8A8_UNORM, ColorType::BGRA8888)),
        _ => Err(format!("unsupported Vulkan swapchain format {format:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> vk::SurfaceCapabilitiesKHR {
        vk::SurfaceCapabilitiesKHR {
            min_image_count: 2,
            max_image_count: 3,
            current_extent: vk::Extent2D {
                width: u32::MAX,
                height: u32::MAX,
            },
            min_image_extent: vk::Extent2D {
                width: 16,
                height: 16,
            },
            max_image_extent: vk::Extent2D {
                width: 4096,
                height: 4096,
            },
            current_transform: vk::SurfaceTransformFlagsKHR::IDENTITY,
            supported_composite_alpha: vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            supported_usage_flags: required_image_usage(),
            ..vk::SurfaceCapabilitiesKHR::default()
        }
    }

    fn formats() -> [vk::SurfaceFormatKHR; 2] {
        [
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::R8G8B8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ]
    }

    #[test]
    fn surface_selection_preserves_fifo_linear_color_and_premultiplied_alpha() {
        let plan = choose_surface(
            &capabilities(),
            &formats(),
            &[vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO],
            vk::Extent2D {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();
        assert_eq!(plan.format, vk::Format::R8G8B8A8_UNORM);
        assert_eq!(plan.present_mode, vk::PresentModeKHR::FIFO);
        assert_eq!(plan.alpha, vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED);
        assert_eq!(
            plan.extent,
            vk::Extent2D {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(plan.image_count, 3);
    }

    #[test]
    fn surface_selection_clamps_extent_and_falls_back_to_mailbox() {
        let plan = choose_surface(
            &capabilities(),
            &formats(),
            &[vk::PresentModeKHR::MAILBOX],
            vk::Extent2D {
                width: 8,
                height: 8192,
            },
        )
        .unwrap();
        assert_eq!(plan.present_mode, vk::PresentModeKHR::MAILBOX);
        assert_eq!(
            plan.extent,
            vk::Extent2D {
                width: 16,
                height: 4096
            }
        );
    }

    #[test]
    fn undefined_surface_format_selects_linear_bgra() {
        let formats = [vk::SurfaceFormatKHR {
            format: vk::Format::UNDEFINED,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        let plan = choose_surface(
            &capabilities(),
            &formats,
            &[vk::PresentModeKHR::FIFO],
            vk::Extent2D {
                width: 100,
                height: 100,
            },
        )
        .unwrap();
        assert_eq!(plan.format, vk::Format::B8G8R8A8_UNORM);
    }

    #[test]
    fn surface_selection_rejects_incompatible_contracts() {
        let mut incompatible = capabilities();
        incompatible.supported_composite_alpha = vk::CompositeAlphaFlagsKHR::OPAQUE;
        assert!(
            choose_surface(
                &incompatible,
                &formats(),
                &[vk::PresentModeKHR::FIFO],
                vk::Extent2D {
                    width: 100,
                    height: 100
                }
            )
            .unwrap_err()
            .contains("premultiplied")
        );

        let mut incompatible = capabilities();
        incompatible.supported_usage_flags = vk::ImageUsageFlags::COLOR_ATTACHMENT;
        assert!(
            choose_surface(
                &incompatible,
                &formats(),
                &[vk::PresentModeKHR::FIFO],
                vk::Extent2D {
                    width: 100,
                    height: 100
                }
            )
            .unwrap_err()
            .contains("transfer")
        );

        let srgb = [vk::SurfaceFormatKHR {
            format: vk::Format::B8G8R8A8_SRGB,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        assert!(
            choose_surface(
                &capabilities(),
                &srgb,
                &[vk::PresentModeKHR::FIFO],
                vk::Extent2D {
                    width: 100,
                    height: 100
                }
            )
            .unwrap_err()
            .contains("linear")
        );
    }

    #[test]
    fn acquisition_and_presentation_recovery_stay_bounded() {
        assert_eq!(ACQUIRE_TIMEOUT_NS, 0);
        assert_eq!(acquire_recovery(vk::Result::TIMEOUT), Recovery::Retry);
        assert_eq!(acquire_recovery(vk::Result::NOT_READY), Recovery::Retry);
        assert_eq!(
            acquire_recovery(vk::Result::ERROR_OUT_OF_DATE_KHR),
            Recovery::Reconfigure
        );
        assert_eq!(
            acquire_recovery(vk::Result::ERROR_SURFACE_LOST_KHR),
            Recovery::RecreateSurface
        );
        assert_eq!(
            present_recovery(vk::Result::ERROR_OUT_OF_DATE_KHR),
            Recovery::Reconfigure
        );
        assert_eq!(
            present_recovery(vk::Result::ERROR_DEVICE_LOST),
            Recovery::Fatal(vk::Result::ERROR_DEVICE_LOST)
        );
        assert_eq!(
            classify_present(Ok(false)),
            PresentOutcome::Queued { suboptimal: false }
        );
        assert_eq!(
            classify_present(Err(vk::Result::ERROR_OUT_OF_DATE_KHR)),
            PresentOutcome::Rejected(Recovery::Reconfigure)
        );
        assert_eq!(
            classify_present(Err(vk::Result::ERROR_DEVICE_LOST)),
            PresentOutcome::Failed(vk::Result::ERROR_DEVICE_LOST)
        );
    }
}
