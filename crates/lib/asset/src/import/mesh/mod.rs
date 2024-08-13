use std::{collections::BTreeMap, ops::Range};

use bytemuck::from_bytes;
use gltf::accessor::{DataType, Dimensions};
use meshopt::VertexDataAdapter;
use metis::Graph;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rustc_hash::FxHashMap;
use tracing::{debug_span, info_span, trace_span};
use uuid::Uuid;
use vek::{Sphere, Vec2, Vec3};

use crate::{
	import::{ImportError, ImportResult, Importer},
	mesh::{Mesh, Meshlet, Vertex},
};

mod simplify;

#[derive(Copy, Clone)]
struct MappedMeshlet {
	vertex_offset: u32,
	vertex_count: u32,
	tri_offset: u32,
	tri_count: u32,
	bounding: Sphere<f32, f32>,
	group_error: Sphere<f32, f32>,
	parent_group_error: Sphere<f32, f32>,
}

impl MappedMeshlet {
	fn vertices(&self) -> Range<usize> {
		(self.vertex_offset as usize)..(self.vertex_offset as usize + self.vertex_count as usize)
	}

	fn tris(&self) -> Range<usize> {
		(self.tri_offset as usize)..(self.tri_offset as usize + self.tri_count as usize * 3)
	}
}

struct Meshlets {
	vertices: Vec<Vertex>,
	vertex_remap: Vec<u32>,
	tris: Vec<u8>,
	meshlets: Vec<MappedMeshlet>,
}

impl Meshlets {
	fn add(&mut self, other: Meshlets) {
		let vertex_offset = self.vertex_remap.len() as u32;
		let tri_offset = self.tris.len() as u32;
		self.meshlets.extend(other.meshlets.into_iter().map(|mut m| {
			m.vertex_offset += vertex_offset;
			m.tri_offset += tri_offset;
			m
		}));
		let vertex_data_offset = self.vertices.len() as u32;
		self.vertices.extend(other.vertices);
		self.vertex_remap
			.extend(other.vertex_remap.into_iter().map(|v| v + vertex_data_offset));
		self.tris.extend(other.tris);
	}
}

