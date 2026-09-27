//! Morph shape interpolation — taken from swf_player's morph_shape.rs.
//!
//! Interpolates between two `swf::MorphShape` definitions at a given ratio
//! (0–65535), producing a complete `swf::Shape` ready for tessellation.

use swf::{
    Color, FillStyle, Fixed8, Fixed16, Gradient, GradientRecord, LineStyle, Matrix, Point,
    Rectangle, ShapeRecord, ShapeStyles, Twips,
};
use tracing::warn;

use crate::shape_utils::calculate_shape_bounds;

/// Interpolate between the start and end of a morph shape at `ratio`.
///
/// `ratio` ranges from 0 (start) to 65535 (end).
pub fn interpolate(start: &swf::MorphShape, end: &swf::MorphShape, ratio: u16) -> swf::Shape {
    let b = f32::from(ratio) / 65535.0;
    let a = 1.0 - b;

    // ── Interpolate fill & line styles ─────────────────────────────────────
    let fill_styles: Vec<FillStyle> = start
        .fill_styles
        .iter()
        .zip(end.fill_styles.iter())
        .map(|(s, e)| lerp_fill(s, e, a, b))
        .collect();

    let line_styles: Vec<LineStyle> = start
        .line_styles
        .iter()
        .zip(end.line_styles.iter())
        .map(|(s, e)| {
            s.clone()
                .with_width(lerp_twips(s.width(), e.width(), a, b))
                .with_fill_style(lerp_fill(s.fill_style(), e.fill_style(), a, b))
        })
        .collect();

    // ── Interpolate shape records (edges) ──────────────────────────────────
    let mut shape = Vec::with_capacity(start.shape.len());
    let mut start_iter = start.shape.iter();
    let mut end_iter = end.shape.iter();
    let mut start_rec = start_iter.next();
    let mut end_rec = end_iter.next();
    let mut start_x = Twips::ZERO;
    let mut start_y = Twips::ZERO;
    let mut end_x = Twips::ZERO;
    let mut end_y = Twips::ZERO;

    while let (Some(s), Some(e)) = (start_rec, end_rec) {
        match (s, e) {
            (ShapeRecord::StyleChange(start_change), ShapeRecord::StyleChange(end_change)) => {
                let mut style_change = start_change.clone();
                if start_change.move_to.is_some() || end_change.move_to.is_some() {
                    if let Some(mv) = &start_change.move_to {
                        start_x = mv.x;
                        start_y = mv.y;
                    }
                    if let Some(mv) = &end_change.move_to {
                        end_x = mv.x;
                        end_y = mv.y;
                    }
                    style_change.move_to = Some(Point::new(
                        lerp_twips(start_x, end_x, a, b),
                        lerp_twips(start_y, end_y, a, b),
                    ));
                }
                shape.push(ShapeRecord::StyleChange(style_change));
                start_rec = start_iter.next();
                end_rec = end_iter.next();
            }
            (ShapeRecord::StyleChange(start_change), _) => {
                let mut style_change = start_change.clone();
                if let Some(mv) = &start_change.move_to {
                    start_x = mv.x;
                    start_y = mv.y;
                    style_change.move_to = Some(Point::new(
                        lerp_twips(start_x, end_x, a, b),
                        lerp_twips(start_y, end_y, a, b),
                    ));
                }
                shape.push(ShapeRecord::StyleChange(style_change));
                update_pos(&mut start_x, &mut start_y, s);
                start_rec = start_iter.next();
            }
            (_, ShapeRecord::StyleChange(end_change)) => {
                let mut style_change = end_change.clone();
                if let Some(mv) = &end_change.move_to {
                    end_x = mv.x;
                    end_y = mv.y;
                    style_change.move_to = Some(Point::new(
                        lerp_twips(start_x, end_x, a, b),
                        lerp_twips(start_y, end_y, a, b),
                    ));
                }
                shape.push(ShapeRecord::StyleChange(style_change));
                update_pos(&mut end_x, &mut end_y, e);
                end_rec = end_iter.next();
                continue;
            }
            _ => {
                shape.push(lerp_edges(
                    Point::new(start_x, start_y),
                    Point::new(end_x, end_y),
                    s,
                    e,
                    a,
                    b,
                ));
                update_pos(&mut start_x, &mut start_y, s);
                update_pos(&mut end_x, &mut end_y, e);
                start_rec = start_iter.next();
                end_rec = end_iter.next();
            }
        }
    }

    let styles = ShapeStyles {
        fill_styles,
        line_styles,
    };
    let shape_bounds = calculate_shape_bounds(&shape);
    // `edge_bounds` must include stroke widths. The interpolated edge records
    // carry no stroke half-widths, so interpolate the source bounds instead.
    let edge_bounds = lerp_rect(&start.edge_bounds, &end.edge_bounds, a, b);

    swf::Shape {
        version: 4,
        id: 0,
        shape_bounds,
        edge_bounds,
        flags: swf::ShapeFlag::HAS_SCALING_STROKES,
        styles,
        shape,
    }
}

