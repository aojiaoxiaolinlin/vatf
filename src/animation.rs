use std::{collections::HashMap, ops::Mul};

use serde::{Deserialize, Serialize};
use swf::CharacterId;

use crate::{DisplayObject, filter::Filter, transform::Transform};

// ---------------------------------------------------------------------------
// Wire types — decomposed to primitives only, no swf crate type dependencies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimContainer {
    pub animations: Vec<(u16, Vec<AnimFrame>)>,
    pub labels: Vec<(Box<str>, usize)>,
    /// Frame rate of the source SWF, in frames per second.
    ///
    /// Carried through to [`crate::baked::BakedMovie::frame_rate`], which is
    /// where the runtime reads it from.
    pub frame_rate: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimFrame {
    pub entries: Vec<AnimDisplayObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimDisplayObject {
    pub id: u16,
    pub name: Option<String>,
    pub depth: u16,
    pub clip_depth: u16,
    pub blend_mode: u8,
    pub transform: AnimTransform,
    pub filters: Vec<AnimFilter>,
    pub ratio: u16,
    /// Index (0-based) of the *parent* timeline frame at which this instance
    /// was placed.
    ///
    /// Sub-sprite timelines are driven relative to this: a sprite placed on
    /// parent frame `place_frame` shows its own frame
    /// `(parent_frame - place_frame) % child_frame_count`.
    pub place_frame: u32,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct AnimTransform {
    pub matrix: AnimMatrix,
    pub color_transform: AnimColorTransform,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AnimMatrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl AnimMatrix {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };
}

impl Default for AnimMatrix {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mul for AnimMatrix {
    type Output = AnimMatrix;

    fn mul(self, rhs: Self) -> Self::Output {
        AnimMatrix {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            tx: self.a * rhs.tx + self.c * rhs.ty + self.tx,
            ty: self.b * rhs.tx + self.d * rhs.ty + self.ty,
        }
    }
}

impl AnimColorTransform {
    pub const IDENTITY: Self = Self {
        r_multiply: 1.0,
        b_multiply: 1.0,
        g_multiply: 1.0,
        a_multiply: 1.0,
        r_add: 0.0,
        g_add: 0.0,
        b_add: 0.0,
        a_add: 0.0,
    };
}

impl Mul for AnimColorTransform {
    type Output = AnimColorTransform;

    fn mul(self, rhs: Self) -> Self::Output {
        AnimColorTransform {
            r_multiply: self.r_multiply * rhs.r_multiply,
            g_multiply: self.g_multiply * rhs.g_multiply,
            b_multiply: self.b_multiply * rhs.b_multiply,
            a_multiply: self.a_multiply * rhs.a_multiply,
            r_add: self.r_add + self.r_multiply * rhs.r_add,
            g_add: self.g_add + self.g_multiply * rhs.g_add,
            b_add: self.b_add + self.b_multiply * rhs.b_add,
            a_add: self.a_add + self.a_multiply * rhs.a_add,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AnimColorTransform {
    pub r_multiply: f32,
    pub g_multiply: f32,
    pub b_multiply: f32,
    pub a_multiply: f32,
    pub r_add: f32,
    pub g_add: f32,
    pub b_add: f32,
    pub a_add: f32,
}

impl Default for AnimColorTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

// Filter variants -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnimFilter {
    DropShadowFilter(AnimDropShadowFilter),
    BlurFilter(AnimBlurFilter),
    GlowFilter(AnimGlowFilter),
    BevelFilter(AnimBevelFilter),
    GradientGlowFilter(AnimGradientFilter),
    ConvolutionFilter(AnimConvolutionFilter),
    ColorMatrixFilter(AnimColorMatrixFilter),
    GradientBevelFilter(AnimGradientFilter),
}

impl AnimFilter {
    /// Scale filter parameters by the stage view factor.
    /// Matches swf crate's `BlurFilter::scale_blur` formula: `(blur - ONE) * factor + ONE`.
    pub fn scale(&mut self, x: f32, y: f32) {
        match self {
            AnimFilter::BlurFilter(f) => f.scale(x, y),
            AnimFilter::GlowFilter(f) => f.scale(x, y),
            AnimFilter::DropShadowFilter(f) => f.scale(x, y),
            AnimFilter::BevelFilter(f) => f.scale(x, y),
            AnimFilter::GradientGlowFilter(f) => f.scale(x, y),
            AnimFilter::GradientBevelFilter(f) => f.scale(x, y),
            _ => {}
        }
    }

    pub fn impotent(&self) -> bool {
        match self {
            AnimFilter::BlurFilter(filter) => filter.impotent(),
            AnimFilter::ColorMatrixFilter(filter) => filter.impotent(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimBlurFilter {
    pub blur_x: i32,
    pub blur_y: i32,
    /// Pre-computed passes (1..15), NOT raw bitflags.
    /// Set from `inner.num_passes()` during conversion.
    pub num_passes: u8,
}

impl AnimBlurFilter {
    pub fn impotent(&self) -> bool {
        self.num_passes == 0 || (self.blur_x <= 65536 && self.blur_y <= 65536)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimGlowFilter {
    pub flags: u8,
    pub color_r: u8,
    pub color_g: u8,
    pub color_b: u8,
    pub color_a: u8,
    pub blur_x: i32,
    pub blur_y: i32,
    pub strength: i16,
    pub num_passes: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimDropShadowFilter {
    pub flags: u8,
    pub color_r: u8,
    pub color_g: u8,
    pub color_b: u8,
    pub color_a: u8,
    pub blur_x: i32,
    pub blur_y: i32,
    pub angle: i32,
    pub distance: i32,
    pub strength: i16,
    pub num_passes: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimBevelFilter {
    pub flags: u8,
    pub shadow_color_r: u8,
    pub shadow_color_g: u8,
    pub shadow_color_b: u8,
    pub shadow_color_a: u8,
    pub highlight_color_r: u8,
    pub highlight_color_g: u8,
    pub highlight_color_b: u8,
    pub highlight_color_a: u8,
    pub blur_x: i32,
    pub blur_y: i32,
    pub angle: i32,
    pub distance: i32,
    pub strength: i16,
    pub num_passes: u8,
}
impl AnimBlurFilter {
    /// `(blur - ONE) * factor + ONE` — matching swf crate's `BlurFilter::scale_blur`.
    fn scale_blur(blur: i32, factor: f32) -> i32 {
        ((blur as f64 - 65536.0) * factor as f64 + 65536.0) as i32
    }
    pub fn scale(&mut self, x: f32, y: f32) {
        self.blur_x = Self::scale_blur(self.blur_x, x);
        self.blur_y = Self::scale_blur(self.blur_y, y);
    }
}

impl AnimGlowFilter {
    pub fn scale(&mut self, x: f32, y: f32) {
        self.blur_x = AnimBlurFilter::scale_blur(self.blur_x, x);
        self.blur_y = AnimBlurFilter::scale_blur(self.blur_y, y);
    }
}

impl AnimDropShadowFilter {
    pub fn scale(&mut self, x: f32, y: f32) {
        self.blur_x = AnimBlurFilter::scale_blur(self.blur_x, x);
        self.blur_y = AnimBlurFilter::scale_blur(self.blur_y, y);
        self.distance = (self.distance as f64 * y as f64) as i32;
    }
}

impl AnimBevelFilter {
    pub fn scale(&mut self, x: f32, y: f32) {
        self.blur_x = AnimBlurFilter::scale_blur(self.blur_x, x);
        self.blur_y = AnimBlurFilter::scale_blur(self.blur_y, y);
        self.distance = (self.distance as f64 * y as f64) as i32;
    }
}

impl AnimGradientFilter {
    pub fn scale(&mut self, x: f32, y: f32) {
        self.blur_x = AnimBlurFilter::scale_blur(self.blur_x, x);
        self.blur_y = AnimBlurFilter::scale_blur(self.blur_y, y);
        self.distance = (self.distance as f64 * y as f64) as i32;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimGradientFilter {
    pub flags: u8,
    pub colors: Vec<AnimGradientRecord>,
    pub blur_x: i32,
    pub blur_y: i32,
    pub angle: i32,
    pub distance: i32,
    pub strength: i16,
    pub num_passes: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimColorMatrixFilter {
    pub matrix: [f32; 20],
}

impl Default for AnimColorMatrixFilter {
    fn default() -> Self {
        Self {
            matrix: [
                1.0, 0.0, 0.0, 0.0, 0.0, // r
                0.0, 1.0, 0.0, 0.0, 0.0, // g
                0.0, 0.0, 1.0, 0.0, 0.0, // b
                0.0, 0.0, 0.0, 1.0, 0.0, //a
            ],
        }
    }
}

impl AnimColorMatrixFilter {
    pub fn impotent(&self) -> bool {
        self == &Default::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimConvolutionFilter {
    pub num_matrix_rows: u8,
    pub num_matrix_cols: u8,
    pub matrix: Vec<f32>,
    pub divisor: f32,
    pub bias: f32,
    pub default_color_r: u8,
    pub default_color_g: u8,
    pub default_color_b: u8,
    pub default_color_a: u8,
    pub flags: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimGradientRecord {
    pub ratio: u8,
    pub color_r: u8,
    pub color_g: u8,
    pub color_b: u8,
    pub color_a: u8,
}

// ---------------------------------------------------------------------------
// From impls: runtime types → wire types
// ---------------------------------------------------------------------------

impl AnimContainer {
    /// Build the in-memory container from the parsed SWF data.
    ///
    /// This is *not* a wire format any more — `ANIM` was removed from `.vab`.
    /// `AnimContainer` survives as the input to the baker, and the ordering
    /// applied here (sprite id ascending, then `(frame, name)` ascending) is
    /// what makes the baked output deterministic.
    pub(crate) fn from_parts(
        animations: &HashMap<CharacterId, Vec<Vec<DisplayObject>>>,
        labels: &HashMap<Box<str>, usize>,
        frame_rate: f32,
    ) -> Self {
        let mut anim_entries: Vec<(u16, Vec<AnimFrame>)> = Vec::with_capacity(animations.len());
        for (&id, frames) in animations {
            let anim_frames: Vec<AnimFrame> = frames
                .iter()
                .map(|frame| AnimFrame {
                    entries: frame.iter().map(AnimDisplayObject::from).collect(),
                })
                .collect();
            anim_entries.push((id, anim_frames));
        }

        let mut label_list: Vec<(Box<str>, usize)> = Vec::with_capacity(labels.len());
        for (name, frame) in labels {
            label_list.push((name.clone(), *frame));
        }

        anim_entries.sort_by_key(|entry| entry.0);
        label_list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        AnimContainer {
            animations: anim_entries,
            labels: label_list,
            frame_rate,
        }
    }
}

impl From<&DisplayObject> for AnimDisplayObject {
    fn from(obj: &DisplayObject) -> Self {
        AnimDisplayObject {
            id: obj.id,
            name: obj.name.as_deref().map(String::from),
            depth: obj.depth,
            clip_depth: obj.clip_depth,
            blend_mode: obj.blend_mode as u8,
            transform: AnimTransform::from(&obj.transform),
            filters: obj.filters.iter().map(AnimFilter::from).collect(),
            ratio: obj.ratio,
            place_frame: obj.place_frame,
        }
    }
}

impl From<&Transform> for AnimTransform {
    fn from(t: &Transform) -> Self {
        AnimTransform {
            matrix: AnimMatrix {
                a: t.matrix.a,
                b: t.matrix.b,
                c: t.matrix.c,
                d: t.matrix.d,
                tx: t.matrix.tx.to_pixels() as f32,
                ty: t.matrix.ty.to_pixels() as f32,
            },
            color_transform: AnimColorTransform {
                r_multiply: t.color_transform.r_multiply.to_f32(),
                g_multiply: t.color_transform.g_multiply.to_f32(),
                b_multiply: t.color_transform.b_multiply.to_f32(),
                a_multiply: t.color_transform.a_multiply.to_f32(),
                r_add: f32::from(t.color_transform.r_add) / 255.0,
                g_add: f32::from(t.color_transform.g_add) / 255.0,
                b_add: f32::from(t.color_transform.b_add) / 255.0,
                a_add: f32::from(t.color_transform.a_add) / 255.0,
            },
        }
    }
}

impl From<&Filter> for AnimFilter {
    fn from(f: &Filter) -> Self {
        match f {
            Filter::BlurFilter(inner) => AnimFilter::BlurFilter(AnimBlurFilter {
                blur_x: inner.blur_x.get(),
                blur_y: inner.blur_y.get(),
                num_passes: inner.num_passes(),
            }),
            Filter::GlowFilter(inner) => AnimFilter::GlowFilter(AnimGlowFilter {
                flags: inner.flags.bits(),
                color_r: inner.color.r,
                color_g: inner.color.g,
                color_b: inner.color.b,
                color_a: inner.color.a,
                blur_x: inner.blur_x.get(),
                blur_y: inner.blur_y.get(),
                strength: inner.strength.get(),
                num_passes: inner.num_passes(),
            }),
            Filter::DropShadowFilter(inner) => AnimFilter::DropShadowFilter(AnimDropShadowFilter {
                flags: inner.flags.bits(),
                color_r: inner.color.r,
                color_g: inner.color.g,
                color_b: inner.color.b,
                color_a: inner.color.a,
                blur_x: inner.blur_x.get(),
                blur_y: inner.blur_y.get(),
                angle: inner.angle.get(),
                distance: inner.distance.get(),
                strength: inner.strength.get(),
                num_passes: inner.num_passes(),
            }),
            Filter::BevelFilter(inner) => AnimFilter::BevelFilter(AnimBevelFilter {
                flags: inner.flags.bits(),
                shadow_color_r: inner.shadow_color.r,
                shadow_color_g: inner.shadow_color.g,
                shadow_color_b: inner.shadow_color.b,
                shadow_color_a: inner.shadow_color.a,
                highlight_color_r: inner.highlight_color.r,
                highlight_color_g: inner.highlight_color.g,
                highlight_color_b: inner.highlight_color.b,
                highlight_color_a: inner.highlight_color.a,
                blur_x: inner.blur_x.get(),
                blur_y: inner.blur_y.get(),
                angle: inner.angle.get(),
                distance: inner.distance.get(),
                strength: inner.strength.get(),
                num_passes: inner.num_passes(),
            }),
            Filter::GradientGlowFilter(inner) => {
                AnimFilter::GradientGlowFilter(AnimGradientFilter {
                    flags: inner.flags.bits(),
                    colors: inner
                        .colors
                        .iter()
                        .map(|record| AnimGradientRecord {
                            ratio: record.ratio,
                            color_r: record.color.r,
                            color_g: record.color.g,
                            color_b: record.color.b,
                            color_a: record.color.a,
                        })
                        .collect(),
                    blur_x: inner.blur_x.get(),
                    blur_y: inner.blur_y.get(),
                    angle: inner.angle.get(),
                    distance: inner.distance.get(),
                    strength: inner.strength.get(),
                    num_passes: inner.num_passes(),
                })
            }
            Filter::ColorMatrixFilter(inner) => {
                AnimFilter::ColorMatrixFilter(AnimColorMatrixFilter {
                    matrix: inner.matrix,
                })
            }
            Filter::ConvolutionFilter(inner) => {
                AnimFilter::ConvolutionFilter(AnimConvolutionFilter {
                    num_matrix_rows: inner.num_matrix_rows,
                    num_matrix_cols: inner.num_matrix_cols,
                    matrix: inner.matrix.clone(),
                    divisor: inner.divisor,
                    bias: inner.bias,
                    default_color_r: inner.default_color.r,
                    default_color_g: inner.default_color.g,
                    default_color_b: inner.default_color.b,
                    default_color_a: inner.default_color.a,
                    flags: inner.flags.bits(),
                })
            }
            Filter::GradientBevelFilter(inner) => {
                AnimFilter::GradientBevelFilter(AnimGradientFilter {
                    flags: inner.flags.bits(),
                    colors: inner
                        .colors
                        .iter()
                        .map(|record| AnimGradientRecord {
                            ratio: record.ratio,
                            color_r: record.color.r,
                            color_g: record.color.g,
                            color_b: record.color.b,
                            color_a: record.color.a,
                        })
                        .collect(),
                    blur_x: inner.blur_x.get(),
                    blur_y: inner.blur_y.get(),
                    angle: inner.angle.get(),
                    distance: inner.distance.get(),
                    strength: inner.strength.get(),
                    num_passes: inner.num_passes(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Offscreen texture sizing for filters
// ---------------------------------------------------------------------------

/// PASS_SCALES from the swf crate (BlurFilter `calculate_dest_rect`).
const PASS_SCALES: [f64; 15] = [
    1.0, 2.1, 2.7, 3.1, 3.5, 3.8, 4.0, 4.2, 4.4, 4.6, 5.0, 6.0, 6.0, 7.0, 7.0,
];

/// Expand a pixel-space AABB `[off_x, off_y, w, h]` per filter, matching swf
/// crate's `calculate_dest_rect` math but operating directly in f64 pixels.
///
/// Returns expanded `(offset_x, offset_y, pixel_width, pixel_height)` for
/// offscreen texture sizing. Pure math, no swf crate dependency.
pub fn filter_dest_rect(
    off_x: f32,
    off_y: f32,
    w: f32,
    h: f32,
    filters: &[AnimFilter],
) -> (f32, f32, f32, f32) {
    let mut x0 = 0.0f64;
    let mut y0 = 0.0f64;
    let mut x1 = w as f64;
    let mut y1 = h as f64;

    // Blur radius in pixels, expanded by the cumulative pass scale — mirrors
    // the swf crate's `BlurFilter::calculate_dest_rect`.
    //
    // `raw_blur` is a Fixed16 (16.16) value, i.e. pixels * 65536.
    // `num_passes` is already the decoded pass count (1..15), not raw bitflags.
    let blur_expand = |raw_blur: i32, num_passes: u8| -> f64 {
        let pass_index = num_passes.clamp(1, 15) as usize - 1;
        (raw_blur as f64 / 65536.0).max(0.0) * PASS_SCALES[pass_index]
    };

    for filter in filters {
        match filter {
            AnimFilter::BlurFilter(f) => {
                let bx = blur_expand(f.blur_x, f.num_passes);
                let by = blur_expand(f.blur_y, f.num_passes);
                x0 -= bx;
                y0 -= by;
                x1 += bx;
                y1 += by;
            }
            AnimFilter::GlowFilter(f) => {
                let bx = blur_expand(f.blur_x, f.num_passes);
                let by = blur_expand(f.blur_y, f.num_passes);
                x0 -= bx;
                y0 -= by;
                x1 += bx;
                y1 += by;
            }
            AnimFilter::DropShadowFilter(f) => {
                let bx = blur_expand(f.blur_x, f.num_passes);
                let by = blur_expand(f.blur_y, f.num_passes);
                x0 -= bx;
                y0 -= by;
                x1 += bx;
                y1 += by;
                let distance = f.distance as f64 / 65536.0;
                let angle = f.angle as f64 / 65536.0;
                let dx = distance * angle.cos();
                let dy = distance * angle.sin();
                if dx < 0.0 {
                    x0 += dx;
                } else {
                    x1 += dx;
                }
                if dy < 0.0 {
                    y0 += dy;
                } else {
                    y1 += dy;
                }
            }
            AnimFilter::BevelFilter(f) => {
                let blur_x = blur_expand(f.blur_x, f.num_passes);
                let blur_y = blur_expand(f.blur_y, f.num_passes);
                x0 -= blur_x;
                y0 -= blur_y;
                x1 += blur_x;
                y1 += blur_y;
                // Bevel expands symmetrically by the offset distance.
                let distance = f.distance as f64 / 65536.0;
                let angle = f.angle as f64 / 65536.0;
                let dx = (angle.cos() * distance).abs();
                let dy = (angle.sin() * distance).abs();
                x0 -= dx;
                x1 += dx;
                y0 -= dy;
                y1 += dy;
            }
            AnimFilter::GradientGlowFilter(f) => {
                let bx = blur_expand(f.blur_x, f.num_passes);
                let by = blur_expand(f.blur_y, f.num_passes);
                x0 -= bx;
                y0 -= by;
                x1 += bx;
                y1 += by;
                let distance = f.distance as f64 / 65536.0;
                let angle = f.angle as f64 / 65536.0;
                let dx = distance * angle.cos();
                let dy = distance * angle.sin();
                if dx < 0.0 {
                    x0 += dx;
                } else {
                    x1 += dx;
                }
                if dy < 0.0 {
                    y0 += dy;
                } else {
                    y1 += dy;
                }
            }
            AnimFilter::GradientBevelFilter(f) => {
                let bx = blur_expand(f.blur_x, f.num_passes);
                let by = blur_expand(f.blur_y, f.num_passes);
                x0 -= bx;
                y0 -= by;
                x1 += bx;
                y1 += by;
                let distance = f.distance as f64 / 65536.0;
                let angle = f.angle as f64 / 65536.0;
                let dx = (distance * angle.cos()).abs();
                let dy = (distance * angle.sin()).abs();
                x0 -= dx;
                x1 += dx;
                y0 -= dy;
                y1 += dy;
            }
            AnimFilter::ColorMatrixFilter(_) | AnimFilter::ConvolutionFilter(_) => {}
        }
    }

    // Round the expanded rect out to whole pixels, matching the reference
    // player (`swf_player/src/render.rs`): floor the min, ceil the max, then
    // take the difference — `ceil(x1 - x0)` would under-size by up to 1 px.
    (
        off_x + x0.floor() as f32,
        off_y + y0.floor() as f32,
        (x1.ceil() - x0.floor()) as f32,
        (y1.ceil() - y0.floor()) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parent * local` must apply the **local** transform first. This is the
    /// rule the baker relies on when composing a display object's world
    /// transform (`baked.rs` `compose`).
    #[test]
    fn matrix_mul_applies_rhs_before_self() {
        let parent = AnimMatrix {
            tx: 10.0,
            ty: 20.0,
            ..AnimMatrix::IDENTITY
        };
        let local = AnimMatrix {
            tx: 1.0,
            ty: 2.0,
            ..AnimMatrix::IDENTITY
        };
        let composed = parent * local;
        assert_eq!(composed.tx, 11.0);
        assert_eq!(composed.ty, 22.0);

        // A parent scale must also scale the child's translation.
        let scaled_parent = AnimMatrix {
            a: 2.0,
            d: 2.0,
            ..AnimMatrix::IDENTITY
        };
        assert_eq!((scaled_parent * local).tx, 2.0);
    }

    /// Colour transforms follow the same "rhs first" rule:
    /// `add = self.add + self.multiply * rhs.add`.
    #[test]
    fn color_transform_mul_applies_rhs_before_self() {
        let parent = AnimColorTransform {
            r_multiply: 0.5,
            r_add: 0.25,
            ..AnimColorTransform::IDENTITY
        };
        let child = AnimColorTransform {
            r_multiply: 0.5,
            r_add: 0.1,
            ..AnimColorTransform::IDENTITY
        };
        let composed = parent * child;
        assert!((composed.r_multiply - 0.25).abs() < 1e-6);
        assert!((composed.r_add - 0.3).abs() < 1e-6);
    }

    fn gradient_filter(distance: i32) -> AnimGradientFilter {
        AnimGradientFilter {
            flags: 0,
            colors: Vec::new(),
            blur_x: 65_536,
            blur_y: 65_536,
            angle: 0,
            distance,
            strength: 256,
            num_passes: 1,
        }
    }

    #[test]
    fn gradient_filter_bounds_include_blur_and_offset() {
        let glow = filter_dest_rect(
            10.0,
            20.0,
            30.0,
            40.0,
            &[AnimFilter::GradientGlowFilter(gradient_filter(4 * 65_536))],
        );
        assert_eq!(glow, (9.0, 19.0, 36.0, 42.0));

        let bevel = filter_dest_rect(
            10.0,
            20.0,
            30.0,
            40.0,
            &[AnimFilter::GradientBevelFilter(gradient_filter(4 * 65_536))],
        );
        assert_eq!(bevel, (5.0, 19.0, 40.0, 42.0));
    }

    #[test]
    fn glow_modes_survive_encoding() {
        let original = Filter::GlowFilter(swf::GlowFilter {
            color: swf::Color::WHITE,
            blur_x: swf::Fixed16::ONE,
            blur_y: swf::Fixed16::ONE,
            strength: swf::Fixed8::ONE,
            flags: swf::GlowFilterFlags::from_bits_retain(0xe3),
        });
        let encoded = bincode::serialize(&AnimFilter::from(&original)).unwrap();
        let decoded: AnimFilter = bincode::deserialize(&encoded).unwrap();
        match decoded {
            AnimFilter::GlowFilter(filter) => assert_eq!(filter.flags, 0xe3),
            _ => panic!("wrong filter"),
        }
    }

    #[test]
    fn from_parts_empty() {
        let anims = HashMap::new();
        let labels = HashMap::new();
        let container = AnimContainer::from_parts(&anims, &labels, 24.0);
        assert!(container.animations.is_empty());
        assert!(container.labels.is_empty());
        assert_eq!(container.frame_rate, 24.0);
    }

    #[test]
    fn from_parts_single_frame() {
        use crate::transform::Transform;

        let mut anims = HashMap::new();
        let labels = HashMap::new();

        let obj = DisplayObject {
            id: 1,
            name: Some("test_mc".into()),
            depth: 1,
            clip_depth: 0,
            blend_mode: swf::BlendMode::Normal,
            transform: Transform {
                matrix: crate::matrix::Matrix::IDENTITY,
                color_transform: swf::ColorTransform::IDENTITY,
            },
            filters: Box::new([]),
            ratio: 0,
            place_frame: 0,
        };
        anims.insert(0u16, vec![vec![obj]]);

        let container = AnimContainer::from_parts(&anims, &labels, 30.0);
        assert_eq!(container.animations.len(), 1);
        let (id, frames) = &container.animations[0];
        assert_eq!(*id, 0);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].entries.len(), 1);

        let entry = &frames[0].entries[0];
        assert_eq!(entry.name.as_deref(), Some("test_mc"));
        assert_eq!(entry.depth, 1);
        assert_eq!(entry.id, 1);
        assert_eq!(entry.blend_mode, swf::BlendMode::Normal as u8);
    }

    /// The `(frame, name)` ordering imposed by `from_parts` is what makes the
    /// baked output deterministic, so assert the order rather than membership.
    #[test]
    fn from_parts_sorts_labels_by_frame_then_name() {
        let anims = HashMap::new();
        let mut labels = HashMap::new();
        labels.insert(Box::from("loop"), 24usize);
        labels.insert(Box::from("intro"), 0usize);
        labels.insert(Box::from("alpha"), 24usize);

        let container = AnimContainer::from_parts(&anims, &labels, 30.0);
        assert_eq!(
            container.labels,
            vec![
                (Box::from("intro"), 0usize),
                (Box::from("alpha"), 24usize),
                (Box::from("loop"), 24usize),
            ],
        );
    }

    /// Sprite ids are sorted ascending so that the baked clip/skin order does
    /// not depend on `HashMap` iteration order.
    #[test]
    fn from_parts_sorts_sprite_ids() {
        let mut anims = HashMap::new();
        anims.insert(7u16, Vec::new());
        anims.insert(2u16, Vec::new());
        anims.insert(5u16, Vec::new());

        let container = AnimContainer::from_parts(&anims, &HashMap::new(), 30.0);
        let ids: Vec<u16> = container.animations.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![2, 5, 7]);
    }
}
