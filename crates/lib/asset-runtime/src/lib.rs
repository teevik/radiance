#![feature(allocator_api)]

//! Bridge between raw assets and cached assets on the GPU or CPU.

use ash::vk;
use bytemuck::NoUninit;
use radiance_asset::{
	mesh::{Meshlet, Vertex},
	util::SliceWriter,
	Asset,
	AssetSource,
	AssetSystem,
};
use radiance_graph::{
	arena::Arena,
	device::{descriptor::BufferId, Device},
	gpu_allocator::MemoryLocation,
	resource::{Buffer, BufferDesc},
};
use radiance_util::staging::{StageTicket, Staging, StagingCtx};
use rustc_hash::FxHashMap;
use uuid::Uuid;
use vek::Vec4;

pub struct Model {
	/// Organized as:
	/// - `meshlet_count`.
	/// - `vertex_count`.
	/// - `index_count`.
	/// - `meshlet_count` meshlets.
	/// - `vertex_count` vertices.
	/// - `index_count` indices.
	pub buffer: Buffer,
	pub meshlet_count: u32,
	pub vertex_count: u32,
	pub index_count: u32,
}

#[repr(C)]
#[derive(Copy, Clone, NoUninit)]
pub struct Instance {
	/// Columns.
	pub transform: Vec4<Vec4<f32>>,
	pub buffer: BufferId,
}

#[repr(C)]
#[derive(Copy, Clone, NoUninit)]
pub struct MeshletPointer {
	pub instance: u32,
	pub meshlet: u32,
}

pub struct Scene {
	pub instances: Buffer,
	pub meshlets: Buffer,
	pub instance_count: u32,
	pub meshlet_count: u32,
}

pub enum LoadedAsset {
	Model(Model),
	Scene(Scene),
}

/// ID of a loaded asset.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct AssetId(u32);

pub struct AssetRuntime {
	id_map: FxHashMap<Uuid, AssetId>,
	assets: Vec<LoadedAsset>,
	staging: Staging,
}

impl AssetRuntime {
	pub fn new(device: &Device) -> radiance_graph::Result<Self> {
		Ok(Self {
			id_map: FxHashMap::default(),
			assets: Vec::new(),
			staging: Staging::new(device)?,
		})
	}

	pub fn get(&self, asset: AssetId) -> &LoadedAsset { &self.assets[asset.0 as usize] }

	pub fn load_asset<S: AssetSource>(
		&mut self, device: &Device, wait: Vec<vk::SemaphoreSubmitInfo, &Arena>, asset: Uuid,
		system: &mut AssetSystem<S>,
	) -> Result<(AssetId, StageTicket), S::Error> {
		let mut out = Ok(AssetId(0));
		let ticket = self
			.staging
			.stage(device, wait, |ctx| {
				out = Self::load_inner(&mut self.id_map, &mut self.assets, device, ctx, asset, system);
				Ok(())
			})
			.unwrap();
		out.map(|x| (x, ticket))
	}

