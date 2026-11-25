//! Lossless compact binary codec for `BAKD` and `UIGR` chunks.
//!
//! Features:
//! - Frame-level interning: identical baked frames share one entry in a frame table.
//! - Bitmask transform encoding: omits identity linear matrices, zero translations,
//!   and identity color transforms (the common case for ~95% of display nodes).
//! - Bit-exact `f32` round-trip (`to_bits` / `from_bits`) with LEB128 varints.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

use crate::{
    animation::{
        AnimBevelFilter, AnimBlurFilter, AnimColorMatrixFilter, AnimColorTransform,
        AnimConvolutionFilter, AnimDropShadowFilter, AnimFilter, AnimGlowFilter,
        AnimGradientFilter, AnimGradientRecord, AnimMatrix, AnimTransform,
    },
    baked::{BakedClip, BakedMovie, BakedNode, BakedSkin, BakedSkinVariant, FrameEvent},
    graphics::Graphic,
};

const MAX_NESTING_DEPTH: usize = 128;

// Node tag discriminants (bits 0..2).
const NODE_SHAPE: u8 = 0;
const NODE_GROUP: u8 = 1;
const NODE_SKIN: u8 = 2;
const NODE_MASK: u8 = 3;

// Shape / Skin payload flags (bits 3..7).
const FLAG_RATIO: u8 = 1 << 3;
const FLAG_LINEAR_MATRIX: u8 = 1 << 4;
const FLAG_TRANSLATION: u8 = 1 << 5;
const FLAG_COLOR_MUL: u8 = 1 << 6;
const FLAG_COLOR_ADD: u8 = 1 << 7;

// Group payload flags (bits 3..4).
const FLAG_GROUP_FILTERS: u8 = 1 << 3;
const FLAG_GROUP_BLEND: u8 = 1 << 4;

// Filter discriminants.
const FILTER_DROP_SHADOW: u8 = 0;
const FILTER_BLUR: u8 = 1;
const FILTER_GLOW: u8 = 2;
const FILTER_BEVEL: u8 = 3;
const FILTER_GRADIENT_GLOW: u8 = 4;
const FILTER_CONVOLUTION: u8 = 5;
const FILTER_COLOR_MATRIX: u8 = 6;
const FILTER_GRADIENT_BEVEL: u8 = 7;

// ── Primitive writers ───────────────────────────────────────────────────────

fn write_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn write_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_bits().to_le_bytes());
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn is_bit_eq(a: f32, b: f32) -> bool {
    a.to_bits() == b.to_bits()
}

fn transform_flags(t: &AnimTransform) -> u8 {
    let mut flags = 0u8;
    let m = &t.matrix;
    if !(is_bit_eq(m.a, 1.0) && is_bit_eq(m.b, 0.0) && is_bit_eq(m.c, 0.0) && is_bit_eq(m.d, 1.0)) {
        flags |= FLAG_LINEAR_MATRIX;
    }
    if !(is_bit_eq(m.tx, 0.0) && is_bit_eq(m.ty, 0.0)) {
        flags |= FLAG_TRANSLATION;
    }
    let c = &t.color_transform;
    if !(is_bit_eq(c.r_multiply, 1.0)
        && is_bit_eq(c.g_multiply, 1.0)
        && is_bit_eq(c.b_multiply, 1.0)
        && is_bit_eq(c.a_multiply, 1.0))
    {
        flags |= FLAG_COLOR_MUL;
    }
    if !(is_bit_eq(c.r_add, 0.0)
        && is_bit_eq(c.g_add, 0.0)
        && is_bit_eq(c.b_add, 0.0)
        && is_bit_eq(c.a_add, 0.0))
    {
        flags |= FLAG_COLOR_ADD;
    }
    flags
}

