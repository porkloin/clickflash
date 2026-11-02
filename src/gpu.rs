//! The Vulkan side: each frame clears a swapchain image to black, or to white for
//! a flash, and presents it. No shaders: a clear is all a flash needs.

use std::sync::Arc;

use ash::{khr, vk};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use crate::{Options, now_us};

const FRAMES_IN_FLIGHT: usize = 2;

const COLOR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
	aspect_mask: vk::ImageAspectFlags::COLOR,
	base_mip_level: 0,
	level_count: 1,
	base_array_layer: 0,
	layer_count: 1,
};

struct Frame {
	command_buffer: vk::CommandBuffer,
	fence: vk::Fence,
	acquired: vk::Semaphore,
}

pub struct Gpu {
	window: Arc<Window>,
	/// Keeps libvulkan loaded.
	_entry: ash::Entry,
	instance: ash::Instance,
	surface_fn: khr::surface::Instance,
	surface: vk::SurfaceKHR,
	physical: vk::PhysicalDevice,
	device_name: String,
	device: ash::Device,
	queue: vk::Queue,
	swapchain_fn: khr::swapchain::Device,
	pool: vk::CommandPool,
	frames: Vec<Frame>,
	frame: usize,
	swapchain: vk::SwapchainKHR,
	images: Vec<vk::Image>,
	views: Vec<vk::ImageView>,
	/// One per swapchain image: presentation waits on the image's own.
	rendered: Vec<vk::Semaphore>,
	extent: vk::Extent2D,
	vsync: bool,
	/// The swapchain needs recreating before the next frame.
	stale: bool,
}

fn vk_err(what: &str) -> impl Fn(vk::Result) -> String + '_ {
	move |e| format!("{what}: {e}")
}

fn load_vulkan() -> Result<ash::Entry, String> {
	// SAFETY: loads the system's Vulkan loader, which stays loaded as long as
	// the returned entry.
	unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))
}

