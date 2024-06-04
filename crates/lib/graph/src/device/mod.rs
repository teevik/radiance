//! An abstraction over a raw Vulkan device.

use std::{
	mem::ManuallyDrop,
	sync::{Mutex, MutexGuard},
};

use ash::{
	extensions::{ext, khr},
	vk,
};
pub use gpu_allocator::vulkan as alloc;
use gpu_allocator::vulkan::Allocator;

use crate::{device::descriptor::Descriptors, Result};

pub mod descriptor;
mod init;

/// Has everything you need to do Vulkan stuff.
pub struct Device {
	debug_messenger: vk::DebugUtilsMessengerEXT, // Can be null.
	physical_device: vk::PhysicalDevice,
	device: ash::Device,
	as_ext: khr::AccelerationStructure,
	rt_ext: khr::RayTracingPipeline,
	surface_ext: Option<khr::Surface>,
	debug_utils_ext: Option<ext::DebugUtils>,
	queues: Queues<QueueData>,
	allocator: ManuallyDrop<Mutex<Allocator>>,
	descriptors: Descriptors,
	instance: ash::Instance,
	entry: ash::Entry,
}

struct QueueData {
	queue: Mutex<vk::Queue>,
	family: u32,
}

/// The type of a queue.
#[derive(Copy, Clone)]
pub enum QueueType {
	Graphics,
	Compute,
	Transfer,
}

/// Data consisting of two queue strategies:
/// - Separate: Separate queues for graphics and presentation, async compute, and DMA transfer.
/// - Single: One queue for all operations.
pub struct Queues<T> {
	pub graphics: T, // Also supports presentation.
	pub compute: T,
	pub transfer: T,
}

impl<T> Queues<T> {
	pub fn get(&self, ty: QueueType) -> &T {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		match ty {
			QueueType::Graphics => graphics,
			QueueType::Compute => compute,
			QueueType::Transfer => transfer,
		}
	}

	pub fn get_mut(&mut self, ty: QueueType) -> &mut T {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		match ty {
			QueueType::Graphics => graphics,
			QueueType::Compute => compute,
			QueueType::Transfer => transfer,
		}
	}

	pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Queues<U> {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		Queues {
			graphics: f(graphics),
			compute: f(compute),
			transfer: f(transfer),
		}
	}

	pub fn map_ref<U>(&self, mut f: impl FnMut(&T) -> U) -> Queues<U> {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		Queues {
			graphics: f(graphics),
			compute: f(compute),
			transfer: f(transfer),
		}
	}

	pub fn map_mut<U>(&mut self, mut f: impl FnMut(&mut T) -> U) -> Queues<U> {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		Queues {
			graphics: f(graphics),
			compute: f(compute),
			transfer: f(transfer),
		}
	}

	pub fn try_map_ref<U, E>(
		&self, mut f: impl FnMut(&T) -> std::result::Result<U, E>,
	) -> std::result::Result<Queues<U>, E> {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		Ok(Queues {
			graphics: f(graphics)?,
			compute: f(compute)?,
			transfer: f(transfer)?,
		})
	}

	pub fn try_map<U, E>(self, mut f: impl FnMut(T) -> std::result::Result<U, E>) -> std::result::Result<Queues<U>, E> {
		let Queues {
			graphics,
			compute,
			transfer,
		} = self;
		Ok(Queues {
			graphics: f(graphics)?,
			compute: f(compute)?,
			transfer: f(transfer)?,
		})
	}
}

impl Device {
	pub fn entry(&self) -> &ash::Entry { &self.entry }

	pub fn instance(&self) -> &ash::Instance { &self.instance }

	pub fn device(&self) -> &ash::Device { &self.device }

	pub fn physical_device(&self) -> vk::PhysicalDevice { self.physical_device }

	pub fn as_ext(&self) -> &khr::AccelerationStructure { &self.as_ext }

	pub fn rt_ext(&self) -> &khr::RayTracingPipeline { &self.rt_ext }

	pub fn surface_ext(&self) -> Option<&khr::Surface> { self.surface_ext.as_ref() }

	pub fn debug_utils_ext(&self) -> Option<&ext::DebugUtils> { self.debug_utils_ext.as_ref() }

	pub fn queue_families(&self) -> Queues<u32> { self.queues.map_ref(|data| data.family) }

	pub fn graphics_queue(&self) -> MutexGuard<'_, vk::Queue> { self.queues.graphics.queue.lock().unwrap() }

	pub fn compute_queue(&self) -> MutexGuard<'_, vk::Queue> { self.queues.compute.queue.lock().unwrap() }

	pub fn transfer_queue(&self) -> MutexGuard<'_, vk::Queue> { self.queues.transfer.queue.lock().unwrap() }

	pub fn allocator(&self) -> MutexGuard<'_, Allocator> { self.allocator.lock().unwrap() }

	pub fn descriptors(&self) -> &Descriptors { &self.descriptors }

	/// # Safety
	/// Thread-safety is handled, nothing else is.
	pub unsafe fn submit(&self, ty: QueueType, submits: &[vk::SubmitInfo2], fence: vk::Fence) -> Result<()> {
		let queue = self.queues.get(ty);
		self.device
			.queue_submit2(*queue.queue.lock().unwrap(), submits, fence)?;

		Ok(())
	}

	/// # Safety
	/// Thread-safety is handled, nothing else is.
	pub unsafe fn submit_graphics(&self, submits: &[vk::SubmitInfo2], fence: vk::Fence) -> Result<()> {
		let queue = &self.queues.graphics;
		self.device
			.queue_submit2(*queue.queue.lock().unwrap(), submits, fence)?;

		Ok(())
	}

	/// # Safety
	/// Thread-safety is handled, nothing else is.
	pub unsafe fn submit_compute(&self, submits: &[vk::SubmitInfo2], fence: vk::Fence) -> Result<()> {
		let queue = &self.queues.compute;
		self.device
			.queue_submit2(*queue.queue.lock().unwrap(), submits, fence)?;

		Ok(())
	}

	/// # Safety
	/// Thread-safety is handled, nothing else is.
	pub unsafe fn submit_transfer(&self, submits: &[vk::SubmitInfo2], fence: vk::Fence) -> Result<()> {
		let queue = &self.queues.transfer;
		self.device
			.queue_submit2(*queue.queue.lock().unwrap(), submits, fence)?;

		Ok(())
	}
}

impl Drop for Device {
	fn drop(&mut self) {
		unsafe {
			// Drop the allocator before the device.
			ManuallyDrop::drop(&mut self.allocator);
			self.descriptors.cleanup(&self.device);

			self.device.destroy_device(None);

			if let Some(utils) = self.debug_utils_ext.as_ref() {
				utils.destroy_debug_utils_messenger(self.debug_messenger, None);
			}
			self.instance.destroy_instance(None);
		}
	}
}
