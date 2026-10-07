pub mod animation;
pub mod baked;
mod bitmap;
mod compiler;
pub use compiler::{
    COMPILER_REVISION, CompiledVab, RootTranslationPolicy, SwfCompileMode, SwfCompileSettings,
    compile_swf, convert_swf,
};
mod decoder;
pub mod filter;
pub mod graphics;
mod matrix;
mod morph;
mod pruning;
pub use pruning::{ResourceCounts, ResourcePruningReport};
pub mod reader;
mod shape_utils;
mod tessellator;
pub mod transform;

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufReader, Cursor},
    mem,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use bytemuck::{Pod, Zeroable};
use image::{ImageBuffer, ImageFormat, Rgba};
use swf::{
    BlendMode, CharacterId, Depth, GradientInterpolation, GradientSpread, PlaceObject, Shape, Tag,
    UTF_8, decompress_swf, parse_swf,
};
use tracing::{error, info};

use crate::{
    bitmap::CompressedBitmap,
    decoder::{decode_define_bits_jpeg_dimensions, glue_tables_to_jpeg, remove_invalid_jpeg_data},
    filter::Filter,
    shape_utils::GradientType,
    tessellator::{DrawType, Gradient, ShapeTessellator},
    transform::Transform,
};

// ===========================================================================
// VAB format constants
// ===========================================================================

pub const MAGIC_BYTES: &[u8; 4] = b"VATF";

/// Layout generation of the container.
///
/// This is a **stale-file detector, not a compatibility policy** — the reader
/// requires an exact match, and there is no migration path. Bump it whenever the
/// `BAKD` schema or a POD struct's layout changes, so that files left over from
/// an older build fail with a clear message instead of being silently
/// misparsed. Re-converting the source `.swf` is the only remedy either way.
pub const VAB_VERSION: u32 = 1;

#[cfg(test)]
mod texture_tests {
    #[test]
    fn repeated_texture_payload_is_stored_once() {
        let mut builder = super::VatfBuilder::default();
        let first = builder.intern_texture(&[1, 2, 3]);
        assert_eq!(first, builder.intern_texture(&[1, 2, 3]));
        assert_ne!(first, builder.intern_texture(&[4, 5, 6]));
        assert_eq!(builder.texture.len(), 6);
    }
}

pub(crate) const CHUNK_HEADER_SIZE: usize = mem::size_of::<ChunkHeader>();

// ===========================================================================
// File format types — #[repr(C)] Pod structs for bytemuck zero-copy
// ===========================================================================

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Header {
    pub version: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable)]