impl Gpu {
	pub fn new(window: Arc<Window>, options: &Options) -> Result<Self, String> {
		let display = window
			.display_handle()
			.map_err(|e| format!("display handle: {e}"))?
			.as_raw();
		let handle = window
			.window_handle()
			.map_err(|e| format!("window handle: {e}"))?
			.as_raw();

		// SAFETY: every call gets valid handles created here. If one fails
		// partway, what was made leaks; the caller exits.
		unsafe {
			let entry = load_vulkan()?;

			let extensions = ash_window::enumerate_required_extensions(display)
				.map_err(vk_err("surface extensions"))?
				.to_vec();
			let app = vk::ApplicationInfo::default()
				.application_name(c"clickflash")
				.api_version(vk::API_VERSION_1_3);
			let instance = entry
				.create_instance(
					&vk::InstanceCreateInfo::default()
						.application_info(&app)
						.enabled_extension_names(&extensions),
					None,
				)
				.map_err(vk_err("create instance"))?;

			let surface_fn = khr::surface::Instance::new(&entry, &instance);
			let surface = ash_window::create_surface(&entry, &instance, display, handle, None)
				.map_err(vk_err("create surface"))?;

			// A Vulkan 1.3 device with a queue that draws and presents; a
			// discrete GPU if there is one.
			let mut best = None;
			for physical in instance.enumerate_physical_devices().map_err(vk_err("devices"))? {
				let props = instance.get_physical_device_properties(physical);
				if props.api_version < vk::API_VERSION_1_3 {
					continue;
				}
				let families = instance.get_physical_device_queue_family_properties(physical);
				let family = (0..families.len() as u32).find(|&i| {
					families[i as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS)
						&& surface_fn
							.get_physical_device_surface_support(physical, i, surface)
							.unwrap_or(false)
				});
				if let Some(family) = family {
					let discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
					if best.is_none() || discrete {
						best = Some((physical, family, props));
					}
					if discrete {
						break;
					}
				}
			}
			let (physical, family, props) = best.ok_or("no Vulkan 1.3 device can draw to this window")?;
			let device_name = props
				.device_name_as_c_str()
				.unwrap_or(c"?")
				.to_string_lossy()
				.into_owned();

			let extensions = [khr::swapchain::NAME.as_ptr()];
			let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
				.dynamic_rendering(true)
				.synchronization2(true);
			let priorities = [1.0];
			let queues = [vk::DeviceQueueCreateInfo::default()
				.queue_family_index(family)
				.queue_priorities(&priorities)];
			let device = instance
				.create_device(
					physical,
					&vk::DeviceCreateInfo::default()
						.queue_create_infos(&queues)
						.enabled_extension_names(&extensions)
						.push_next(&mut features13),
					None,
				)
				.map_err(vk_err("create device"))?;
			let queue = device.get_device_queue(family, 0);
			let swapchain_fn = khr::swapchain::Device::new(&instance, &device);

			let pool = device
				.create_command_pool(
					&vk::CommandPoolCreateInfo::default()
						.queue_family_index(family)
						.flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
					None,
				)
				.map_err(vk_err("command pool"))?;
			let buffers = device
				.allocate_command_buffers(
					&vk::CommandBufferAllocateInfo::default()
						.command_pool(pool)
						.level(vk::CommandBufferLevel::PRIMARY)
						.command_buffer_count(FRAMES_IN_FLIGHT as u32),
				)
				.map_err(vk_err("command buffers"))?;
			let mut frames = Vec::new();
			for command_buffer in buffers {
				frames.push(Frame {
					command_buffer,
					fence: device
						.create_fence(
							&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
							None,
						)
						.map_err(vk_err("fence"))?,
					acquired: device
						.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
						.map_err(vk_err("semaphore"))?,
				});
			}

			Ok(Self {
				window,
				_entry: entry,
				instance,
				surface_fn,
				surface,
				physical,
				device_name,
				device,
				queue,
				swapchain_fn,
				pool,
				frames,
				frame: 0,
				swapchain: vk::SwapchainKHR::null(),
				images: Vec::new(),
				views: Vec::new(),
				rendered: Vec::new(),
				extent: vk::Extent2D::default(),
				vsync: options.vsync,
				stale: true,
			})
		}
	}

	pub fn device_name(&self) -> &str {
		&self.device_name
	}

	pub fn window(&self) -> &Window {
		&self.window
	}

	pub fn resized(&mut self) {
		self.stale = true;
	}