fn write_transform_fields(out: &mut Vec<u8>, t: &AnimTransform, flags: u8) {
    if flags & FLAG_LINEAR_MATRIX != 0 {
        write_f32(out, t.matrix.a);
        write_f32(out, t.matrix.b);
        write_f32(out, t.matrix.c);
        write_f32(out, t.matrix.d);
    }
    if flags & FLAG_TRANSLATION != 0 {
        write_f32(out, t.matrix.tx);
        write_f32(out, t.matrix.ty);
    }
    if flags & FLAG_COLOR_MUL != 0 {
        write_f32(out, t.color_transform.r_multiply);
        write_f32(out, t.color_transform.g_multiply);
        write_f32(out, t.color_transform.b_multiply);
        write_f32(out, t.color_transform.a_multiply);
    }
    if flags & FLAG_COLOR_ADD != 0 {
        write_f32(out, t.color_transform.r_add);
        write_f32(out, t.color_transform.g_add);
        write_f32(out, t.color_transform.b_add);
        write_f32(out, t.color_transform.a_add);
    }
}

fn write_gradient_filter(out: &mut Vec<u8>, f: &AnimGradientFilter) {
    write_u8(out, f.flags);
    write_varint(out, f.colors.len() as u64);
    for r in &f.colors {
        write_u8(out, r.ratio);
        write_u8(out, r.color_r);
        write_u8(out, r.color_g);
        write_u8(out, r.color_b);
        write_u8(out, r.color_a);
    }
    write_i32(out, f.blur_x);
    write_i32(out, f.blur_y);
    write_i32(out, f.angle);
    write_i32(out, f.distance);
    write_i16(out, f.strength);
    write_u8(out, f.num_passes);
}

fn write_filter(out: &mut Vec<u8>, filter: &AnimFilter) {
    match filter {
        AnimFilter::DropShadowFilter(f) => {
            write_u8(out, FILTER_DROP_SHADOW);
            write_u8(out, f.flags);
            write_u8(out, f.color_r);
            write_u8(out, f.color_g);
            write_u8(out, f.color_b);
            write_u8(out, f.color_a);
            write_i32(out, f.blur_x);
            write_i32(out, f.blur_y);
            write_i32(out, f.angle);
            write_i32(out, f.distance);
            write_i16(out, f.strength);
            write_u8(out, f.num_passes);
        }
        AnimFilter::BlurFilter(f) => {
            write_u8(out, FILTER_BLUR);
            write_i32(out, f.blur_x);
            write_i32(out, f.blur_y);
            write_u8(out, f.num_passes);
        }
        AnimFilter::GlowFilter(f) => {
            write_u8(out, FILTER_GLOW);
            write_u8(out, f.flags);
            write_u8(out, f.color_r);
            write_u8(out, f.color_g);
            write_u8(out, f.color_b);
            write_u8(out, f.color_a);
            write_i32(out, f.blur_x);
            write_i32(out, f.blur_y);
            write_i16(out, f.strength);
            write_u8(out, f.num_passes);
        }
        AnimFilter::BevelFilter(f) => {
            write_u8(out, FILTER_BEVEL);
            write_u8(out, f.flags);
            write_u8(out, f.shadow_color_r);
            write_u8(out, f.shadow_color_g);
            write_u8(out, f.shadow_color_b);
            write_u8(out, f.shadow_color_a);
            write_u8(out, f.highlight_color_r);
            write_u8(out, f.highlight_color_g);
            write_u8(out, f.highlight_color_b);
            write_u8(out, f.highlight_color_a);
            write_i32(out, f.blur_x);
            write_i32(out, f.blur_y);
            write_i32(out, f.angle);
            write_i32(out, f.distance);
            write_i16(out, f.strength);
            write_u8(out, f.num_passes);
        }
        AnimFilter::GradientGlowFilter(f) => {
            write_u8(out, FILTER_GRADIENT_GLOW);
            write_gradient_filter(out, f);
        }
        AnimFilter::ConvolutionFilter(f) => {
            write_u8(out, FILTER_CONVOLUTION);
            write_u8(out, f.num_matrix_rows);
            write_u8(out, f.num_matrix_cols);
            write_varint(out, f.matrix.len() as u64);
            for &v in &f.matrix {
                write_f32(out, v);
            }
            write_f32(out, f.divisor);
            write_f32(out, f.bias);
            write_u8(out, f.default_color_r);
            write_u8(out, f.default_color_g);
            write_u8(out, f.default_color_b);
            write_u8(out, f.default_color_a);
            write_u8(out, f.flags);
        }
        AnimFilter::ColorMatrixFilter(f) => {
            write_u8(out, FILTER_COLOR_MATRIX);
            for &v in &f.matrix {
                write_f32(out, v);
            }
        }
        AnimFilter::GradientBevelFilter(f) => {
            write_u8(out, FILTER_GRADIENT_BEVEL);
            write_gradient_filter(out, f);
        }
    }
}

