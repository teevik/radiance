use bincode::{Decode, Encode};
use bytemuck::{Pod, Zeroable};
use static_assertions::const_assert_eq;
use uuid::Uuid;
use vek::{Aabb, Vec2, Vec3};

#[derive(Pod, Zeroable, Copy, Clone, Default, Encode, Decode)]
#[repr(C)]
pub struct Vertex {
	/// Normalized vertex coordinates relative to the meshlet AABB.
	#[bincode(with_serde)]
	pub position: Vec3<u16>,
	/// Signed normalized normal vector.
	#[bincode(with_serde)]
	pub normal: Vec3<i16>,
	/// Normalized UV coordinates relative to the [0.0, 1.0] UV range.
	#[bincode(with_serde)]
	pub uv: Vec2<u16>,
}

const_assert_eq!(std::mem::size_of::<Vertex>(), 16);
const_assert_eq!(std::mem::align_of::<Vertex>(), 2);

#[derive(Copy, Clone, Encode, Decode)]
#[repr(C)]
pub struct Meshlet {
	/// AABB of the meshlet relative to the mesh origin.
	#[bincode(with_serde)]
	pub aabb_min: Vec3<f32>,
	#[bincode(with_serde)]
	pub aabb_extent: Vec3<f32>,
	/// Offset of the meshlet index buffer relative to the parent mesh index buffer.
	pub index_offset: u32,
	/// Offset of the meshlet vertex buffer relative to the parent mesh vertex buffer.
	pub vertex_offset: u32,
	/// Number of triangles in the meshlet. The number of indices will be 3 times this.
	pub tri_count: u8,
	/// Number of vertices in the meshlet.
	pub vert_count: u8,
	pub _pad: u16,
}

const_assert_eq!(std::mem::size_of::<Meshlet>(), 36);
const_assert_eq!(std::mem::align_of::<Meshlet>(), 4);

/// A mesh asset consisting of meshlets.
#[derive(Encode, Decode)]
pub struct Mesh {
	/// Vertices of the mesh.
	pub vertices: Vec<Vertex>,
	/// Indices of each meshlet - should be added to `vertex_offset`.
	pub indices: Vec<u8>,
	/// Meshlets of the mesh.
	pub meshlets: Vec<Meshlet>,
	/// AABB of the mesh.
	#[bincode(with_serde)]
	pub aabb: Aabb<f32>,
	/// Material of the mesh.
	#[bincode(with_serde)]
	pub material: Uuid,
}