	fn pick_format(&self) -> Result<vk::SurfaceFormatKHR, String> {
		// SAFETY: valid physical device and surface.
		let offered = unsafe {
			self.surface_fn
				.get_physical_device_surface_formats(self.physical, self.surface)
		}
		.map_err(vk_err("surface formats"))?;
		// Black and white come out the same in any format; prefer plain 8-bit sRGB.
		offered
			.iter()
			.find(|o| o.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
			.or(offered.first())
			.copied()
			.ok_or_else(|| "the surface offers no formats".to_string())
	}

	fn create_swapchain(&mut self) -> Result<(), String> {
		let format = self.pick_format()?;
		// SAFETY: the device is idle before anything the old swapchain uses is
		// destroyed; all handles are this device's.
		unsafe {
			let caps = self
				.surface_fn
				.get_physical_device_surface_capabilities(self.physical, self.surface)
				.map_err(vk_err("surface capabilities"))?;
			let size = self.window.inner_size();
			let extent = if caps.current_extent.width != u32::MAX {
				caps.current_extent
			} else {
				vk::Extent2D {
					width: size
						.width
						.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
					height: size
						.height
						.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
				}
			};
			if extent.width == 0 || extent.height == 0 {
				// Minimised; try again on the next frame.
				self.stale = true;
				return Ok(());
			}
			let modes = self
				.surface_fn
				.get_physical_device_surface_present_modes(self.physical, self.surface)
				.map_err(vk_err("present modes"))?;
			let present_mode = if self.vsync {
				vk::PresentModeKHR::FIFO
			} else {
				[vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE]
					.into_iter()
					.find(|m| modes.contains(m))
					.unwrap_or(vk::PresentModeKHR::FIFO)
			};
			let mut count = caps.min_image_count + 1;
			if caps.max_image_count > 0 {
				count = count.min(caps.max_image_count);
			}
			let alpha = [
				vk::CompositeAlphaFlagsKHR::OPAQUE,
				vk::CompositeAlphaFlagsKHR::INHERIT,
				vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
				vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
			]
			.into_iter()
			.find(|a| caps.supported_composite_alpha.contains(*a))
			.unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);

			self.device.device_wait_idle().map_err(vk_err("wait idle"))?;
			let old = self.swapchain;
			let swapchain = self
				.swapchain_fn
				.create_swapchain(
					&vk::SwapchainCreateInfoKHR::default()
						.surface(self.surface)
						.min_image_count(count)
						.image_format(format.format)
						.image_color_space(format.color_space)
						.image_extent(extent)
						.image_array_layers(1)
						.image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
						.image_sharing_mode(vk::SharingMode::EXCLUSIVE)
						.pre_transform(caps.current_transform)
						.composite_alpha(alpha)
						.present_mode(present_mode)
						.clipped(true)
						.old_swapchain(old),
					None,
				)
				.map_err(vk_err("create swapchain"))?;
			self.destroy_swapchain_resources();
			self.swapchain_fn.destroy_swapchain(old, None);
			self.swapchain = swapchain;
			self.images = self
				.swapchain_fn
				.get_swapchain_images(swapchain)
				.map_err(vk_err("swapchain images"))?;
			for &image in &self.images {
				let view = self
					.device
					.create_image_view(
						&vk::ImageViewCreateInfo::default()
							.image(image)
							.view_type(vk::ImageViewType::TYPE_2D)
							.format(format.format)
							.subresource_range(COLOR),
						None,
					)
					.map_err(vk_err("image view"))?;
				self.views.push(view);
				let semaphore = self
					.device
					.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
					.map_err(vk_err("semaphore"))?;
				self.rendered.push(semaphore);
			}
			self.extent = extent;
			self.stale = false;
		}
		Ok(())
	}

	/// # Safety
	///
	/// The device must be idle.
	unsafe fn destroy_swapchain_resources(&mut self) {
		unsafe {
			for view in self.views.drain(..) {
				self.device.destroy_image_view(view, None);
			}
			for semaphore in self.rendered.drain(..) {
				self.device.destroy_semaphore(semaphore, None);
			}
		}
		self.images.clear();
	}