fn write_nodes(out: &mut Vec<u8>, nodes: &[BakedNode]) {
    write_varint(out, nodes.len() as u64);
    for node in nodes {
        match node {
            BakedNode::Shape {
                id,
                ratio,
                transform,
            } => {
                let mut header = NODE_SHAPE | transform_flags(transform);
                if *ratio != 0 {
                    header |= FLAG_RATIO;
                }
                write_u8(out, header);
                write_u16(out, *id);
                if *ratio != 0 {
                    write_u16(out, *ratio);
                }
                write_transform_fields(out, transform, header);
            }
            BakedNode::Group {
                children,
                filters,
                blend_mode,
            } => {
                let mut header = NODE_GROUP;
                if !filters.is_empty() {
                    header |= FLAG_GROUP_FILTERS;
                }
                if *blend_mode != 0 {
                    header |= FLAG_GROUP_BLEND;
                }
                write_u8(out, header);
                if *blend_mode != 0 {
                    write_u8(out, *blend_mode);
                }
                if !filters.is_empty() {
                    write_varint(out, filters.len() as u64);
                    for filter in filters {
                        write_filter(out, filter);
                    }
                }
                write_nodes(out, children);
            }
            BakedNode::Skin {
                slot,
                symbol,
                transform,
            } => {
                let header = NODE_SKIN | transform_flags(transform);
                write_u8(out, header);
                write_u16(out, *symbol);
                write_str(out, slot);
                write_transform_fields(out, transform, header);
            }
            BakedNode::Mask { mask, children } => {
                write_u8(out, NODE_MASK);
                write_nodes(out, mask);
                write_nodes(out, children);
            }
        }
    }
}

struct FrameInterner {
    lookup: HashMap<Vec<u8>, u32>,
    frames: Vec<Vec<u8>>,
}

impl FrameInterner {
    fn new() -> Self {
        Self {
            lookup: HashMap::new(),
            frames: Vec::new(),
        }
    }

    fn intern(&mut self, nodes: &[BakedNode]) -> u32 {
        let mut encoded = Vec::new();
        write_nodes(&mut encoded, nodes);
        if let Some(&index) = self.lookup.get(&encoded) {
            return index;
        }
        let index = self.frames.len() as u32;
        self.lookup.insert(encoded.clone(), index);
        self.frames.push(encoded);
        index
    }

    fn write_table(&self, out: &mut Vec<u8>) {
        write_varint(out, self.frames.len() as u64);
        for frame in &self.frames {
            out.extend_from_slice(frame);
        }
    }
}

/// Encode a `BakedMovie` into compact lossless binary bytes with frame deduplication.
pub fn encode_movie(movie: &BakedMovie) -> Vec<u8> {
    let mut interner = FrameInterner::new();
    let clip_frame_ids: Vec<Vec<u32>> = movie
        .clips
        .iter()
        .map(|clip| clip.frames.iter().map(|f| interner.intern(f)).collect())
        .collect();

    let mut out = Vec::new();
    write_f32(&mut out, movie.frame_rate);
    interner.write_table(&mut out);

    write_varint(&mut out, movie.clips.len() as u64);
    for (clip, frame_ids) in movie.clips.iter().zip(&clip_frame_ids) {
        write_str(&mut out, &clip.name);
        write_varint(&mut out, u64::from(clip.start_frame));
        write_varint(&mut out, frame_ids.len() as u64);
        for &id in frame_ids {
            write_varint(&mut out, u64::from(id));
        }
        write_varint(&mut out, clip.events.len() as u64);
        for event in &clip.events {
            write_varint(&mut out, u64::from(event.frame));
            write_str(&mut out, &event.name);
        }
    }

    write_varint(&mut out, movie.skins.len() as u64);
    for skin in &movie.skins {
        write_u16(&mut out, skin.symbol);
        write_varint(&mut out, skin.variants.len() as u64);
        for variant in &skin.variants {
            write_str(&mut out, &variant.name);
            write_nodes(&mut out, &variant.nodes);
        }
    }
    out
}