	fn load_inner<S: AssetSource>(
		map: &mut FxHashMap<Uuid, AssetId>, assets: &mut Vec<LoadedAsset>, device: &Device, ctx: &mut StagingCtx,
		asset: Uuid, system: &mut AssetSystem<S>,
	) -> Result<AssetId, S::Error> {
		if let Some(&x) = map.get(&asset) {
			return Ok(x);
		}

		let uuid = asset;
		let asset = system.load(asset)?;
		let asset = match asset {
			Asset::Mesh(_) => unreachable!("Meshes are not loaded directly"),
			Asset::Model(m) => {
				let meshes: Vec<_> = m
					.meshes
					.into_iter()
					.map(|x| {
						system.load(x).map(|x| match x {
							Asset::Mesh(m) => m,
							_ => unreachable!("Model mesh is not a mesh"),
						})
					})
					.collect::<Result<_, _>>()?;
				let meshlet_count = meshes.iter().map(|x| x.meshlets.len() as u32).sum::<u32>();
				let vertex_count = meshes.iter().map(|x| x.vertices.len() as u32).sum::<u32>();
				let index_count = meshes.iter().map(|x| x.indices.len() as u32).sum::<u32>();
				let mut bytes = vec![
					0;
					std::mem::size_of::<u32>() * 3
						+ std::mem::size_of::<Meshlet>() * meshlet_count as usize
						+ std::mem::size_of::<Vertex>() * vertex_count as usize
						+ std::mem::size_of::<u32>() * index_count as usize
				];
				let mut writer = SliceWriter::new(&mut bytes);
				writer.write(meshlet_count);
				writer.write(vertex_count);
				writer.write(index_count);
				let mut vertex_offset = 0;
				let mut index_offset = 0;
				for mesh in meshes.iter() {
					for &meshlet in mesh.meshlets.iter() {
						let meshlet = Meshlet {
							vertex_offset: vertex_offset + meshlet.vertex_offset,
							index_offset: index_offset + meshlet.index_offset,
							..meshlet
						};
						writer.write(meshlet);
					}
					vertex_offset += mesh.vertices.len() as u32;
				}

				for mesh in meshes.iter() {
					writer.write_slice(&mesh.vertices);
				}

				let mut vertex_offset = 0;
				for mesh in meshes.iter() {
					for &meshlet in mesh.meshlets.iter() {
						for &index in mesh.indices[meshlet.index_offset as usize
							..meshlet.index_offset as usize + meshlet.tri_count as usize * 3]
							.iter()
						{
							writer.write(index as u32 + vertex_offset + meshlet.vertex_offset);
						}
					}
					vertex_offset += mesh.vertices.len() as u32;
				}

				let buffer = Buffer::create(
					device,
					BufferDesc {
						size: bytes.len(),
						usage: vk::BufferUsageFlags::TRANSFER_DST
							| vk::BufferUsageFlags::STORAGE_BUFFER
							| vk::BufferUsageFlags::INDEX_BUFFER,
					},
					MemoryLocation::GpuOnly,
				)
				.unwrap();
				ctx.stage_buffer(&bytes, buffer.inner(), 0).unwrap();
				LoadedAsset::Model(Model {
					buffer,
					meshlet_count,
					vertex_count,
					index_count,
				})
			},
			Asset::Scene(s) => {
				let instances = s
					.nodes
					.into_iter()
					.map(|x| Self::load_inner(map, assets, device, ctx, x.model, system).map(|id| (x.transform, id)))
					.collect::<Result<Vec<_>, _>>()?;
				let instance_count = instances.len() as u32;
				let meshlet_count = instances
					.iter()
					.map(|(_, id)| match &assets[id.0 as usize] {
						LoadedAsset::Model(x) => x.meshlet_count,
						_ => unreachable!("Scene node is not a model"),
					})
					.sum::<u32>();

				let mut instance_bytes = vec![0; std::mem::size_of::<Instance>() * instances.len()];
				let mut instance_writer = SliceWriter::new(&mut instance_bytes);
				let mut meshlet_bytes = vec![0; std::mem::size_of::<MeshletPointer>() * meshlet_count as usize];
				let mut meshlet_writer = SliceWriter::new(&mut meshlet_bytes);
				for (instance, (transform, id)) in instances.into_iter().enumerate() {
					let m = match &assets[id.0 as usize] {
						LoadedAsset::Model(x) => x,
						_ => unreachable!("Scene node is not a model"),
					};
					instance_writer.write(Instance {
						transform: transform.cols,
						buffer: m.buffer.id().unwrap(),
					});
					for meshlet in 0..m.meshlet_count {
						meshlet_writer.write(MeshletPointer {
							instance: instance as u32,
							meshlet,
						});
					}
				}

				let instances = Buffer::create(
					device,
					BufferDesc {
						size: instance_bytes.len(),
						usage: vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
					},
					MemoryLocation::GpuOnly,
				)
				.unwrap();
				ctx.stage_buffer(&instance_bytes, instances.inner(), 0).unwrap();

				let meshlets = Buffer::create(
					device,
					BufferDesc {
						size: meshlet_bytes.len(),
						usage: vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
					},
					MemoryLocation::GpuOnly,
				)
				.unwrap();
				ctx.stage_buffer(&meshlet_bytes, meshlets.inner(), 0).unwrap();

				LoadedAsset::Scene(Scene {
					instances,
					meshlets,
					instance_count,
					meshlet_count,
				})
			},
		};

		let id = AssetId(assets.len() as u32);
		assets.push(asset);
		map.insert(uuid, id);

		Ok(id)
	}
}