/// Component-wise interpolation of a twips-space rectangle.
fn lerp_rect(start: &Rectangle<Twips>, end: &Rectangle<Twips>, a: f32, b: f32) -> Rectangle<Twips> {
    Rectangle {
        x_min: lerp_twips(start.x_min, end.x_min, a, b),
        x_max: lerp_twips(start.x_max, end.x_max, a, b),
        y_min: lerp_twips(start.y_min, end.y_min, a, b),
        y_max: lerp_twips(start.y_max, end.y_max, a, b),
    }
}

// ── Pen position tracking ──────────────────────────────────────────────────

fn update_pos(x: &mut Twips, y: &mut Twips, record: &ShapeRecord) {
    match record {
        ShapeRecord::StraightEdge { delta } => {
            *x += delta.dx;
            *y += delta.dy;
        }
        ShapeRecord::CurvedEdge {
            control_delta,
            anchor_delta,
        } => {
            *x += control_delta.dx + anchor_delta.dx;
            *y += control_delta.dy + anchor_delta.dy;
        }
        ShapeRecord::StyleChange(sc) => {
            if let Some(mv) = &sc.move_to {
                *x = mv.x;
                *y = mv.y;
            }
        }
    }
}

// ── Lerp helpers ───────────────────────────────────────────────────────────

fn lerp_color(start: &Color, end: &Color, a: f32, b: f32) -> Color {
    Color {
        r: (a * f32::from(start.r) + b * f32::from(end.r)) as u8,
        g: (a * f32::from(start.g) + b * f32::from(end.g)) as u8,
        b: (a * f32::from(start.b) + b * f32::from(end.b)) as u8,
        a: (a * f32::from(start.a) + b * f32::from(end.a)) as u8,
    }
}

fn lerp_twips(start: Twips, end: Twips, a: f32, b: f32) -> Twips {
    Twips::new((start.get() as f32 * a + end.get() as f32 * b).round() as i32)
}

fn lerp_point_twips(start: Point<Twips>, end: Point<Twips>, a: f32, b: f32) -> Point<Twips> {
    Point::new(
        lerp_twips(start.x, end.x, a, b),
        lerp_twips(start.y, end.y, a, b),
    )
}

fn lerp_fill(start: &FillStyle, end: &FillStyle, a: f32, b: f32) -> FillStyle {
    match (start, end) {
        (FillStyle::Color(s), FillStyle::Color(e)) => FillStyle::Color(lerp_color(s, e, a, b)),

        (
            FillStyle::Bitmap {
                id,
                matrix: sm,
                is_smoothed,
                is_repeating,
            },
            FillStyle::Bitmap { matrix: em, .. },
        ) => FillStyle::Bitmap {
            id: *id,
            matrix: lerp_matrix(sm, em, a, b),
            is_smoothed: *is_smoothed,
            is_repeating: *is_repeating,
        },

        (FillStyle::LinearGradient(s), FillStyle::LinearGradient(e)) => {
            FillStyle::LinearGradient(lerp_gradient(s, e, a, b))
        }
        (FillStyle::RadialGradient(s), FillStyle::RadialGradient(e)) => {
            FillStyle::RadialGradient(lerp_gradient(s, e, a, b))
        }
        (
            FillStyle::FocalGradient {
                gradient: sg,
                focal_point: sf,
            },
            FillStyle::FocalGradient {
                gradient: eg,
                focal_point: ef,
            },
        ) => FillStyle::FocalGradient {
            gradient: lerp_gradient(sg, eg, a, b),
            focal_point: *sf * Fixed8::from_f32(a) + *ef * Fixed8::from_f32(b),
        },

        _ => {
            warn!("Unexpected morph fill style combination: {start:#?}, {end:#?}");
            start.clone()
        }
    }
}