/// Encode exported UI graphics into compact lossless binary bytes with frame deduplication.
pub fn encode_graphics(graphics: &[Graphic]) -> Vec<u8> {
    let mut interner = FrameInterner::new();
    let graphic_frame_ids: Vec<Vec<u32>> = graphics
        .iter()
        .map(|g| g.frames.iter().map(|f| interner.intern(f)).collect())
        .collect();

    let mut out = Vec::new();
    interner.write_table(&mut out);
    write_varint(&mut out, graphics.len() as u64);
    for (graphic, frame_ids) in graphics.iter().zip(&graphic_frame_ids) {
        write_str(&mut out, &graphic.name);
        for &v in &graphic.source_bounds {
            write_f32(&mut out, v);
        }
        write_f32(&mut out, graphic.frame_rate);
        write_varint(&mut out, frame_ids.len() as u64);
        for &id in frame_ids {
            write_varint(&mut out, u64::from(id));
        }
    }
    out
}

// ── Primitive readers ───────────────────────────────────────────────────────

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("cursor overflow")?;
        let slice = self
            .data
            .get(self.pos..end)
            .context("unexpected end of compact VAB stream")?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_bytes(2)?.try_into().unwrap()))
    }

    fn read_i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.read_bytes(2)?.try_into().unwrap()))
    }

    fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.read_bytes(4)?.try_into().unwrap()))
    }

    fn read_f32(&mut self) -> Result<f32> {
        let bits = u32::from_le_bytes(self.read_bytes(4)?.try_into().unwrap());
        Ok(f32::from_bits(bits))
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            ensure!(shift < 64, "varint overflow");
            let byte = self.read_u8()?;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn read_count(&mut self) -> Result<usize> {
        let count = usize::try_from(self.read_varint()?).context("count exceeds usize")?;
        // Each element consumes at least 1 byte on wire, bounding pre-allocation.
        ensure!(
            count <= self.data.len() - self.pos,
            "declared count {count} exceeds remaining bytes"
        );
        Ok(count)
    }

    fn read_str(&mut self) -> Result<String> {
        let len = self.read_count()?;
        let bytes = self.read_bytes(len)?;
        let s = std::str::from_utf8(bytes).context("invalid UTF-8 string in compact chunk")?;
        Ok(s.to_owned())
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.pos == self.data.len(),
            "trailing {} bytes in compact chunk",
            self.data.len() - self.pos
        );
        Ok(())
    }
}

fn read_transform_fields(cur: &mut Cursor<'_>, header: u8) -> Result<AnimTransform> {
    let mut matrix = AnimMatrix::IDENTITY;
    if header & FLAG_LINEAR_MATRIX != 0 {
        matrix.a = cur.read_f32()?;
        matrix.b = cur.read_f32()?;
        matrix.c = cur.read_f32()?;
        matrix.d = cur.read_f32()?;
    }
    if header & FLAG_TRANSLATION != 0 {
        matrix.tx = cur.read_f32()?;
        matrix.ty = cur.read_f32()?;
    }
    let mut color = AnimColorTransform::IDENTITY;
    if header & FLAG_COLOR_MUL != 0 {
        color.r_multiply = cur.read_f32()?;
        color.g_multiply = cur.read_f32()?;
        color.b_multiply = cur.read_f32()?;
        color.a_multiply = cur.read_f32()?;
    }
    if header & FLAG_COLOR_ADD != 0 {
        color.r_add = cur.read_f32()?;
        color.g_add = cur.read_f32()?;
        color.b_add = cur.read_f32()?;
        color.a_add = cur.read_f32()?;
    }
    Ok(AnimTransform {
        matrix,
        color_transform: color,
    })
}