pub struct ChunkHeader {
    pub chunk_type: [u8; 4],
    pub length: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub x: i16,
    pub y: i16,
    pub color: Color,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ShapeRecord {
    pub id: u16,
    pub sub_shape_count: u16,
    pub sub_shape_offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
pub struct ShapeMesh {
    pub vertex_count: u32,
    pub vertex_offset: u32,
    pub index_count: u32,
    pub index_offset: u32,
    pub material_type: u16,
    pub sampler_flags: u16,
    pub texture_offset: u32,
    pub texture_length: u32,
    pub material_offset: u32,
    /// Bounding-box half-extents for vertex unquantization.
    /// Player: `local_x = q_x / 32767 * b_half_x + bounds_center_x`
    pub bounds_half_x: f32,
    pub bounds_half_y: f32,
    pub bounds_center_x: f32,
    pub bounds_center_y: f32,
}

/// Material type discriminants used in `ShapeMesh::material_type`.
/// `ShapeMesh::material_type` discriminants.
pub mod material {
    pub const COLOR: u16 = 0;
    pub const GRADIENT: u16 = 1;
    pub const BITMAP: u16 = 2;
}

/// A pre-interpolated morph frame — maps a (morph_id, ratio) pair to
/// a single mesh entry in the VERT / INDX chunks.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct MorphEntry {
    pub morph_id: u16,
    pub ratio: u16,
    _pad: u32,
    pub vertex_offset: u32,
    pub vertex_count: u32,
    pub index_offset: u32,
    pub index_count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct GradientUniforms {
    pub focal_point: f32,
    pub interpolation: i32,
    pub shape: i32,
    pub repeat: i32,
    pub texture_transform: [f32; 6],
}

impl From<Gradient> for GradientUniforms {
    fn from(gradient: Gradient) -> Self {
        Self {
            focal_point: gradient.focal_point.to_f32().clamp(-0.98, 0.98),
            interpolation: (gradient.interpolation == GradientInterpolation::LinearRgb) as i32,
            shape: match gradient.gradient_type {
                GradientType::Linear => 1,
                GradientType::Radial => 2,
                GradientType::Focal => 3,
            },
            repeat: match gradient.repeat_mode {
                GradientSpread::Pad => 1,
                GradientSpread::Reflect => 2,
                GradientSpread::Repeat => 3,
            },
            texture_transform: [0.0; 6],
        }
    }
}

// ===========================================================================
// Vertex quantization
// ===========================================================================

/// Normalize a shape-local vertex into the i16 range [-32767, 32767].
///
/// The quantized coordinate is relative to the shape's bounding box:
/// `(-32767, -32767)` = bounds_min corner, `(32767, 32767)` = bounds_max corner.
fn quantize_vertex(x: f32, y: f32, bounds_min: (f32, f32), bounds_max: (f32, f32)) -> (i16, i16) {
    let center_x = (bounds_max.0 + bounds_min.0) * 0.5;
    let center_y = (bounds_max.1 + bounds_min.1) * 0.5;
    let half_x = (bounds_max.0 - bounds_min.0) * 0.5;
    let half_y = (bounds_max.1 - bounds_min.1) * 0.5;

    let q_x = if half_x > 0.0 {
        ((x - center_x) / half_x) * 32767.0
    } else {
        0.0
    };
    let q_y = if half_y > 0.0 {
        ((y - center_y) / half_y) * 32767.0
    } else {
        0.0
    };

    (
        q_x.round().clamp(-32767.0, 32767.0) as i16,
        q_y.round().clamp(-32767.0, 32767.0) as i16,
    )
}

// ===========================================================================
// Animation types
// ===========================================================================

/// A display-object entry on the stage at a given frame and depth.
///
/// This is the baked (non-recordable) representation: all fields are resolved
/// from the original PlaceObject/RemoveObject stream at conversion time.
#[derive(Clone, Default)]
pub struct DisplayObject {
    pub id: CharacterId,
    pub name: Option<Box<str>>,
    pub depth: Depth,
    pub clip_depth: Depth,
    pub blend_mode: BlendMode,
    pub transform: Transform,
    pub filters: Box<[Filter]>,
    pub ratio: u16,
    /// Index (0-based) of the parent timeline frame where this instance was placed.
    pub place_frame: u32,
}

/// Embedded SWF outline data needed to flatten `DefineText` into ordinary
/// shapes. Static text addresses glyphs by index, so character mappings and
/// layout metrics are unnecessary here.
struct StaticFont {
    scale: f32,
    glyphs: Vec<Vec<swf::ShapeRecord>>,
}

impl DisplayObject {
    fn new(id: CharacterId) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    fn apply_place_object(&mut self, place_object: &PlaceObject) {
        self.depth = place_object.depth;
        if let Some(name) = place_object.name {
            self.name = Some(name.to_str_lossy(UTF_8).into());
        }
        if let Some(clip_depth) = place_object.clip_depth {
            self.clip_depth = clip_depth;
        }
        if let Some(matrix) = place_object.matrix {
            self.transform.matrix = matrix.into();
        }
        if let Some(color_transform) = place_object.color_transform {
            self.transform.color_transform = color_transform;
        }
        if let Some(ratio) = place_object.ratio {
            self.ratio = ratio
        }
        if let Some(blend_mode) = place_object.blend_mode {
            self.blend_mode = blend_mode;
        }
        if let Some(filters) = &place_object.filters {
            self.filters = filters.iter().map(Filter::from).collect();
        }
    }
}

// ===========================================================================
// VAB builder — collects and writes all data
// ===========================================================================

#[derive(Default)]
pub struct VatfBuilder {
    pub root_translation: RootTranslationPolicy,
    pub graphics: Option<Vec<graphics::Graphic>>,
    pub buttons: Vec<graphics::Button>,
    pub shape_records: Vec<ShapeRecord>,
    pub shape_meshes: Vec<ShapeMesh>,
    pub gradient_uniforms: Vec<GradientUniforms>,
    pub bitmap_uniforms: Vec<[f32; 6]>,
    pub texture: Vec<u8>,
    pub texture_lookup: HashMap<Vec<u8>, (u32, u32)>,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub frame_labels: HashMap<Box<str>, usize>,
    pub event_labels: Vec<(Box<str>, usize)>,
    /// Named frames on non-root timelines. They become variants only when the
    /// sprite is placed through a `skin_<slot>` instance.
    pub skin_variants: HashMap<CharacterId, Vec<(Box<str>, usize)>>,
    pub animations: HashMap<CharacterId, Vec<Vec<DisplayObject>>>,
    /// Frame rate of the source SWF (frames per second), written into the ANIM chunk.
    pub frame_rate: f32,
    /// Raw morph shape data collected during tag parsing.
    pub morph_shapes: HashMap<CharacterId, swf::DefineMorphShape>,
    /// Pre-interpolated morph frame entries (built by `process_morphs`).
    pub morph_entries: Vec<MorphEntry>,
    /// Fonts and static text are compiler-only inputs. `process_static_texts`
    /// turns them into synthetic shape timelines before BAKD is written.
    static_fonts: HashMap<CharacterId, StaticFont>,
    static_texts: HashMap<CharacterId, swf::Text>,
}

impl VatfBuilder {
    fn intern_texture(&mut self, bytes: &[u8]) -> (u32, u32) {
        if let Some(entry) = self.texture_lookup.get(bytes) {
            return *entry;
        }
        let entry = (self.texture.len() as u32, bytes.len() as u32);
        self.texture.extend_from_slice(bytes);
        self.texture_lookup.insert(bytes.to_vec(), entry);
        entry
    }
    /// Tessellate an SWF shape and store its geometry in the builder.
    ///
    /// Returns `(mesh_start, mesh_count)`. Does **not** emit a `ShapeRecord`;
    /// use [`Self::process_swf_shape`] when the shape needs one.
    pub fn process_shape_geometry(
        &mut self,
        shape: &Shape,
        bitmap: &HashMap<CharacterId, CompressedBitmap>,
    ) -> (u32, u32) {
        let mut tessellator = ShapeTessellator::default();
        let lyon_mesh = tessellator.tessellate_shape(shape.into(), bitmap);
        // EdgeBounds excludes stroke expansion. Include actual tessellated vertices,
        // including caps and miter joins, so i16 quantization cannot flatten them.
        let mut bounds = (
            shape.shape_bounds.x_min.to_pixels() as f32,
            shape.shape_bounds.y_min.to_pixels() as f32,
            shape.shape_bounds.x_max.to_pixels() as f32,
            shape.shape_bounds.y_max.to_pixels() as f32,
        );
        for vertex in lyon_mesh.draws.iter().flat_map(|draw| &draw.vertices) {
            bounds.0 = bounds.0.min(vertex.x);
            bounds.1 = bounds.1.min(vertex.y);
            bounds.2 = bounds.2.max(vertex.x);
            bounds.3 = bounds.3.max(vertex.y);
        }
        let bounds_min = (bounds.0, bounds.1);
        let bounds_max = (bounds.2, bounds.3);

        // Pre-compute vertex unquantization params for each ShapeMesh.
        let b_half_x = (bounds_max.0 - bounds_min.0) * 0.5;
        let b_half_y = (bounds_max.1 - bounds_min.1) * 0.5;
        let b_center_x = (bounds_max.0 + bounds_min.0) * 0.5;
        let b_center_y = (bounds_max.1 + bounds_min.1) * 0.5;

        // Pre-encode gradient textures (WebP compressed).
        let gradients: Vec<_> = lyon_mesh
            .gradients
            .into_iter()
            .map(|gradient| {
                let webp_data = encode_gradient_as_webp(&gradient);
                (webp_data, GradientUniforms::from(gradient))
            })
            .collect();

        let mesh_start = self.shape_meshes.len() as u32;

        for draw in lyon_mesh.draws {
            let vertex_offset = self.vertices.len() as u32;
            let index_offset = self.indices.len() as u32;
            let vertex_count = draw.vertices.len() as u32;
            let index_count = draw.indices.len() as u32;

            for vertex in draw.vertices {
                let (x, y) = quantize_vertex(vertex.x, vertex.y, bounds_min, bounds_max);
                self.vertices.push(Vertex {
                    x,
                    y,
                    color: Color {
                        r: vertex.color.r,
                        g: vertex.color.g,
                        b: vertex.color.b,
                        a: vertex.color.a,
                    },
                });
            }
            self.indices.extend(draw.indices);

            match draw.draw_type {
                DrawType::Color => {
                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: material::COLOR,
                        sampler_flags: 0,
                        texture_offset: 0,
                        texture_length: 0,
                        material_offset: 0,
                        bounds_half_x: b_half_x,
                        bounds_half_y: b_half_y,
                        bounds_center_x: b_center_x,
                        bounds_center_y: b_center_y,
                    });
                }
                DrawType::Gradient { matrix, gradient } => {
                    let Some((gradient, gradient_uniforms)) = gradients.get(gradient) else {
                        continue;
                    };
                    let mut gradient_uniforms = *gradient_uniforms;
                    gradient_uniforms.texture_transform = flatten_matrix_3x3_to_6(matrix);

                    let material_offset = self.gradient_uniforms.len() as u32;
                    self.gradient_uniforms.push(gradient_uniforms);

                    let (texture_offset, texture_length) = self.intern_texture(gradient);

                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: material::GRADIENT,
                        sampler_flags: 0,
                        texture_offset,
                        texture_length,
                        material_offset,
                        bounds_half_x: b_half_x,
                        bounds_half_y: b_half_y,
                        bounds_center_x: b_center_x,
                        bounds_center_y: b_center_y,
                    });
                }
                DrawType::Bitmap(bm) => {
                    let Some(compressed_bitmap) = bitmap.get(&bm.bitmap_id) else {
                        continue;
                    };
                    let decoded = match compressed_bitmap.decode() {
                        Ok(decoded) => decoded,
                        Err(e) => {
                            error!("Failed to decode bitmap: {:?}", e);
                            continue;
                        }
                    };
                    let bitmap_rgba = decoded.into_rgba();
                    let w = bitmap_rgba.width();
                    let h = bitmap_rgba.height();

                    // Compress bitmap as WebP — consistent with gradient textures.
                    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                        ImageBuffer::from_raw(w, h, bitmap_rgba.data().to_vec())
                            .expect("Bitmap dimensions must match RGBA data");
                    let mut webp_buf = Cursor::new(Vec::new());
                    img.write_to(&mut webp_buf, ImageFormat::WebP).unwrap();
                    let webp_bytes = webp_buf.into_inner();

                    let (texture_offset, texture_length) = self.intern_texture(&webp_bytes);

                    let material_offset = self.bitmap_uniforms.len() as u32;
                    self.bitmap_uniforms
                        .push(flatten_matrix_3x3_to_6(bm.matrix));

                    self.shape_meshes.push(ShapeMesh {
                        vertex_count,
                        vertex_offset,
                        index_count,
                        index_offset,
                        material_type: material::BITMAP,
                        sampler_flags: u16::from(bm.is_smoothed)
                            | (u16::from(bm.is_repeating) << 1),
                        texture_offset,
                        texture_length,
                        material_offset,
                        bounds_half_x: b_half_x,
                        bounds_half_y: b_half_y,
                        bounds_center_x: b_center_x,
                        bounds_center_y: b_center_y,
                    });
                }
            }
        }

        (mesh_start, self.shape_meshes.len() as u32 - mesh_start)
    }

    /// Tessellate an SWF shape and register its `ShapeRecord` (shape id → mesh range).
    pub fn process_swf_shape(
        &mut self,
        shape: &Shape,
        bitmap: &HashMap<CharacterId, CompressedBitmap>,
    ) {
        let (mesh_start, mesh_count) = self.process_shape_geometry(shape, bitmap);
        self.shape_records.push(ShapeRecord {
            id: shape.id,
            sub_shape_count: mesh_count as u16,
            sub_shape_offset: mesh_start,
        });
    }

    /// Serialise all collected data into a compressed .vab file.
    pub fn write_vatf(&mut self, path: PathBuf) -> Result<()> {
        self.write_vatf_with_report(path).map(|_| ())
    }

    /// Write compact runtime resources without mutating compiler-side source data.
    pub fn write_vatf_with_report(&mut self, path: PathBuf) -> Result<ResourcePruningReport> {
        let (bytes, report) = self.to_vab_bytes()?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(report)
    }

    /// Serialize compact resources without altering source timelines or offsets.
    pub fn to_vab_bytes(&self) -> Result<(Vec<u8>, ResourcePruningReport)> {
        if cfg!(target_endian = "big") {
            bail!("VATF POD chunks require a little-endian host");
        }

        // -----------------------------------------------------------------------
        // 1. Define chunks (FourCC + raw byte slice)
        // -----------------------------------------------------------------------
        type ChunkSpec<'a> = (&'a [u8; 4], &'a [u8]);

        let container = animation::AnimContainer::from_parts(
            &self.animations,
            &self.frame_labels,
            self.frame_rate,
        );
        let baked = if self.graphics.is_some() {
            baked::BakedMovie::default()
        } else {
            baked::bake_with_options(
                &container,
                &self.event_labels,
                &self.skin_variants,
                self.root_translation,
            )?
        };
        baked.validate()?;
        let (resources, report) = pruning::Resources::compact(
            self,
            &baked,
            self.graphics.as_deref().unwrap_or_default(),
        )?;
        let shape_records = bytemuck::cast_slice(&resources.shapes);
        let shape_meshes = bytemuck::cast_slice(&resources.meshes);
        let gradient_uniforms = bytemuck::cast_slice(&resources.gradients);
        let bitmap_uniforms = bytemuck::cast_slice(&resources.bitmaps);
        let texture: &[u8] = &resources.texture;
        let vertices = bytemuck::cast_slice(&resources.vertices);
        let indices = bytemuck::cast_slice(&resources.indices);
        let morph_bytes: &[u8] = bytemuck::cast_slice(&resources.morphs);
        let baked_bytes = bincode::serialize(&baked)?;
        let graphic_bytes = self.graphics.as_ref().map(bincode::serialize).transpose()?;
        let button_bytes = if self.buttons.is_empty() {
            None
        } else {
            Some(bincode::serialize(&self.buttons)?)
        };
        let mut chunks: Vec<ChunkSpec> = vec![
            (b"BAKD", &baked_bytes),
            (b"SHAP", shape_records),
            (b"SHME", shape_meshes),
            (b"GRAD", gradient_uniforms),
            (b"BMAP", bitmap_uniforms),
            (b"TEXT", texture),
            (b"VERT", vertices),
            (b"INDX", indices),
            (b"MORP", morph_bytes),
        ];

        // -----------------------------------------------------------------------
        // 2. Compute sizes and write file header
        // -----------------------------------------------------------------------
        if let Some(bytes) = &graphic_bytes {
            chunks.push((b"UIGR", bytes));
        }
        if let Some(bytes) = &button_bytes {
            chunks.push((b"UIBT", bytes));
        }
        let payload_size: u32 = chunks
            .iter()
            .map(|(_, data)| data.len() as u32 + CHUNK_HEADER_SIZE as u32)
            .sum();

        let file_header = Header {
            version: VAB_VERSION,
            length: MAGIC_BYTES.len() as u32 + mem::size_of::<Header>() as u32 + payload_size,
        };

        // -----------------------------------------------------------------------
        // 3. Assemble and write payload
        // -----------------------------------------------------------------------
        let mut raw_payload = Vec::with_capacity(file_header.length as usize);
        raw_payload.extend(MAGIC_BYTES);
        raw_payload.extend(bytemuck::bytes_of(&file_header));
        for (four_cc, data) in &chunks {
            let chunk_header = ChunkHeader {
                chunk_type: **four_cc,
                length: data.len() as u32,
            };
            raw_payload.extend(bytemuck::bytes_of(&chunk_header));
            raw_payload.extend(*data);
        }

        Ok((raw_payload, report))
    }
}