fn lerp_edges(
    start_pen: Point<Twips>,
    end_pen: Point<Twips>,
    start: &ShapeRecord,
    end: &ShapeRecord,
    a: f32,
    b: f32,
) -> ShapeRecord {
    let pen = lerp_point_twips(start_pen, end_pen, a, b);
    match (start, end) {
        (ShapeRecord::StraightEdge { delta: sd }, ShapeRecord::StraightEdge { delta: ed }) => {
            let anchor = lerp_point_twips(start_pen + *sd, end_pen + *ed, a, b);
            ShapeRecord::StraightEdge {
                delta: anchor - pen,
            }
        }

        (
            ShapeRecord::CurvedEdge {
                control_delta: sc,
                anchor_delta: sa,
            },
            ShapeRecord::CurvedEdge {
                control_delta: ec,
                anchor_delta: ea,
            },
        ) => {
            let control = lerp_point_twips(start_pen + *sc, end_pen + *ec, a, b);
            let anchor = lerp_point_twips(start_pen + *sc + *sa, end_pen + *ec + *ea, a, b);
            ShapeRecord::CurvedEdge {
                control_delta: control - pen,
                anchor_delta: anchor - control,
            }
        }

        (
            ShapeRecord::StraightEdge { delta: sd },
            ShapeRecord::CurvedEdge {
                control_delta: ec,
                anchor_delta: ea,
            },
        ) => {
            let start_control = start_pen + *sd / 2;
            let control = lerp_point_twips(start_control, end_pen + *ec, a, b);
            let anchor = lerp_point_twips(start_pen + *sd, end_pen + *ec + *ea, a, b);
            ShapeRecord::CurvedEdge {
                control_delta: control - pen,
                anchor_delta: anchor - control,
            }
        }

        (
            ShapeRecord::CurvedEdge {
                control_delta: sc,
                anchor_delta: sa,
            },
            ShapeRecord::StraightEdge { delta: ed },
        ) => {
            let control = lerp_point_twips(start_pen + *sc, end_pen + *ed / 2, a, b);
            let anchor = lerp_point_twips(start_pen + *sc + *sa, end_pen + *ed, a, b);
            ShapeRecord::CurvedEdge {
                control_delta: control - pen,
                anchor_delta: anchor - control,
            }
        }

        _ => unreachable!("morph edge mismatch: {start:?} <-> {end:?}"),
    }
}

fn lerp_matrix(start: &Matrix, end: &Matrix, a: f32, b: f32) -> Matrix {
    let af = Fixed16::from_f32(a);
    let bf = Fixed16::from_f32(b);
    Matrix {
        a: start.a * af + end.a * bf,
        b: start.b * af + end.b * bf,
        c: start.c * af + end.c * bf,
        d: start.d * af + end.d * bf,
        tx: lerp_twips(start.tx, end.tx, a, b),
        ty: lerp_twips(start.ty, end.ty, a, b),
    }
}

fn lerp_gradient(start: &Gradient, end: &Gradient, a: f32, b: f32) -> Gradient {
    debug_assert_eq!(start.records.len(), end.records.len());
    let records: Vec<GradientRecord> = start
        .records
        .iter()
        .zip(end.records.iter())
        .map(|(s, e)| GradientRecord {
            ratio: (f32::from(s.ratio) * a + f32::from(e.ratio) * b) as u8,
            color: lerp_color(&s.color, &e.color, a, b),
        })
        .collect();

    Gradient {
        matrix: lerp_matrix(&start.matrix, &end.matrix, a, b),
        spread: start.spread,
        interpolation: start.interpolation,
        records,
    }
}
