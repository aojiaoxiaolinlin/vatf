use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use bincode::Options;
use bytemuck;

use crate::{
    CHUNK_HEADER_SIZE, ChunkHeader, GradientUniforms, Header, MAGIC_BYTES, MorphEntry, ShapeMesh,
    ShapeRecord, VAB_VERSION, Vertex,
};

/// A chunk payload stored in an 8-byte aligned buffer.
///
/// Chunk payloads are packed without padding in the file, so copying them into
/// a plain `Vec<u8>` would leave the in-memory pointer alignment up to the
/// allocator — and `bytemuck::try_cast_slice` would then silently fail for
/// payloads that happen to land on an odd address. Backing the payload with
/// `Vec<u64>` guarantees 8-byte alignment, which covers every `Pod` type the
/// format stores (max alignment 4).
struct AlignedChunk {
    words: Vec<u64>,
    length: usize,
}

impl AlignedChunk {
    fn from_bytes(data: &[u8]) -> Self {
        let word_count = data.len().div_ceil(size_of::<u64>());
        let mut words = vec![0u64; word_count];
        let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
        bytes[..data.len()].copy_from_slice(data);
        Self {
            words,
            length: data.len(),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        let bytes: &[u8] = bytemuck::cast_slice(&self.words);
        &bytes[..self.length]
    }
}

/// Reader for the VAB (VATF) binary animation format.
///
/// Usage:
/// ```no_run
/// # use vatf::reader::VabReader;
/// let reader = VabReader::open("path/to/file.vab").unwrap();
/// let meshes = reader.shape_meshes().unwrap();
/// let verts = reader.vertices().unwrap();
/// let movie = reader.baked();
/// let nodes = &movie.clips[0].frames[0];
/// let fps = reader.frame_rate();
/// ```
pub struct VabReader {
    header: Header,
    chunks: HashMap<[u8; 4], AlignedChunk>,
    baked: crate::baked::BakedMovie,
}

impl VabReader {
    /// Open and parse a `.vab` file from disk.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref())
            .with_context(|| format!("Failed to read VAB file: {:?}", path.as_ref()))?;
        Self::from_bytes(&bytes)
    }