fn read_gradient_filter(cur: &mut Cursor<'_>) -> Result<AnimGradientFilter> {
    let flags = cur.read_u8()?;
    let count = cur.read_count()?;
    let mut colors = Vec::with_capacity(count);
    for _ in 0..count {
        colors.push(AnimGradientRecord {
            ratio: cur.read_u8()?,
            color_r: cur.read_u8()?,
            color_g: cur.read_u8()?,
            color_b: cur.read_u8()?,
            color_a: cur.read_u8()?,
        });
    }
    Ok(AnimGradientFilter {
        flags,
        colors,
        blur_x: cur.read_i32()?,
        blur_y: cur.read_i32()?,
        angle: cur.read_i32()?,
        distance: cur.read_i32()?,
        strength: cur.read_i16()?,
        num_passes: cur.read_u8()?,
    })
}

fn read_filter(cur: &mut Cursor<'_>) -> Result<AnimFilter> {
    match cur.read_u8()? {
        FILTER_DROP_SHADOW => Ok(AnimFilter::DropShadowFilter(AnimDropShadowFilter {
            flags: cur.read_u8()?,
            color_r: cur.read_u8()?,
            color_g: cur.read_u8()?,
            color_b: cur.read_u8()?,
            color_a: cur.read_u8()?,
            blur_x: cur.read_i32()?,
            blur_y: cur.read_i32()?,
            angle: cur.read_i32()?,
            distance: cur.read_i32()?,
            strength: cur.read_i16()?,
            num_passes: cur.read_u8()?,
        })),
        FILTER_BLUR => Ok(AnimFilter::BlurFilter(AnimBlurFilter {
            blur_x: cur.read_i32()?,
            blur_y: cur.read_i32()?,
            num_passes: cur.read_u8()?,
        })),
        FILTER_GLOW => Ok(AnimFilter::GlowFilter(AnimGlowFilter {
            flags: cur.read_u8()?,
            color_r: cur.read_u8()?,
            color_g: cur.read_u8()?,
            color_b: cur.read_u8()?,
            color_a: cur.read_u8()?,
            blur_x: cur.read_i32()?,
            blur_y: cur.read_i32()?,
            strength: cur.read_i16()?,
            num_passes: cur.read_u8()?,
        })),
        FILTER_BEVEL => Ok(AnimFilter::BevelFilter(AnimBevelFilter {
            flags: cur.read_u8()?,
            shadow_color_r: cur.read_u8()?,
            shadow_color_g: cur.read_u8()?,
            shadow_color_b: cur.read_u8()?,
            shadow_color_a: cur.read_u8()?,
            highlight_color_r: cur.read_u8()?,
            highlight_color_g: cur.read_u8()?,
            highlight_color_b: cur.read_u8()?,
            highlight_color_a: cur.read_u8()?,
            blur_x: cur.read_i32()?,
            blur_y: cur.read_i32()?,
            angle: cur.read_i32()?,
            distance: cur.read_i32()?,
            strength: cur.read_i16()?,
            num_passes: cur.read_u8()?,
        })),
        FILTER_GRADIENT_GLOW => Ok(AnimFilter::GradientGlowFilter(read_gradient_filter(cur)?)),
        FILTER_CONVOLUTION => {
            let num_matrix_rows = cur.read_u8()?;
            let num_matrix_cols = cur.read_u8()?;
            let len = cur.read_count()?;
            let mut matrix = Vec::with_capacity(len);
            for _ in 0..len {
                matrix.push(cur.read_f32()?);
            }
            Ok(AnimFilter::ConvolutionFilter(AnimConvolutionFilter {
                num_matrix_rows,
                num_matrix_cols,
                matrix,
                divisor: cur.read_f32()?,
                bias: cur.read_f32()?,
                default_color_r: cur.read_u8()?,
                default_color_g: cur.read_u8()?,
                default_color_b: cur.read_u8()?,
                default_color_a: cur.read_u8()?,
                flags: cur.read_u8()?,
            }))
        }
        FILTER_COLOR_MATRIX => {
            let mut matrix = [0.0f32; 20];
            for slot in &mut matrix {
                *slot = cur.read_f32()?;
            }
            Ok(AnimFilter::ColorMatrixFilter(AnimColorMatrixFilter {
                matrix,
            }))
        }
        FILTER_GRADIENT_BEVEL => Ok(AnimFilter::GradientBevelFilter(read_gradient_filter(cur)?)),
        other => bail!("unknown filter tag {other}"),
    }
}