/// Encode a gradient colour ramp as a 256×1 WebP texture.
fn encode_gradient_as_webp(gradient: &Gradient) -> Vec<u8> {
    let color = gradient.compute_gradient_color(256);
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(256, 1, color).unwrap();
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::WebP).unwrap();
    buf.into_inner()
}

/// Convert a 3×3 matrix (as [[f32; 3]; 3]) into the 6-element affine slice
/// `[a, c, tx, b, d, ty]` used by shader uniforms.
fn flatten_matrix_3x3_to_6(m: [[f32; 3]; 3]) -> [f32; 6] {
    [m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1]]
}

// ===========================================================================
// Conversion entry-point
// ===========================================================================

/// Parse a `.swf` into a fully populated [`VatfBuilder`] — shapes tessellated,
/// morphs interpolated, every sprite timeline collected.
///
/// Shared by [`convert_swf_to_vab`] and [`parse_animation_container`] so that the
/// parsed timeline data is reachable in-process, without going through a file.
fn build_builder(input: &Path) -> Result<VatfBuilder> {
    ensure!(
        input.extension().and_then(|e| e.to_str()) == Some("swf"),
        "Not a .swf file: {}",
        input.display()
    );
    let file = File::open(input).with_context(|| format!("Failed to open {}", input.display()))?;
    build_builder_reader(BufReader::new(file), &SwfCompileSettings::default())
}

