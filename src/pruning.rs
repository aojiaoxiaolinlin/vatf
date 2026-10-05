//! Compact serialized resources from final baked references, never visibility guesses.
use crate::{
    GradientUniforms, MorphEntry, ShapeMesh, ShapeRecord, VatfBuilder, Vertex,
    baked::{BakedMovie, BakedNode},
    graphics::Graphic,
    material,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    mem::size_of,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceCounts {
    pub shapes: usize,
    pub morph_entries: usize,
    pub meshes: usize,
    pub vertices: usize,
    pub indices: usize,
    pub gradient_materials: usize,
    pub bitmap_materials: usize,
    pub texture_bytes: usize,
    /// Logical resource payload, excluding chunk headers and unchanged baked frames.
    pub bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        animation::AnimTransform,
        baked::{BakedClip, BakedSkin, BakedSkinVariant},
    };
    use bytemuck::Zeroable;

    fn node(id: u16) -> BakedNode {
        BakedNode::Shape {
            id,
            ratio: 0,
            transform: AnimTransform::default(),
        }
    }

    fn fixture() -> (VatfBuilder, BakedMovie) {
        let mut builder = VatfBuilder::default();
        for index in 0..9 {
            builder.vertices.extend((0..3).map(|_| Vertex {
                x: index as i16,
                y: 0,
                color: crate::Color::zeroed(),
            }));
            builder.indices.extend([0, 1, 2]);
            builder.shape_meshes.push(ShapeMesh {
                vertex_count: 3,
                vertex_offset: index * 3,
                index_count: 3,
                index_offset: index * 3,
                ..Default::default()
            });
        }
        builder.shape_records = [
            (10, 1, 2),
            (20, 4, 1),
            (30, 5, 1),
            (31, 6, 1),
            (40, 0, 1),
            (41, 3, 1),
        ]
        .into_iter()
        .map(|(id, offset, count)| ShapeRecord {
            id,
            sub_shape_offset: offset,
            sub_shape_count: count,
        })
        .collect();
        builder.gradient_uniforms = vec![GradientUniforms::default(); 3];
        builder.bitmap_uniforms = vec![[0.0; 6]; 2];
        builder.texture = (0..12).collect();
        for (index, kind, material) in [
            (1, material::GRADIENT, 2),
            (2, material::BITMAP, 1),
            (3, material::GRADIENT, 1),
        ] {
            let mesh = &mut builder.shape_meshes[index];
            mesh.material_type = kind;
            mesh.material_offset = material;
            mesh.texture_offset = 4;
            mesh.texture_length = 4;
        }
        builder.morph_entries = [(1000, 7), (2000, 8)]
            .into_iter()
            .map(|(ratio, index)| MorphEntry {
                morph_id: 99,
                ratio,
                _pad: 0,
                vertex_offset: index * 3,
                vertex_count: 3,
                index_offset: index * 3,
                index_count: 3,
            })
            .collect();
        let baked = BakedMovie {
            frame_rate: 24.0,
            clips: vec![BakedClip {
                name: "default".into(),
                start_frame: 0,
                events: vec![],
                frames: vec![vec![
                    BakedNode::Mask {
                        mask: vec![node(20)],
                        children: vec![BakedNode::Group {
                            children: vec![node(10)],
                            filters: vec![],
                            blend_mode: 0,
                        }],
                    },
                    BakedNode::Skin {
                        slot: "hand".into(),
                        symbol: 50,
                        transform: AnimTransform::default(),
                    },
                    BakedNode::Shape {
                        id: 99,
                        ratio: 1000,
                        transform: AnimTransform::default(),
                    },
                ]],
            }],
            skins: vec![BakedSkin {
                symbol: 50,
                variants: vec![
                    BakedSkinVariant {
                        name: "first".into(),
                        nodes: vec![node(30)],
                    },
                    BakedSkinVariant {
                        name: "alternate".into(),
                        nodes: vec![node(31)],
                    },
                ],
            }],
        };
        (builder, baked)
    }

    #[test]
    fn compacts_dependencies_without_losing_masks_skins_or_morphs() {
        let (builder, baked) = fixture();
        let (resources, report) = Resources::compact(&builder, &baked, &[]).unwrap();
        assert_eq!((report.before.meshes, report.after.meshes), (9, 6));
        assert_eq!((resources.shapes.len(), resources.morphs.len()), (4, 1));
        assert_eq!((resources.gradients.len(), resources.bitmaps.len()), (1, 1));
        assert_eq!(resources.texture, [4, 5, 6, 7]);
        assert!(report.after.bytes < report.before.bytes);
        for (new, old) in [1, 2, 4, 5, 6, 7].into_iter().enumerate() {
            let mesh = resources.meshes[new];
            assert_eq!(mesh.vertex_offset, new as u32 * 3);
            assert_eq!(resources.vertices[new * 3].x, old);
            assert_eq!(&resources.indices[new * 3..new * 3 + 3], &[0, 1, 2]);
        }
        assert_eq!(
            resources
                .shapes
                .iter()
                .map(|s| (s.id, s.sub_shape_offset, s.sub_shape_count))
                .collect::<Vec<_>>(),
            [(10, 0, 2), (20, 2, 1), (30, 3, 1), (31, 4, 1)]
        );
        assert_eq!(resources.morphs[0].vertex_offset, 15);
        assert_eq!(resources.morphs[0].index_offset, 15);
        assert_eq!(resources.meshes[0].material_offset, 0);
        assert_eq!(resources.meshes[1].material_offset, 0);
        assert_eq!(resources.meshes[0].texture_offset, 0);
        assert_eq!(resources.meshes[1].texture_offset, 0);
        // Compaction is repeatable and leaves compiler-side source offsets untouched.
        let (_, again) = Resources::compact(&builder, &baked, &[]).unwrap();
        assert_eq!(report, again);
        assert_eq!(builder.shape_meshes[1].vertex_offset, 3);
    }

    #[test]
    fn invalid_live_references_and_ranges_are_errors() {
        let (mut builder, mut baked) = fixture();
        builder.shape_meshes[1].vertex_offset = u32::MAX;
        assert!(
            Resources::compact(&builder, &baked, &[])
                .err()
                .unwrap()
                .to_string()
                .contains("mesh 1")
        );
        baked.clips[0].frames = vec![vec![node(123)]];
        baked.skins.clear();
        assert!(
            Resources::compact(&builder, &baked, &[])
                .err()
                .unwrap()
                .to_string()
                .contains("unresolved baked shape 123")
        );
    }

    #[test]
    fn ui_frames_and_empty_shapes_are_retained_without_root_clips() {
        let (mut builder, _) = fixture();
        builder.shape_records.push(ShapeRecord {
            id: 60,
            sub_shape_offset: 9,
            sub_shape_count: 0,
        });
        let graphic = Graphic {
            name: "sparkles".into(),
            source_bounds: [0.0; 4],
            frames: vec![vec![node(30)], vec![node(31), node(60)]],
            frame_rate: 24.0,
        };
        let (resources, _) =
            Resources::compact(&builder, &BakedMovie::default(), &[graphic]).unwrap();
        assert_eq!(resources.meshes.len(), 2);
        assert_eq!(
            resources.shapes.iter().map(|s| s.id).collect::<Vec<_>>(),
            [30, 31, 60]
        );
        assert_eq!(resources.shapes[2].sub_shape_offset, 2);
        assert_eq!(resources.shapes[2].sub_shape_count, 0);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourcePruningReport {
    pub before: ResourceCounts,
    pub after: ResourceCounts,
}

#[derive(Default)]
pub(crate) struct Resources {
    pub shapes: Vec<ShapeRecord>,
    pub meshes: Vec<ShapeMesh>,
    pub gradients: Vec<GradientUniforms>,
    pub bitmaps: Vec<[f32; 6]>,
    pub texture: Vec<u8>,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub morphs: Vec<MorphEntry>,
}

#[allow(clippy::too_many_arguments)]
fn count(
    shapes: usize,
    meshes: usize,
    gradients: usize,
    bitmaps: usize,
    texture: usize,
    vertices: usize,
    indices: usize,
    morphs: usize,
) -> ResourceCounts {
    ResourceCounts {
        shapes,
        meshes,
        gradient_materials: gradients,
        bitmap_materials: bitmaps,
        texture_bytes: texture,
        vertices,
        indices,
        morph_entries: morphs,
        bytes: shapes * size_of::<ShapeRecord>()
            + meshes * size_of::<ShapeMesh>()
            + gradients * size_of::<GradientUniforms>()
            + bitmaps * size_of::<[f32; 6]>()
            + texture
            + vertices * size_of::<Vertex>()
            + indices * size_of::<u32>()
            + morphs * size_of::<MorphEntry>(),
    }
}
fn offset(len: usize) -> Result<u32> {
    len.try_into().context("resource offset exceeds u32")
}
fn range<'a, T>(data: &'a [T], start: u32, len: u32, label: &str) -> Result<&'a [T]> {
    let end = start
        .checked_add(len)
        .with_context(|| format!("{label}: range overflow"))?;
    data.get(start as usize..end as usize)
        .with_context(|| format!("{label}: range {start}..{end} exceeds {}", data.len()))
}
fn geometry(mesh: &ShapeMesh) -> (u32, u32, u32, u32) {
    (
        mesh.vertex_offset,
        mesh.vertex_count,
        mesh.index_offset,
        mesh.index_count,
    )
}
fn remap_material<T: Copy>(
    source: &[T],
    old: u32,
    destination: &mut Vec<T>,
    map: &mut HashMap<u32, u32>,
    label: &str,
) -> Result<u32> {
    if let Some(index) = map.get(&old) {
        return Ok(*index);
    }
    let value = source
        .get(old as usize)
        .with_context(|| format!("{label}: missing material {old}"))?;
    let index = offset(destination.len())?;
    destination.push(*value);
    map.insert(old, index);
    Ok(index)
}

