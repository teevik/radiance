#![feature(allocator_api)]

//! Bridge between raw assets and cached assets on the GPU or CPU.

use std::{collections::hash_map::Entry, fmt::Debug};

use ash::vk;
use crossbeam_channel::{Receiver, Sender};
use material::GpuMaterial;
use radiance_asset::{AssetError, AssetSource, AssetSystem};
use radiance_graph::{
	device::{descriptor::BufferId, Device},
	graph::{Frame, Resource},
	resource::{Buffer, BufferDesc, Resource as _},
};
use rref::{RRef, RWeak, RuntimeAsset};
use rustc_hash::FxHashMap;
use uuid::Uuid;

pub mod image;
pub mod material;
pub mod mesh;
pub mod rref;
pub mod scene;

pub enum DelRes {
	Resource(Resource),
	Material(u32),
}

impl From<Resource> for DelRes {
	fn from(value: Resource) -> Self { Self::Resource(value) }
}

pub struct AssetRuntime {
	deleter: Sender<DelRes>,
	delete_recv: Receiver<DelRes>,
	scenes: FxHashMap<Uuid, RWeak<scene::Scene>>,
	images: FxHashMap<Uuid, RWeak<image::Image>>,
	materials: FxHashMap<Uuid, RWeak<material::Material>>,
	meshes: FxHashMap<Uuid, RWeak<mesh::Mesh>>,
	material_buffer: Buffer,
}

impl AssetRuntime {
	pub fn new(device: &Device) -> radiance_graph::Result<Self> {
		let (send, recv) = crossbeam_channel::unbounded();
		Ok(Self {
			deleter: send,
			delete_recv: recv,
			scenes: FxHashMap::default(),
			images: FxHashMap::default(),
			materials: FxHashMap::default(),
			meshes: FxHashMap::default(),
			material_buffer: Buffer::create(
				device,
				BufferDesc {
					name: "materials",
					size: std::mem::size_of::<GpuMaterial>() as u64 * 1000,
					usage: vk::BufferUsageFlags::STORAGE_BUFFER,
					on_cpu: false,
				},
			)?,
		})
	}

	pub unsafe fn destroy(self, device: &Device) {
		for (_, s) in self.scenes {
			assert!(
				s.upgrade().is_none(),
				"Cannot destroy `AssetRuntime` with scene still alive"
			)
		}
		for (_, i) in self.images {
			assert!(
				i.upgrade().is_none(),
				"Cannot destroy `AssetRuntime` with images still alive"
			)
		}
		for (_, m) in self.materials {
			assert!(
				m.upgrade().is_none(),
				"Cannot destroy `AssetRuntime` with materials still alive"
			)
		}
		for (_, m) in self.meshes {
			assert!(
				m.upgrade().is_none(),
				"Cannot destroy `AssetRuntime` with meshes still alive"
			)
		}

		for x in self.delete_recv.try_iter() {
			match x {
				DelRes::Resource(r) => unsafe { r.destroy(device) },
				DelRes::Material(_) => {},
			}
		}

		self.material_buffer.destroy(device);
	}

	pub fn tick(&mut self, frame: &mut Frame) {
		while let Ok(x) = self.delete_recv.try_recv() {
			match x {
				DelRes::Resource(x) => frame.delete(x),
				// TODO: delete materials
				DelRes::Material(_) => {},
			}
		}
	}

	pub fn materials(&self) -> BufferId { self.material_buffer.id().unwrap() }

	pub fn load<S: AssetSource, R>(
		&mut self, device: &Device, sys: &AssetSystem<S>,
		exec: impl FnOnce(&mut Self, &mut Loader<'_, S>) -> Result<R, LoadError<S>>,
	) -> Result<R, LoadError<S>> {
		let mut loader = Loader {
			device,
			sys,
			deleter: self.deleter.clone(),
		};
		exec(self, &mut loader)
	}

	pub fn load_scene<S: AssetSource>(&mut self, loader: &mut Loader<'_, S>, uuid: Uuid) -> LResult<scene::Scene, S> {
		match Self::get_cache(&mut self.scenes, uuid) {
			Some(x) => Ok(x),
			None => {
				let s = self.load_scene_from_disk(loader, uuid)?;
				self.scenes.insert(uuid, s.downgrade());
				Ok(s)
			},
		}
	}

	pub fn load_image<S: AssetSource>(
		&mut self, loader: &mut Loader<'_, S>, uuid: Uuid, srgb: bool,
	) -> LResult<image::Image, S> {
		match Self::get_cache(&mut self.images, uuid) {
			Some(x) => Ok(x),
			None => {
				let i = self.load_image_from_disk(loader, uuid, srgb)?;
				self.images.insert(uuid, i.downgrade());
				Ok(i)
			},
		}
	}

	pub fn load_material<S: AssetSource>(
		&mut self, loader: &mut Loader<'_, S>, uuid: Uuid,
	) -> LResult<material::Material, S> {
		match Self::get_cache(&mut self.materials, uuid) {
			Some(x) => Ok(x),
			None => {
				let m = self.load_material_from_disk(loader, uuid)?;
				self.materials.insert(uuid, m.downgrade());
				Ok(m)
			},
		}
	}

	pub fn load_mesh<S: AssetSource>(&mut self, loader: &mut Loader<'_, S>, uuid: Uuid) -> LResult<mesh::Mesh, S> {
		match Self::get_cache(&mut self.meshes, uuid) {
			Some(x) => Ok(x),
			None => {
				let m = self.load_mesh_from_disk(loader, uuid)?;
				self.meshes.insert(uuid, m.downgrade());
				Ok(m)
			},
		}
	}

	pub fn get_cache<T: RuntimeAsset>(map: &mut FxHashMap<Uuid, RWeak<T>>, uuid: Uuid) -> Option<RRef<T>> {
		match map.entry(uuid) {
			Entry::Occupied(o) => match o.get().upgrade() {
				Some(x) => Some(x),
				None => {
					o.remove_entry();
					None
				},
			},
			Entry::Vacant(_) => None,
		}
	}
}

pub enum LoadError<S: AssetSource> {
	Vulkan(radiance_graph::Error),
	Asset(AssetError<S>),
}
impl<S: AssetSource> From<AssetError<S>> for LoadError<S> {
	fn from(value: AssetError<S>) -> Self { Self::Asset(value) }
}
impl<S: AssetSource> Debug for LoadError<S> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Vulkan(e) => Debug::fmt(e, f),
			Self::Asset(e) => e.fmt(f),
		}
	}
}

type LResult<T, S> = Result<RRef<T>, LoadError<S>>;

pub struct Loader<'a, S> {
	device: &'a Device,
	sys: &'a AssetSystem<S>,
	deleter: Sender<DelRes>,
}