fn build_builder_reader(
    reader: impl std::io::Read,
    settings: &SwfCompileSettings,
) -> Result<VatfBuilder> {
    let ui = settings.mode != SwfCompileMode::Animation;
    let animated = settings.mode == SwfCompileMode::AnimatedUi;
    let swf_buf = decompress_swf(reader)?;
    let swf = parse_swf(&swf_buf)?;
    let ui_sources = if ui {
        Some(graphics::Sources::collect(&swf.tags, animated)?)
    } else {
        None
    };
    let frame_rate = swf.header.frame_rate().to_f32();

    let mut jpeg_tables: Option<Vec<u8>> = None;
    let mut bitmap = HashMap::new();
    let mut builder = VatfBuilder {
        root_translation: settings.root_translation,
        frame_rate,
        ..Default::default()
    };
    let mut animations = HashMap::default();

    let tags = if let Some(source) = &ui_sources {
        source.select(swf.tags)?
    } else {
        swf.tags
    };
    parse_tags(
        tags,
        &mut builder,
        &mut animations,
        0,
        &mut bitmap,
        &mut jpeg_tables,
    )?;

    builder.animations = animations;

    // DefineText is a display character, not a shape. Flatten its embedded
    // glyph outlines into ordinary synthetic shapes and a one-frame timeline
    // so the runtime remains entirely unaware of fonts and text records.
    process_static_texts(&mut builder, &bitmap)?;

    // Process morph shapes: interpolate + tessellate at each unique ratio.
    process_morphs(&mut builder, &bitmap)?;

    if let Some(source) = ui_sources {
        builder.buttons = source.buttons().to_vec();
        builder.graphics = Some(source.compile(&builder)?);
    }
    Ok(builder)
}