    /// Parse a VAB file from an in-memory byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if cfg!(target_endian = "big") {
            bail!("VATF v3 POD chunks require a little-endian host");
        }
        // ── File-level header (raw, uncompressed) ──────────────────────────
        if bytes.len() < 4 + std::mem::size_of::<Header>() {
            bail!("VAB file too short");
        }
        if &bytes[..4] != MAGIC_BYTES {
            bail!("Not a VAB file: bad magic bytes");
        }

        let header: Header = bytemuck::pod_read_unaligned(&bytes[4..12]);
        if header.version != VAB_VERSION {
            bail!(
                "Unsupported VAB version {} (this build expects {VAB_VERSION})",
                header.version,
            );
        }
        if header.length as usize != bytes.len() {
            bail!(
                "VAB length mismatch: header says {} B but file is {} B",
                header.length,
                bytes.len(),
            );
        }

        // ── Parse chunks from the raw payload ────────────────────────────
        let chunk_data = &bytes[12..];
        let mut chunks: HashMap<[u8; 4], AlignedChunk> = HashMap::new();
        let mut offset = 0;

        while offset < chunk_data.len() {
            if offset + CHUNK_HEADER_SIZE > chunk_data.len() {
                bail!("Truncated chunk header at offset {offset}");
            }

            let chunk_header: ChunkHeader =
                bytemuck::pod_read_unaligned(&chunk_data[offset..offset + CHUNK_HEADER_SIZE]);
            offset += CHUNK_HEADER_SIZE;

            let data_end = offset
                .checked_add(chunk_header.length as usize)
                .context("chunk length overflow")?;
            if data_end > chunk_data.len() {
                bail!(
                    "Truncated chunk {:?}: header says {} B but only {} B remain",
                    chunk_header.chunk_type,
                    chunk_header.length,
                    chunk_data.len() - offset,
                );
            }

            if chunks.contains_key(&chunk_header.chunk_type) {
                bail!("duplicate chunk {:?}", chunk_header.chunk_type);
            }
            chunks.insert(
                chunk_header.chunk_type,
                AlignedChunk::from_bytes(&chunk_data[offset..data_end]),
            );
            offset = data_end;
        }

        // ── Deserialize baked animation data ──────────────────────────────
        let baked_data = chunks.get(b"BAKD").context("VAB missing BAKD chunk")?;
        let baked: crate::baked::BakedMovie = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(bytes.len() as u64)
            .reject_trailing_bytes()
            .deserialize(baked_data.as_bytes())
            .context("Corrupt BAKD chunk")?;
        baked.validate()?;
        Ok(Self {
            baked,
            header,
            chunks,
        })
    }

    pub fn baked(&self) -> &crate::baked::BakedMovie {
        &self.baked
    }

    /// Consume the reader and take the owned baked movie (avoids cloning).
    pub fn into_baked(self) -> crate::baked::BakedMovie {
        self.baked
    }

    // ── Metadata ────────────────────────────────────────────────────────────

    /// The file-level header (version, total length).
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Whether a chunk with the given FourCC exists.
    pub fn has_chunk(&self, four_cc: &[u8; 4]) -> bool {
        self.chunks.contains_key(four_cc)
    }

    /// All parsed FourCC chunk types present in this file.
    pub fn chunk_types(&self) -> impl Iterator<Item = &[u8; 4]> {
        self.chunks.keys()
    }

    // ── Typed chunk accessors ───────────────────────────────────────────────

    fn chunk_data(&self, four_cc: &[u8; 4]) -> Option<&[u8]> {
        self.chunks.get(four_cc).map(AlignedChunk::as_bytes)
    }

    /// Try to cast byte slice to typed slice.
    ///
    /// Chunk payloads are backed by an 8-byte aligned buffer, so this only
    /// returns `None` when the payload length is not a multiple of
    /// `size_of::<T>()` (i.e. the file is malformed).
    fn cast_pod_slice<T: bytemuck::Pod>(data: &[u8]) -> Option<&[T]> {
        if data.is_empty() {
            return Some(&[]);
        }
        bytemuck::try_cast_slice(data).ok()
    }

    /// Shape records (lookup table of shapes → sub-mesh ranges).
    pub fn shape_records(&self) -> Option<&[ShapeRecord]> {
        self.chunk_data(b"SHAP").and_then(Self::cast_pod_slice)
    }

    /// Shape meshes (per-draw-call geometry metadata).
    pub fn shape_meshes(&self) -> Option<&[ShapeMesh]> {
        self.chunk_data(b"SHME").and_then(Self::cast_pod_slice)
    }

    /// Per-gradient uniforms (focal, interpolation, repeat, texture transform).
    pub fn gradient_uniforms(&self) -> Option<&[GradientUniforms]> {
        self.chunk_data(b"GRAD").and_then(Self::cast_pod_slice)
    }

    /// Per-bitmap uniforms (6-element affine transform matrix).
    pub fn bitmap_uniforms(&self) -> Option<&[[f32; 6]]> {
        self.chunk_data(b"BMAP").and_then(Self::cast_pod_slice)
    }

    /// Raw texture bytes (interleaved — WebP for gradients, bitmaps).
    /// Use `texture_offset` / `texture_length` from `ShapeMesh` to locate individual textures:
    pub fn texture_data(&self) -> Option<&[u8]> {
        self.chunk_data(b"TEXT")
    }

    /// Vertices (position + colour, quantised to i16).
    pub fn vertices(&self) -> Option<&[Vertex]> {
        self.chunk_data(b"VERT").and_then(Self::cast_pod_slice)
    }

    /// Triangle indices (u32 triplets).
    pub fn indices(&self) -> Option<&[u32]> {
        self.chunk_data(b"INDX").and_then(Self::cast_pod_slice)
    }

    /// Frame rate of the source SWF (frames per second).
    ///
    /// A SWF has exactly one frame rate; sprites share it. Stored in `BAKD`.
    pub fn frame_rate(&self) -> f32 {
        self.baked.frame_rate
    }

    /// Pre-interpolated morph frame entries.
    ///
    /// Each entry maps a (morph_id, ratio) pair to a mesh in VERT/INDX.
    /// Consumers should group by morph_id (e.g. `FnvHashMap<u16, FnvHashMap<u16, &MorphEntry>>`)
    /// for O(1) lookup at runtime.
    pub fn morph_entries(&self) -> Option<&[MorphEntry]> {
        self.chunk_data(b"MORP").and_then(Self::cast_pod_slice)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

impl VabReader {
    /// Optional native button state references. Older UI files need no UIBT chunk.
    pub fn buttons(&self) -> Result<Vec<crate::graphics::Button>> {
        let Some(data) = self.chunk_data(b"UIBT") else {
            return Ok(vec![]);
        };
        let buttons: Vec<crate::graphics::Button> = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(data.len() as u64)
            .reject_trailing_bytes()
            .deserialize(data)
            .context("Corrupt UIBT chunk")?;
        let graphics = self.graphics()?;
        let mut names: std::collections::HashSet<_> =
            graphics.iter().map(|g| g.name.as_str()).collect();
        for button in &buttons {
            crate::graphics::validate_name(&button.name)?;
            anyhow::ensure!(
                names.insert(&button.name),
                "duplicate UI export {}",
                button.name
            );
            for state in [
                Some(&button.up),
                Some(&button.over),
                Some(&button.down),
                button.hit_test.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                anyhow::ensure!(
                    graphics
                        .iter()
                        .any(|g| &g.name == state && g.frames.len() == 1),
                    "button {} references missing/non-static state {state}",
                    button.name
                );
            }
        }
        Ok(buttons)
    }
    /// Optional named static graphics. Ordinary animation files have no UIGR chunk.
    pub fn graphics(&self) -> Result<Vec<crate::graphics::Graphic>> {
        let Some(data) = self.chunk_data(b"UIGR") else {
            return Ok(Vec::new());
        };
        let graphics: Vec<crate::graphics::Graphic> = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(data.len() as u64)
            .reject_trailing_bytes()
            .deserialize(data)
            .context("Corrupt UIGR chunk")?;
        let mut names = std::collections::HashSet::new();
        for graphic in &graphics {
            crate::graphics::validate_name(&graphic.name)?;
            anyhow::ensure!(
                names.insert(&graphic.name),
                "duplicate UI export {}",
                graphic.name
            );
            let b = graphic.source_bounds;
            anyhow::ensure!(
                b.iter().all(|v| v.is_finite()) && b[2] > b[0] && b[3] > b[1],
                "invalid UI bounds for {}",
                graphic.name
            );
            anyhow::ensure!(
                !graphic.frames.is_empty() && graphic.frames.len() <= 4096,
                "invalid UI frame count"
            );
            anyhow::ensure!(
                graphic.frame_rate.is_finite()
                    && graphic.frame_rate >= 0.0
                    && (graphic.frames.len() == 1 || graphic.frame_rate > 0.0),
                "invalid UI frame rate"
            );
            let movie = crate::baked::BakedMovie {
                frame_rate: graphic.frame_rate,
                skins: vec![],
                clips: vec![crate::baked::BakedClip {
                    name: graphic.name.clone(),
                    start_frame: 0,
                    frames: graphic.frames.clone(),
                    events: vec![],
                }],
            };
            movie.validate()?;
        }
        Ok(graphics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayObject, VatfBuilder};

    #[test]
    fn round_trip_empty_file() {
        // Build a VAB with just an empty builder.
        let dir = std::env::temp_dir().join("vatf_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("empty_test.vab");

        let mut builder = VatfBuilder::default();
        builder.write_vatf(path.clone()).unwrap();

        // Read it back.
        let reader = VabReader::open(&path).unwrap();
        assert_eq!(reader.header().version, crate::VAB_VERSION);
        assert!(reader.has_chunk(b"SHAP"));
        assert!(reader.has_chunk(b"SHME"));
        assert!(reader.has_chunk(b"GRAD"));
        assert!(reader.has_chunk(b"BMAP"));
        assert!(reader.has_chunk(b"TEXT"));
        assert!(reader.has_chunk(b"VERT"));
        assert!(reader.has_chunk(b"INDX"));
        assert!(reader.has_chunk(b"MORP"));
        // The ANIM chunk was removed from the format; BAKD is the only
        // animation carrier now.
        assert!(!reader.has_chunk(b"ANIM"));

        // All data chunks should be empty.
        assert_eq!(reader.shape_records().unwrap().len(), 0);
        assert_eq!(reader.shape_meshes().unwrap().len(), 0);
        assert_eq!(reader.gradient_uniforms().unwrap().len(), 0);
        assert_eq!(reader.bitmap_uniforms().unwrap().len(), 0);
        assert_eq!(reader.texture_data().unwrap().len(), 0);
        assert_eq!(reader.vertices().unwrap().len(), 0);
        assert_eq!(reader.indices().unwrap().len(), 0);

        // An empty builder produces an empty movie.
        assert!(reader.baked().clips.is_empty());
        assert!(reader.baked().skins.is_empty());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_with_data() {
        use crate::{matrix::Matrix, transform::Transform};
        use swf::ColorTransform;

        let mut builder = VatfBuilder::default();

        // Add one dummy mesh and a simple animation.
        builder.shape_records.push(ShapeRecord {
            id: 1,
            sub_shape_count: 1,
            sub_shape_offset: 0,
        });
        builder.shape_meshes.push(ShapeMesh {
            vertex_count: 3,
            vertex_offset: 0,
            index_count: 3,
            index_offset: 0,
            material_type: 0,
            sampler_flags: 0,
            texture_offset: 0,
            texture_length: 0,
            material_offset: 0,
            bounds_half_x: 50.0,
            bounds_half_y: 50.0,
            bounds_center_x: 0.0,
            bounds_center_y: 0.0,
        });
        builder.vertices.extend_from_slice(&[
            Vertex {
                x: 0,
                y: 0,
                color: crate::Color {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Vertex {
                x: 100,
                y: 0,
                color: crate::Color {
                    r: 0,
                    g: 255,
                    b: 0,
                    a: 255,
                },
            },
            Vertex {
                x: 0,
                y: 100,
                color: crate::Color {
                    r: 0,
                    g: 0,
                    b: 255,
                    a: 255,
                },
            },
        ]);
        builder.indices.extend_from_slice(&[0u32, 1, 2]);

        let mut frames = Vec::new();
        let frame = vec![DisplayObject {
            id: 1,
            name: Some("shape1".into()),
            depth: 1,
            clip_depth: 0,
            blend_mode: swf::BlendMode::Normal,
            transform: Transform {
                matrix: Matrix::IDENTITY,
                color_transform: ColorTransform::IDENTITY,
            },
            filters: Box::new([]),
            ratio: 0,
            place_frame: 0,
        }];
        frames.push(frame);
        builder.animations.insert(0u16, frames);
        // Only `anim_`-prefixed root labels become clips; the prefix is stripped.
        builder.frame_labels.insert(Box::from("anim_start"), 0usize);
        builder.frame_rate = 24.0;

        let dir = std::env::temp_dir().join("vatf_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("data_test.vab");
        builder.write_vatf(path.clone()).unwrap();

        // Read it back.
        let reader = VabReader::open(&path).unwrap();

        let recs = reader.shape_records().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, 1);

        let meshes = reader.shape_meshes().unwrap();
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].vertex_count, 3);

        let verts = reader.vertices().unwrap();
        assert_eq!(verts.len(), 3);
        assert_eq!(verts[0].x, 0);

        let indices = reader.indices().unwrap();
        assert_eq!(indices, &[0u32, 1, 2]);

        // The `anim_start` label produced a clip named "start".
        let movie = reader.baked();
        assert_eq!(movie.frame_rate, 24.0);
        assert_eq!(movie.clips.len(), 1);
        assert_eq!(movie.clips[0].name, "start");
        assert_eq!(movie.clips[0].start_frame, 0);
        assert_eq!(movie.clips[0].frames.len(), 1);

        // The single display object became a shape node.
        match movie.clips[0].frames[0].as_slice() {
            [crate::baked::BakedNode::Shape { id, .. }] => assert_eq!(*id, 1),
            other => panic!("expected one Shape node, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reject_unsupported_version() {
        let dir = std::env::temp_dir().join("vatf_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("version_test.vab");

        let mut builder = VatfBuilder::default();
        builder.write_vatf(path.clone()).unwrap();

        // Header layout: magic (4 B) + version (u32, native endian) + length.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&(crate::VAB_VERSION + 1).to_ne_bytes());

        match VabReader::from_bytes(&bytes) {
            Err(error) => assert!(
                error.to_string().contains("Unsupported VAB version"),
                "unexpected error: {error}",
            ),
            Ok(_) => panic!("Expected error for unsupported version"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reject_invalid_magic() {
        match VabReader::from_bytes(b"NOTVATF\x00\x00\x00\x00\x00\x00\x00") {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                assert!(
                    msg.contains("magic"),
                    "Expected magic-related error, got: {e}",
                );
            }
            Ok(_) => panic!("Expected error for invalid magic"),
        }
    }
}