fn read_nodes(cur: &mut Cursor<'_>, depth: usize) -> Result<Vec<BakedNode>> {
    ensure!(depth <= MAX_NESTING_DEPTH, "baked node nesting too deep");
    let count = cur.read_count()?;
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let header = cur.read_u8()?;
        let tag = header & 0x07;
        match tag {
            NODE_SHAPE => {
                let id = cur.read_u16()?;
                let ratio = if header & FLAG_RATIO != 0 {
                    cur.read_u16()?
                } else {
                    0
                };
                let transform = read_transform_fields(cur, header)?;
                nodes.push(BakedNode::Shape {
                    id,
                    ratio,
                    transform,
                });
            }
            NODE_GROUP => {
                let blend_mode = if header & FLAG_GROUP_BLEND != 0 {
                    cur.read_u8()?
                } else {
                    0
                };
                let filters = if header & FLAG_GROUP_FILTERS != 0 {
                    let n = cur.read_count()?;
                    let mut out = Vec::with_capacity(n);
                    for _ in 0..n {
                        out.push(read_filter(cur)?);
                    }
                    out
                } else {
                    Vec::new()
                };
                let children = read_nodes(cur, depth + 1)?;
                nodes.push(BakedNode::Group {
                    children,
                    filters,
                    blend_mode,
                });
            }
            NODE_SKIN => {
                let symbol = cur.read_u16()?;
                let slot = cur.read_str()?;
                let transform = read_transform_fields(cur, header)?;
                nodes.push(BakedNode::Skin {
                    slot,
                    symbol,
                    transform,
                });
            }
            NODE_MASK => {
                let mask = read_nodes(cur, depth + 1)?;
                let children = read_nodes(cur, depth + 1)?;
                nodes.push(BakedNode::Mask { mask, children });
            }
            other => bail!("unknown baked node tag {other}"),
        }
    }
    Ok(nodes)
}

fn read_frame_table(cur: &mut Cursor<'_>) -> Result<Vec<Vec<BakedNode>>> {
    let count = cur.read_count()?;
    let mut table = Vec::with_capacity(count);
    for _ in 0..count {
        table.push(read_nodes(cur, 0)?);
    }
    Ok(table)
}

/// Decode a `BakedMovie` from compact binary bytes.
pub fn decode_movie(data: &[u8]) -> Result<BakedMovie> {
    let mut cur = Cursor::new(data);
    let frame_rate = cur.read_f32()?;
    let frame_table = read_frame_table(&mut cur)?;

    let clip_count = cur.read_count()?;
    let mut clips = Vec::with_capacity(clip_count);
    for _ in 0..clip_count {
        let name = cur.read_str()?;
        let start_frame = u32::try_from(cur.read_varint()?).context("start_frame exceeds u32")?;
        let frame_count = cur.read_count()?;
        let mut frames = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            let idx = usize::try_from(cur.read_varint()?).context("frame index exceeds usize")?;
            let frame = frame_table.get(idx).with_context(|| {
                format!("frame index {idx} out of range ({})", frame_table.len())
            })?;
            frames.push(frame.clone());
        }
        let event_count = cur.read_count()?;
        let mut events = Vec::with_capacity(event_count);
        for _ in 0..event_count {
            let frame = u32::try_from(cur.read_varint()?).context("event frame exceeds u32")?;
            let name = cur.read_str()?;
            events.push(FrameEvent { frame, name });
        }
        clips.push(BakedClip {
            name,
            start_frame,
            frames,
            events,
        });
    }

    let skin_count = cur.read_count()?;
    let mut skins = Vec::with_capacity(skin_count);
    for _ in 0..skin_count {
        let symbol = cur.read_u16()?;
        let variant_count = cur.read_count()?;
        let mut variants = Vec::with_capacity(variant_count);
        for _ in 0..variant_count {
            let name = cur.read_str()?;
            let nodes = read_nodes(&mut cur, 0)?;
            variants.push(BakedSkinVariant { name, nodes });
        }
        skins.push(BakedSkin { symbol, variants });
    }
    cur.finish()?;
    Ok(BakedMovie {
        frame_rate,
        clips,
        skins,
    })
}