/// Parse a `.swf` and return its (unexpanded) animation timelines.
///
/// `.vab` no longer stores this data — the runtime reads the fully expanded
/// `BAKD` tree instead. This exists so that tests can compare the baked output
/// against an independently derived reference without re-deriving the expansion
/// (see `tests/swf_oracle.rs`).
pub fn parse_animation_container(input: &Path) -> Result<animation::AnimContainer> {
    let builder = build_builder(input)?;
    Ok(animation::AnimContainer::from_parts(
        &builder.animations,
        &builder.frame_labels,
        builder.frame_rate,
    ))
}

/// Convert a single `.swf` file into a `.vab` file at the given output path.
///
/// `output` should include the `.vab` filename (e.g. `"out/anim.vab"`).
pub fn convert_swf_to_vab(input: &Path, output: &Path) -> Result<()> {
    convert_swf_to_vab_with_report(input, output).map(|_| ())
}

pub fn convert_swf_to_vab_with_report(
    input: &Path,
    output: &Path,
) -> Result<ResourcePruningReport> {
    convert_swf(input, output, &SwfCompileSettings::default())
}

// ===========================================================================
// SWF tag processing
// ===========================================================================

/// Recursively process SWF tags, collecting geometry into `builder` and
/// frame timelines into `animations`.
fn parse_tags(
    tags: Vec<Tag<'_>>,
    builder: &mut VatfBuilder,
    animations: &mut HashMap<CharacterId, Vec<Vec<DisplayObject>>>,
    sprite_id: CharacterId,
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
    jpeg_tables: &mut Option<Vec<u8>>,
) -> Result<()> {
    // BTreeMap used during frame construction for sorted insert/lookup/remove
    // by depth. Cloned into a Vec at each ShowFrame — depth ordering is preserved.
    // Objects persist across frames until an explicit RemoveObject, so the map
    // is *not* cleared at frame boundaries.
    let mut current_frame: BTreeMap<Depth, DisplayObject> = BTreeMap::new();
    let mut timeline: Vec<Vec<DisplayObject>> = Vec::new();

    for tag in tags {
        match tag {
            Tag::DefineShape(shape) => {
                builder.process_swf_shape(&shape, bitmap);
            }
            Tag::DefineMorphShape(morph_data) => {
                builder.morph_shapes.insert(morph_data.id, *morph_data);
            }

            // -- Static text resources ------------------------------------------
            Tag::DefineFont(font) => {
                builder.static_fonts.insert(
                    font.id,
                    StaticFont {
                        scale: 1024.0,
                        glyphs: font.glyphs,
                    },
                );
            }
            Tag::DefineFont2(font) => {
                let scale = if font.version >= 3 { 20480.0 } else { 1024.0 };
                builder.static_fonts.insert(
                    font.id,
                    StaticFont {
                        scale,
                        glyphs: font
                            .glyphs
                            .into_iter()
                            .map(|glyph| glyph.shape_records)
                            .collect(),
                    },
                );
            }
            Tag::DefineText(text) | Tag::DefineText2(text) => {
                builder.static_texts.insert(text.id, *text);
            }

            // -- Bitmap resources --------------------------------------------------
            Tag::JpegTables(data) => register_jpeg_tables(&mut *jpeg_tables, data),
            Tag::DefineBits { id, jpeg_data } => {
                define_bits_jpeg(id, jpeg_data, jpeg_tables.as_deref(), bitmap)?;
            }
            Tag::DefineBitsJpeg2 { id, jpeg_data } => {
                define_bits_jpeg2(id, jpeg_data, bitmap)?;
            }
            Tag::DefineBitsJpeg3(data) => {
                define_bits_jpeg3(data, bitmap)?;
            }
            Tag::DefineBitsLossless(data) => {
                define_bits_lossless(data, bitmap);
            }

            // -- Sprite hierarchy --------------------------------------------------
            Tag::DefineSprite(sprite) => {
                parse_tags(
                    sprite.tags,
                    builder,
                    animations,
                    sprite.id,
                    bitmap,
                    jpeg_tables,
                )?;
            }

            // -- Frame construction ------------------------------------------------
            Tag::PlaceObject(place_object) => match place_object.action {
                swf::PlaceObjectAction::Place(id) => {
                    let mut obj = DisplayObject::new(id);
                    obj.apply_place_object(&place_object);
                    obj.place_frame = timeline.len() as u32;
                    current_frame.insert(place_object.depth, obj);
                }
                swf::PlaceObjectAction::Modify => {
                    if let Some(child) = current_frame.get_mut(&place_object.depth) {
                        child.apply_place_object(&place_object);
                    }
                }
                swf::PlaceObjectAction::Replace(id) => {
                    if let Some(child) = current_frame.get_mut(&place_object.depth) {
                        child.id = id;
                        child.apply_place_object(&place_object);
                        // A replaced instance restarts its own timeline.
                        child.place_frame = timeline.len() as u32;
                    }
                }
            },
            Tag::RemoveObject(remove_object) => {
                current_frame.remove(&remove_object.depth);
            }

            // -- Frame boundary ---------------------------------------------------
            Tag::ShowFrame => {
                // Full display list for this frame: every object still on stage,
                // in ascending depth order (BTreeMap iteration order = SWF paint order).
                timeline.push(current_frame.values().cloned().collect());
            }

            // -- Meta -------------------------------------------------------------
            Tag::FrameLabel(label) => {
                let name: Box<str> = label.label.to_str_lossy(UTF_8).into();
                if sprite_id == 0 {
                    if name.starts_with("event_") {
                        builder.event_labels.push((name, timeline.len()));
                    } else if builder
                        .frame_labels
                        .insert(name.clone(), timeline.len())
                        .is_some()
                        && name.starts_with("anim_")
                    {
                        bail!("duplicate animation label {name}");
                    }
                } else {
                    ensure!(!name.is_empty(), "empty frame label on sprite {sprite_id}");
                    let variants = builder.skin_variants.entry(sprite_id).or_default();
                    ensure!(
                        variants.iter().all(|(variant, _)| variant != &name),
                        "duplicate frame label {name} on sprite {sprite_id}"
                    );
                    ensure!(
                        variants
                            .last()
                            .is_none_or(|(_, frame)| *frame != timeline.len()),
                        "multiple frame labels on sprite {sprite_id} frame {}",
                        timeline.len()
                    );
                    variants.push((name, timeline.len()));
                }
            }

            _ => {}
        }
    }

    animations.insert(sprite_id, timeline);
    Ok(())
}