impl Resources {
    pub(crate) fn compact(
        builder: &VatfBuilder,
        baked: &BakedMovie,
        graphics: &[Graphic],
    ) -> Result<(Self, ResourcePruningReport)> {
        // Include every clip, mask and skin variant, not just the default selection.
        fn visit(nodes: &[BakedNode], references: &mut BTreeSet<(u16, u16)>) {
            for node in nodes {
                match node {
                    BakedNode::Shape { id, ratio, .. } => {
                        references.insert((*id, *ratio));
                    }
                    BakedNode::Group { children, .. } => visit(children, references),
                    BakedNode::Mask { mask, children } => {
                        visit(mask, references);
                        visit(children, references);
                    }
                    BakedNode::Skin { .. } => {} // All variants are walked below.
                }
            }
        }
        let mut references = BTreeSet::new();
        for clip in &baked.clips {
            for frame in &clip.frames {
                visit(frame, &mut references);
            }
        }
        for skin in &baked.skins {
            for variant in &skin.variants {
                visit(&variant.nodes, &mut references);
            }
        }
        for graphic in graphics {
            for frame in &graphic.frames {
                visit(frame, &mut references);
            }
        }

        let shapes: HashMap<_, _> = builder
            .shape_records
            .iter()
            .map(|shape| (shape.id, shape))
            .collect();
        let mesh_lookup: HashMap<_, _> = builder
            .shape_meshes
            .iter()
            .enumerate()
            .map(|(index, mesh)| (geometry(mesh), index))
            .collect();
        let mut live_meshes = BTreeSet::new();
        let mut live_shapes = BTreeSet::new();
        let mut live_morphs = BTreeSet::new();
        for (id, ratio) in references {
            if let Some(shape) = shapes.get(&id) {
                range(
                    &builder.shape_meshes,
                    shape.sub_shape_offset,
                    shape.sub_shape_count.into(),
                    &format!("Shape {id}"),
                )?;
                let start = shape.sub_shape_offset as usize;
                live_meshes.extend(start..start + shape.sub_shape_count as usize);
                live_shapes.insert(id);
            } else {
                let mut found = false;
                for (index, entry) in builder
                    .morph_entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry.morph_id == id && entry.ratio == ratio)
                {
                    let key = (
                        entry.vertex_offset,
                        entry.vertex_count,
                        entry.index_offset,
                        entry.index_count,
                    );
                    let mesh = mesh_lookup.get(&key).with_context(|| {
                        format!("Morph {id} ratio {ratio}: unresolved mesh range")
                    })?;
                    live_meshes.insert(*mesh);
                    live_morphs.insert(index);
                    found = true;
                }
                ensure!(found, "unresolved baked shape {id} ratio {ratio}");
            }
        }
        let mut result = Self::default();
        let mut mesh_map = vec![None; builder.shape_meshes.len()];
        let mut gradient_map = HashMap::new();
        let mut bitmap_map = HashMap::new();
        let mut texture_map = HashMap::new();
        // Preserve original mesh order so each Shape's sub-meshes remain contiguous.
        for old in &live_meshes {
            let mut mesh = builder.shape_meshes[*old];
            let label = format!("mesh {old}");
            let vertices = range(
                &builder.vertices,
                mesh.vertex_offset,
                mesh.vertex_count,
                &label,
            )?;
            let indices = range(
                &builder.indices,
                mesh.index_offset,
                mesh.index_count,
                &label,
            )?;
            ensure!(
                indices.iter().all(|index| *index < mesh.vertex_count),
                "{label}: index exceeds vertex count"
            );
            mesh.vertex_offset = offset(result.vertices.len())?;
            mesh.index_offset = offset(result.indices.len())?;
            result.vertices.extend_from_slice(vertices);
            result.indices.extend_from_slice(indices); // Indices are mesh-local, not global.
            match mesh.material_type {
                material::COLOR => {}
                material::GRADIENT => {
                    mesh.material_offset = remap_material(
                        &builder.gradient_uniforms,
                        mesh.material_offset,
                        &mut result.gradients,
                        &mut gradient_map,
                        &label,
                    )?
                }
                material::BITMAP => {
                    mesh.material_offset = remap_material(
                        &builder.bitmap_uniforms,
                        mesh.material_offset,
                        &mut result.bitmaps,
                        &mut bitmap_map,
                        &label,
                    )?
                }
                value => anyhow::bail!("{label}: unknown material type {value}"),
            }
            if mesh.material_type != material::COLOR {
                let key = (mesh.texture_offset, mesh.texture_length);
                mesh.texture_offset = if let Some(new) = texture_map.get(&key) {
                    *new
                } else {
                    let bytes = range(&builder.texture, key.0, key.1, &label)?;
                    let new = offset(result.texture.len())?;
                    result.texture.extend_from_slice(bytes);
                    texture_map.insert(key, new);
                    new
                };
            }
            mesh_map[*old] = Some(offset(result.meshes.len())?);
            result.meshes.push(mesh);
        }
        for shape in &builder.shape_records {
            if !live_shapes.contains(&shape.id) {
                continue;
            }
            let mut shape = *shape;
            // Zero-mesh shapes still resolve by id and intentionally draw nothing.
            shape.sub_shape_offset =
                offset(live_meshes.range(..shape.sub_shape_offset as usize).count())?;
            result.shapes.push(shape);
        }
        for old in live_morphs {
            let mut entry = builder.morph_entries[old];
            let key = (
                entry.vertex_offset,
                entry.vertex_count,
                entry.index_offset,
                entry.index_count,
            );
            let source = mesh_lookup[&key]; // Validated when marking the morph live.
            let index = mesh_map[source].context("live morph mesh was not copied")?;
            let mesh = result.meshes[index as usize];
            entry.vertex_offset = mesh.vertex_offset;
            entry.index_offset = mesh.index_offset;
            result.morphs.push(entry);
        }
        let report = ResourcePruningReport {
            before: count(
                builder.shape_records.len(),
                builder.shape_meshes.len(),
                builder.gradient_uniforms.len(),
                builder.bitmap_uniforms.len(),
                builder.texture.len(),
                builder.vertices.len(),
                builder.indices.len(),
                builder.morph_entries.len(),
            ),
            after: count(
                result.shapes.len(),
                result.meshes.len(),
                result.gradients.len(),
                result.bitmaps.len(),
                result.texture.len(),
                result.vertices.len(),
                result.indices.len(),
                result.morphs.len(),
            ),
        };
        Ok((result, report))
    }
}