/// Decode exported UI graphics from compact binary bytes.
pub fn decode_graphics(data: &[u8]) -> Result<Vec<Graphic>> {
    let mut cur = Cursor::new(data);
    let frame_table = read_frame_table(&mut cur)?;
    let count = cur.read_count()?;
    let mut graphics = Vec::with_capacity(count);
    for _ in 0..count {
        let name = cur.read_str()?;
        let source_bounds = [
            cur.read_f32()?,
            cur.read_f32()?,
            cur.read_f32()?,
            cur.read_f32()?,
        ];
        let frame_rate = cur.read_f32()?;
        let frame_count = cur.read_count()?;
        let mut frames = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            let idx = usize::try_from(cur.read_varint()?).context("frame index exceeds usize")?;
            let frame = frame_table.get(idx).with_context(|| {
                format!("frame index {idx} out of range ({})", frame_table.len())
            })?;
            frames.push(frame.clone());
        }
        graphics.push(Graphic {
            name,
            source_bounds,
            frames,
            frame_rate,
        });
    }
    cur.finish()?;
    Ok(graphics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_movie_round_trips_all_node_and_filter_variants_and_deduplicates_frames() {
        let shared_frame = vec![
            BakedNode::Shape {
                id: 7,
                ratio: 0,
                transform: AnimTransform {
                    matrix: AnimMatrix {
                        tx: 12.5,
                        ty: -3.25,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
            BakedNode::Group {
                blend_mode: 8,
                filters: vec![
                    AnimFilter::BlurFilter(AnimBlurFilter {
                        blur_x: 5 * 65536,
                        blur_y: 9 * 65536,
                        num_passes: 2,
                    }),
                    AnimFilter::GlowFilter(AnimGlowFilter {
                        flags: 0x21,
                        color_r: 255,
                        color_g: 10,
                        color_b: 20,
                        color_a: 200,
                        blur_x: 4 * 65536,
                        blur_y: 4 * 65536,
                        strength: 512,
                        num_passes: 1,
                    }),
                    AnimFilter::ColorMatrixFilter(AnimColorMatrixFilter {
                        matrix: [
                            0.5, 0.0, 0.0, 0.0, 10.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
                            0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
                        ],
                    }),
                ],
                children: vec![BakedNode::Mask {
                    mask: vec![BakedNode::Shape {
                        id: 2,
                        ratio: 32000,
                        transform: AnimTransform::default(),
                    }],
                    children: vec![BakedNode::Skin {
                        slot: "weapon".into(),
                        symbol: 99,
                        transform: AnimTransform::default(),
                    }],
                }],
            },
        ];
        let movie = BakedMovie {
            frame_rate: 24.0,
            clips: vec![BakedClip {
                name: "idle".into(),
                start_frame: 0,
                frames: vec![shared_frame.clone(); 50],
                events: vec![FrameEvent {
                    frame: 3,
                    name: "step".into(),
                }],
            }],
            skins: vec![BakedSkin {
                symbol: 99,
                variants: vec![BakedSkinVariant {
                    name: "sword".into(),
                    nodes: vec![BakedNode::Shape {
                        id: 10,
                        ratio: 0,
                        transform: AnimTransform::default(),
                    }],
                }],
            }],
        };

        let encoded = encode_movie(&movie);
        let bincode_len = bincode::serialize(&movie).unwrap().len();
        assert!(
            encoded.len() * 5 < bincode_len,
            "compact encoding should drastically shrink repeated frames: {} vs {}",
            encoded.len(),
            bincode_len
        );
        let decoded = decode_movie(&encoded).unwrap();
        assert_eq!(decoded.frame_rate, 24.0);
        assert_eq!(decoded.clips.len(), 1);
        assert_eq!(decoded.clips[0].frames.len(), 50);
        assert_eq!(decoded.clips[0].events, movie.clips[0].events);
        assert_eq!(decoded.skins.len(), 1);
        assert_eq!(decoded.skins[0].variants[0].name, "sword");
    }
}