// ===========================================================================
// Static text processing
// ===========================================================================

/// Convert each `DefineText`/`DefineText2` character into a one-frame sprite
/// containing one synthetic colored shape per glyph.
///
/// This follows Ruffle's static-text transform order:
/// `text_matrix * translate(pen) * scale(text_height / font_scale)`.
fn process_static_texts(
    builder: &mut VatfBuilder,
    bitmap: &HashMap<CharacterId, CompressedBitmap>,
) -> Result<()> {
    if builder.static_texts.is_empty() {
        return Ok(());
    }

    let max_used_id = builder
        .shape_records
        .iter()
        .map(|shape| shape.id)
        .chain(builder.morph_shapes.keys().copied())
        .chain(builder.animations.keys().copied())
        .chain(
            builder
                .animations
                .values()
                .flat_map(|frames| frames.iter().flatten().map(|object| object.id)),
        )
        .chain(builder.static_fonts.keys().copied())
        .chain(builder.static_texts.keys().copied())
        .max()
        .unwrap_or(0);
    let mut next_id = max_used_id
        .checked_add(1)
        .context("no character ids remain for static text glyphs")?;

    // HashMap iteration must not influence synthetic ids or mesh ordering.
    let mut texts: Vec<_> = mem::take(&mut builder.static_texts).into_iter().collect();
    texts.sort_by_key(|(id, _)| *id);

    for (text_id, text) in texts {
        let text_matrix: matrix::Matrix = text.matrix.into();
        let mut color = swf::Color::TRANSPARENT;
        let mut font_id = 0;
        let mut height = swf::Twips::ZERO;
        let mut pen_x = swf::Twips::ZERO;
        let mut pen_y = swf::Twips::ZERO;
        let mut objects = Vec::new();

        for record in text.records {
            if let Some(x) = record.x_offset {
                pen_x = x;
            }
            if let Some(y) = record.y_offset {
                pen_y = y;
            }
            color = record.color.unwrap_or(color);
            font_id = record.font_id.unwrap_or(font_id);
            height = record.height.unwrap_or(height);

            let font_scale = builder
                .static_fonts
                .get(&font_id)
                .with_context(|| format!("text {text_id}: missing font {font_id}"))?
                .scale;
            ensure!(
                font_scale.is_finite() && font_scale > 0.0,
                "text {text_id}: invalid font {font_id} scale"
            );
            let scale = height.get() as f32 / font_scale;

            for glyph in record.glyphs {
                let records = builder
                    .static_fonts
                    .get(&font_id)
                    .expect("font was resolved above")
                    .glyphs
                    .get(glyph.index as usize)
                    .with_context(|| {
                        format!(
                            "text {text_id}: glyph {} outside font {font_id}",
                            glyph.index
                        )
                    })?
                    .clone();
                if !records.is_empty() {
                    let bounds = shape_utils::calculate_shape_bounds(&records);
                    let shape = swf::Shape {
                        version: 4,
                        id: next_id,
                        shape_bounds: bounds.clone(),
                        edge_bounds: bounds,
                        flags: swf::ShapeFlag::empty(),
                        styles: swf::ShapeStyles {
                            fill_styles: vec![swf::FillStyle::Color(color)],
                            line_styles: Vec::new(),
                        },
                        shape: records,
                    };
                    builder.process_swf_shape(&shape, bitmap);

                    ensure!(
                        objects.len() < u16::MAX as usize,
                        "text {text_id}: too many glyphs"
                    );
                    let glyph_matrix = matrix::Matrix::translate(pen_x, pen_y)
                        * matrix::Matrix::scale(scale, scale);
                    objects.push(DisplayObject {
                        id: next_id,
                        depth: objects.len() as u16 + 1,
                        transform: Transform {
                            matrix: text_matrix * glyph_matrix,
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    next_id = next_id
                        .checked_add(1)
                        .context("no character ids remain for static text glyphs")?;
                }
                pen_x += swf::Twips::new(glyph.advance);
            }
        }

        ensure!(
            builder.animations.insert(text_id, vec![objects]).is_none(),
            "text {text_id} collides with a sprite timeline"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bitmap tag handlers
// ---------------------------------------------------------------------------

fn register_jpeg_tables(jpeg_tables: &mut Option<Vec<u8>>, data: &[u8]) {
    if jpeg_tables.is_some() {
        eprintln!("SWF contains multiple JPEGTables tags");
    } else {
        *jpeg_tables = if data.is_empty() {
            None
        } else {
            Some(remove_invalid_jpeg_data(data).into_owned())
        };
    }
}

fn define_bits_jpeg(
    id: u16,
    jpeg_data: &[u8],
    jpeg_tables: Option<&[u8]>,
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
) -> Result<()> {
    let jpeg_data = glue_tables_to_jpeg(jpeg_data, jpeg_tables).into_owned();
    let (width, height) = decode_define_bits_jpeg_dimensions(&jpeg_data)?;
    bitmap.insert(
        id,
        CompressedBitmap::Jpeg {
            data: jpeg_data,
            alpha: None,
            width,
            height,
        },
    );
    Ok(())
}

fn define_bits_jpeg2(
    id: u16,
    jpeg_data: &[u8],
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
) -> Result<()> {
    let (width, height) = decode_define_bits_jpeg_dimensions(jpeg_data)?;
    bitmap.insert(
        id,
        CompressedBitmap::Jpeg {
            data: jpeg_data.to_vec(),
            alpha: None,
            width,
            height,
        },
    );
    Ok(())
}

fn define_bits_jpeg3(
    data: swf::DefineBitsJpeg3<'_>,
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
) -> Result<()> {
    let (width, height) = decode_define_bits_jpeg_dimensions(data.data)?;
    bitmap.insert(
        data.id,
        CompressedBitmap::Jpeg {
            data: data.data.to_vec(),
            alpha: Some(data.alpha_data.to_vec()),
            width,
            height,
        },
    );
    Ok(())
}

fn define_bits_lossless(
    data: swf::DefineBitsLossless<'_>,
    bitmap: &mut HashMap<CharacterId, CompressedBitmap>,
) {
    bitmap.insert(
        data.id,
        CompressedBitmap::Lossless(swf::DefineBitsLossless {
            version: data.version,
            id: data.id,
            format: data.format,
            width: data.width,
            height: data.height,
            data: Cow::Owned(data.data.into_owned()),
        }),
    );
}

// ===========================================================================
// Morph shape processing
// ===========================================================================

/// After all tags have been parsed, interpolate and tessellate morph shapes
/// at every unique (morph_id, ratio) that appears in the animation timeline.
fn process_morphs(
    builder: &mut VatfBuilder,
    bitmap: &HashMap<CharacterId, CompressedBitmap>,
) -> Result<()> {
    if builder.morph_shapes.is_empty() {
        return Ok(());
    }

    // Collect unique (morph_id, ratio) pairs from the animation timeline.
    let mut pairs: Vec<(u16, u16)> = Vec::new();
    for frames in builder.animations.values() {
        for frame in frames {
            for obj in frame {
                if builder.morph_shapes.contains_key(&obj.id)
                    && !pairs.contains(&(obj.id, obj.ratio))
                {
                    pairs.push((obj.id, obj.ratio));
                }
            }
        }
    }

    if pairs.is_empty() {
        return Ok(());
    }

    // Sort by (morph_id, ratio) for binary-search-friendly output.
    pairs.sort();
    info!(
        "Processing {} morph shape(s), {} unique frame(s)",
        builder.morph_shapes.len(),
        pairs.len()
    );

    // For each pair: interpolate → tessellate → record.
    for &(morph_id, ratio) in &pairs {
        let morph_data = &builder.morph_shapes[&morph_id];
        let shape = morph::interpolate(&morph_data.start, &morph_data.end, ratio);

        // Morph meshes are addressed through MORP, so they must not emit a
        // ShapeRecord (which would pollute `shape_map`, notably key 0).
        let (mesh_start, _mesh_count) = builder.process_shape_geometry(&shape, bitmap);

        // Record a MorphEntry for each mesh produced by this frame.
        for mi in mesh_start..builder.shape_meshes.len() as u32 {
            let mesh = &builder.shape_meshes[mi as usize];
            builder.morph_entries.push(MorphEntry {
                morph_id,
                ratio,
                _pad: 0,
                vertex_offset: mesh.vertex_offset,
                vertex_count: mesh.vertex_count,
                index_offset: mesh.index_offset,
                index_count: mesh.index_count,
            });
        }
    }

    info!("Built {} morph mesh entries", builder.morph_entries.len());

    Ok(())
}

/// Convert ExportAssets entries into a strict static, pure-vector UI library.
pub fn convert_swf_ui_to_vab(input: &Path, output: &Path) -> Result<()> {
    convert_swf_ui_to_vab_with_report(input, output).map(|_| ())
}
pub fn convert_swf_ui_to_vab_with_report(
    input: &Path,
    output: &Path,
) -> Result<ResourcePruningReport> {
    convert_swf(
        input,
        output,
        &SwfCompileSettings {
            mode: SwfCompileMode::StaticUi,
            ..Default::default()
        },
    )
}

/// Export vector UI with automatically baked looping child timelines.
pub fn convert_swf_animated_ui_to_vab(input: &Path, output: &Path) -> Result<()> {
    convert_swf_animated_ui_to_vab_with_report(input, output).map(|_| ())
}
pub fn convert_swf_animated_ui_to_vab_with_report(
    input: &Path,
    output: &Path,
) -> Result<ResourcePruningReport> {
    convert_swf(
        input,
        output,
        &SwfCompileSettings {
            mode: SwfCompileMode::AnimatedUi,
            ..Default::default()
        },
    )
}