impl Importer<'_> {
	pub fn mesh(&self, name: &str, mesh: gltf::Mesh, materials: &[Uuid]) -> ImportResult<Mesh> {
		let s = info_span!("importing mesh", name = name);
		let _e = s.enter();

		let mesh = self.conv_to_mesh(mesh, materials)?;
		let mut meshlets = self.generate_meshlets(mesh, None);

		let mut simplify = 0..meshlets.meshlets.len();
		let mut lod = 1;
		while simplify.len() > 1 {
			let s = debug_span!("generating lod", lod, meshlets = simplify.len());
			let _e = s.enter();

			let next_start = meshlets.meshlets.len();
			let groups = self.generate_groups(simplify.clone(), &meshlets);

			let par: Vec<_> = groups
				.into_par_iter()
				.filter(|x| x.len() > 1)
				.filter_map(|group| {
					let (mesh, group_error) = self.simplify_group(&group, &meshlets)?;
					let n_meshlets = self.generate_meshlets(mesh, Some(group_error));
					Some((group, group_error, n_meshlets))
				})
				.collect();
			for (group, group_error, n_meshlets) in par {
				for mid in group {
					meshlets.meshlets[mid].parent_group_error = group_error;
				}
				meshlets.add(n_meshlets);
			}

			simplify = next_start..meshlets.meshlets.len();
			lod += 1;
		}

		Ok(self.convert_meshlets(meshlets))
	}

	fn convert_meshlets(&self, meshlets: Meshlets) -> Mesh {
		let vertices = meshlets.vertices;
		let mut out = Mesh {
			vertices: Vec::with_capacity(vertices.len()),
			indices: Vec::with_capacity(meshlets.meshlets.len() * 124 * 3),
			meshlets: Vec::with_capacity(meshlets.meshlets.len()),
			material_ranges: Vec::new(),
		};

		out.meshlets.extend(meshlets.meshlets.into_iter().map(|m| {
			let indices: Vec<_> = meshlets.tris[m.tris()].iter().map(|&x| x as u32).collect();
			let vertices = meshlets.vertex_remap[m.vertices()]
				.iter()
				.map(|&x| vertices[x as usize]);

			let index_offset = out.indices.len() as u32;
			let vertex_offset = out.vertices.len() as u32;
			let vert_count = vertices.len() as u8;
			let tri_count = (indices.len() / 3) as u8;

			out.vertices.extend(vertices);
			out.indices.extend(indices.into_iter().map(|x| x as u8));
			let start = out.material_ranges.len() as _;

			Meshlet {
				index_offset,
				vertex_offset,
				tri_count,
				vert_count,
				material_ranges: start..(out.material_ranges.len() as _),
				bounding: m.bounding,
				group_error: m.group_error,
				parent_group_error: m.parent_group_error,
			}
		}));

		out
	}

	fn generate_meshlets(&self, mesh: FullMesh, group_error: Option<Sphere<f32, f32>>) -> Meshlets {
		let s = trace_span!("building meshlets");
		let _e = s.enter();

		let adapter = VertexDataAdapter::new(
			bytemuck::cast_slice(mesh.vertices.as_slice()),
			std::mem::size_of::<Vertex>(),
			0,
		)
		.unwrap();
		let ms = meshopt::build_meshlets(&mesh.indices, &adapter, 64, 124, 0.5);
		let meshlets = ms
			.meshlets
			.iter()
			.enumerate()
			.map(|(i, m)| {
				let mbounds = meshopt::compute_meshlet_bounds(ms.get(i), &adapter);
				let center = Vec3::from(mbounds.center);
				let group_error = group_error.unwrap_or(Sphere { center, radius: 0.0 });
				MappedMeshlet {
					vertex_offset: m.vertex_offset,
					vertex_count: m.vertex_count,
					tri_offset: m.triangle_offset,
					tri_count: m.triangle_count,
					bounding: Sphere {
						center,
						radius: mbounds.radius,
					},
					group_error,
					parent_group_error: Sphere {
						center: group_error.center,
						radius: f32::INFINITY,
					},
				}
			})
			.collect();

		Meshlets {
			vertices: mesh.vertices,
			vertex_remap: ms.vertices,
			tris: ms.triangles,
			meshlets,
		}
	}

	fn simplify_group(&self, group: &[usize], meshlets: &Meshlets) -> Option<(FullMesh, Sphere<f32, f32>)> {
		let s = trace_span!("simplifying group");
		let _e = s.enter();

		let indices: Vec<_> = group
			.iter()
			.map(|&mid| meshlets.meshlets[mid])
			.flat_map(|m| {
				meshlets.tris[m.tris()]
					.iter()
					.map(move |&x| meshlets.vertex_remap[m.vertices()][x as usize])
			})
			.collect();
		let (omesh, mut error) = simplify::simplify_mesh(&meshlets.vertices, &indices)?;

		error.radius += group
			.iter()
			.map(|&x| meshlets.meshlets[x].group_error.radius)
			.reduce(f32::max)
			.unwrap();

		Some((omesh, error))
	}

	fn generate_groups(&self, range: Range<usize>, meshlets: &Meshlets) -> Vec<Vec<usize>> {
		let s = trace_span!("grouping meshlets");
		let _e = s.enter();

		// TODO: locality links
		let connections = self.find_connections(range.clone(), meshlets);

		let mut xadj = Vec::with_capacity(range.len() + 1);
		let mut adj = Vec::new();
		let mut weights = Vec::new();

		for mid in range.clone() {
			xadj.push(adj.len() as i32);
			for &(connected, count) in connections[mid - range.start].iter() {
				adj.push(connected as i32);
				weights.push(count as i32);
			}
		}
		xadj.push(adj.len() as i32);

		let mut group_of = vec![0; range.len()];
		let group_count = range.len().div_ceil(4);
		Graph::new(1, group_count as _, &xadj, &adj)
			.unwrap()
			.set_adjwgt(&weights)
			.part_kway(&mut group_of)
			.unwrap();

		let mut groups = vec![Vec::new(); group_count];
		for (i, group) in group_of.into_iter().enumerate() {
			groups[group as usize].push(i + range.start);
		}
		groups
	}

	fn find_connections(&self, range: Range<usize>, meshlets: &Meshlets) -> Vec<Vec<(usize, usize)>> {
		let s = trace_span!("generating meshlet graph");
		let _e = s.enter();

		let mut shared_edges = FxHashMap::default();
		for mid in range.clone() {
			let m = &meshlets.meshlets[mid];
			let verts = &meshlets.vertex_remap[m.vertices()];
			for i in meshlets.tris[m.tris()].chunks(3) {
				for j in 0..3 {
					let i0 = i[j] as usize;
					let i1 = i[(j + 1) % 3] as usize;
					let v0 = verts[i0];
					let v1 = verts[i1];
					let edge = (v0.min(v1), v0.max(v1));
					let out = shared_edges.entry(edge).or_insert(Vec::new());
					if out.last() != Some(&mid) {
						out.push(mid);
					}
				}
			}
		}

		let mut shared_count = BTreeMap::new();
		for (_, mids) in shared_edges {
			for i in 0..mids.len() {
				for j in (i + 1)..mids.len() {
					let i = mids[i];
					let j = mids[j];
					*shared_count.entry((i.min(j), i.max(j))).or_default() += 1;
				}
			}
		}

		let mut connections = vec![Vec::new(); range.len()];
		for ((m1, m2), count) in shared_count {
			connections[m1 - range.start].push((m2 - range.start, count));
			connections[m2 - range.start].push((m1 - range.start, count));
		}
		connections
	}

	fn conv_to_mesh(&self, mesh: gltf::Mesh, _: &[Uuid]) -> ImportResult<FullMesh> {
		let s = trace_span!("convert from gltf");
		let _e = s.enter();

		let mut out = FullMesh {
			vertices: Vec::with_capacity(
				mesh.primitives()
					.flat_map(|x| x.get(&gltf::Semantic::Positions).map(|x| x.count()))
					.sum(),
			),
			indices: Vec::with_capacity(mesh.primitives().flat_map(|x| x.indices().map(|x| x.count())).sum()),
		};
		for prim in mesh.primitives() {
			let positions = prim.get(&gltf::Semantic::Positions).ok_or(ImportError::InvalidGltf)?;
			let (positions, ty, comp) = self.accessor(positions)?;
			if comp != Dimensions::Vec3 || ty != DataType::F32 {
				return Err(ImportError::InvalidGltf);
			}
			let positions = positions.map(|p| *from_bytes::<Vec3<f32>>(p));

			let normals = prim.get(&gltf::Semantic::Normals).ok_or(ImportError::InvalidGltf)?;
			let (normals, ty, comp) = self.accessor(normals)?;
			if comp != Dimensions::Vec3 || ty != DataType::F32 {
				return Err(ImportError::InvalidGltf);
			}
			let normals = normals.map(|n| *from_bytes::<Vec3<f32>>(n));

			let uv = prim.get(&gltf::Semantic::TexCoords(0));
			let mut uv = uv
				.map(|uv| {
					let (uv, ty, comp) = self.accessor(uv)?;
					if comp != Dimensions::Vec2 {
						return Err(ImportError::InvalidGltf);
					}

					if !matches!(ty, DataType::F32 | DataType::U8 | DataType::U16) {
						return Err(ImportError::InvalidGltf);
					}
					Ok(uv.map(move |uv| match ty {
						DataType::F32 => *from_bytes(uv),
						DataType::U8 => from_bytes::<Vec2<u8>>(uv).map(|u| u as f32 / 255.0),
						DataType::U16 => from_bytes::<Vec2<u16>>(uv).map(|u| u as f32 / 65535.0),
						_ => panic!("yikes"),
					}))
				})
				.transpose()?;

			let indices = prim.indices().ok_or(ImportError::InvalidGltf)?;
			let (indices, ty, comp) = self.accessor(indices)?;
			if comp != Dimensions::Scalar {
				return Err(ImportError::InvalidGltf);
			}
			let offset = out.vertices.len() as u32;
			match ty {
				DataType::U8 => out.indices.extend(indices.flatten().map(|&i| i as u32 + offset)),
				DataType::U16 => out
					.indices
					.extend(indices.map(|i| *from_bytes::<u16>(i) as u32 + offset)),
				DataType::U32 => out.indices.extend(indices.map(|i| *from_bytes::<u32>(i) + offset)),
				_ => return Err(ImportError::InvalidGltf),
			}

			out.vertices.extend(
				positions
					.zip(normals)
					.zip(std::iter::from_fn(move || {
						if let Some(ref mut uv) = uv {
							uv.next()
						} else {
							Some(Vec2::new(0.0, 0.0))
						}
					}))
					.map(|((position, normal), uv)| Vertex { position, normal, uv }),
			);
		}

		Ok(out)
	}
}

struct FullMesh {
	vertices: Vec<Vertex>,
	indices: Vec<u32>,
}