	/// Clear the next image to white or black and present it. Returns when the frame
	/// was submitted to the GPU, or None if nothing was drawn (the window is minimised
	/// or resizing). That's the last moment the app controls: with vsync,
	/// `queue_present` itself can block for a frame.
	pub fn draw(&mut self, white: bool) -> Result<Option<u64>, String> {
		if self.stale {
			self.create_swapchain()?;
			if self.stale {
				return Ok(None);
			}
		}
		let frame = &self.frames[self.frame];
		let submitted;
		// SAFETY: the frame's fence guards its command buffer and semaphore; the
		// image's `rendered` semaphore is only reused after its present.
		unsafe {
			self.device
				.wait_for_fences(&[frame.fence], true, u64::MAX)
				.map_err(vk_err("wait for frame"))?;
			let index =
				match self
					.swapchain_fn
					.acquire_next_image(self.swapchain, u64::MAX, frame.acquired, vk::Fence::null())
				{
					Ok((index, _)) => index,
					Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
						self.stale = true;
						return Ok(None);
					},
					Err(e) => return Err(format!("acquire: {e}")),
				};
			let (image, view, rendered) = (
				self.images[index as usize],
				self.views[index as usize],
				self.rendered[index as usize],
			);
			self.device
				.reset_fences(&[frame.fence])
				.map_err(vk_err("reset fence"))?;

			let cmd = frame.command_buffer;
			self.device
				.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
				.map_err(vk_err("reset command buffer"))?;
			self.device
				.begin_command_buffer(
					cmd,
					&vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
				)
				.map_err(vk_err("begin command buffer"))?;

			let to_attachment = vk::ImageMemoryBarrier2::default()
				.src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
				.old_layout(vk::ImageLayout::UNDEFINED)
				.new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.image(image)
				.subresource_range(COLOR);
			self.device.cmd_pipeline_barrier2(
				cmd,
				&vk::DependencyInfo::default().image_memory_barriers(&[to_attachment]),
			);

			let attachments = [vk::RenderingAttachmentInfo::default()
				.image_view(view)
				.image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.load_op(vk::AttachmentLoadOp::CLEAR)
				.store_op(vk::AttachmentStoreOp::STORE)
				.clear_value(vk::ClearValue {
					color: vk::ClearColorValue {
						float32: if white { [1.0; 4] } else { [0.0, 0.0, 0.0, 1.0] },
					},
				})];
			let area = vk::Rect2D {
				offset: vk::Offset2D::default(),
				extent: self.extent,
			};
			self.device.cmd_begin_rendering(
				cmd,
				&vk::RenderingInfo::default()
					.render_area(area)
					.layer_count(1)
					.color_attachments(&attachments),
			);
			self.device.cmd_end_rendering(cmd);

			let to_present = vk::ImageMemoryBarrier2::default()
				.src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
				.src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
				.dst_stage_mask(vk::PipelineStageFlags2::NONE)
				.old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
				.new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
				.image(image)
				.subresource_range(COLOR);
			self.device
				.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[to_present]));
			self.device
				.end_command_buffer(cmd)
				.map_err(vk_err("end command buffer"))?;

			let waits = [vk::SemaphoreSubmitInfo::default()
				.semaphore(frame.acquired)
				.stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
			let signals = [vk::SemaphoreSubmitInfo::default()
				.semaphore(rendered)
				.stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
			let buffers = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
			self.device
				.queue_submit2(
					self.queue,
					&[vk::SubmitInfo2::default()
						.wait_semaphore_infos(&waits)
						.command_buffer_infos(&buffers)
						.signal_semaphore_infos(&signals)],
					frame.fence,
				)
				.map_err(vk_err("submit"))?;
			submitted = now_us();

			let swapchains = [self.swapchain];
			let indices = [index];
			let wait = [rendered];
			match self.swapchain_fn.queue_present(
				self.queue,
				&vk::PresentInfoKHR::default()
					.wait_semaphores(&wait)
					.swapchains(&swapchains)
					.image_indices(&indices),
			) {
				Ok(false) => {},
				Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.stale = true,
				Err(e) => return Err(format!("present: {e}")),
			}
		}
		self.frame = (self.frame + 1) % FRAMES_IN_FLIGHT;
		Ok(Some(submitted))
	}
}

impl Drop for Gpu {
	fn drop(&mut self) {
		// SAFETY: nothing is in flight after the wait, and each object is
		// destroyed once, children before their parents.
		unsafe {
			let _ = self.device.device_wait_idle();
			self.destroy_swapchain_resources();
			self.swapchain_fn.destroy_swapchain(self.swapchain, None);
			for frame in &self.frames {
				self.device.destroy_fence(frame.fence, None);
				self.device.destroy_semaphore(frame.acquired, None);
			}
			self.device.destroy_command_pool(self.pool, None);
			self.device.destroy_device(None);
			self.surface_fn.destroy_surface(self.surface, None);
			self.instance.destroy_instance(None);
		}
	}
}
