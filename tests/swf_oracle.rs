//! SWF-oracle tests: independently walk a source SWF's `PlaceObject` /
//! `RemoveObject` / `ShowFrame` stream and compare the result against the
//! display lists produced by `convert_swf_to_vab`.
//!
//! The oracle is written from SWF semantics (an object stays on stage until an
//! explicit `RemoveObject`), **not** by copying `parse_tags`. That is what makes
//! it able to catch a compiler that emits per-frame deltas instead of the full
//! display list.
//!
//! The comparison runs against the *unexpanded*, per-sprite, local-space
//! timelines — the shape `AnimContainer` has. `BAKD` stores the expanded,
//! world-space tree instead, so comparing against it would mean re-deriving the
//! expansion here and would defeat the point of an independent oracle. The
//! container is therefore obtained in-process via
//! [`vatf::parse_animation_container`] rather than from a file chunk: `.vab` no
//! longer carries this data.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use swf::{BlendMode, CharacterId, Depth, PlaceObject, Tag};
use vatf::animation::{AnimContainer, AnimDisplayObject};
use vatf::reader::VabReader;

// ===========================================================================
// Oracle
// ===========================================================================

/// One display object as it should appear on stage, derived only from the SWF.
#[derive(Clone, Debug, PartialEq)]
struct ExpectedObject {
    id: CharacterId,
    depth: Depth,
    clip_depth: Depth,
    blend_mode: u8,
    ratio: u16,
    place_frame: u32,
    /// `[a, b, c, d, tx, ty]` with `tx`/`ty` in pixels.
    matrix: [f32; 6],
    /// `[r_mul, g_mul, b_mul, a_mul, r_add, g_add, b_add, a_add]`.
    color_transform: [f32; 8],
}

impl ExpectedObject {
    fn new(id: CharacterId) -> Self {
        Self {
            id,
            depth: 0,
            clip_depth: 0,
            blend_mode: BlendMode::Normal as u8,
            ratio: 0,
            place_frame: 0,
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            color_transform: [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    /// Applies only the fields the `PlaceObject` record actually specifies —
    /// omitted fields leave the existing instance untouched.
    fn apply_place_object(&mut self, place: &PlaceObject<'_>) {
        self.depth = place.depth;
        if let Some(clip_depth) = place.clip_depth {
            self.clip_depth = clip_depth;
        }
        if let Some(matrix) = place.matrix {
            self.matrix = [
                matrix.a.to_f32(),
                matrix.b.to_f32(),
                matrix.c.to_f32(),
                matrix.d.to_f32(),
                matrix.tx.to_pixels() as f32,
                matrix.ty.to_pixels() as f32,
            ];
        }
        if let Some(color_transform) = place.color_transform {
            self.color_transform = [
                color_transform.r_multiply.to_f32(),
                color_transform.g_multiply.to_f32(),
                color_transform.b_multiply.to_f32(),
                color_transform.a_multiply.to_f32(),
                f32::from(color_transform.r_add) / 255.0,
                f32::from(color_transform.g_add) / 255.0,
                f32::from(color_transform.b_add) / 255.0,
                f32::from(color_transform.a_add) / 255.0,
            ];
        }
        if let Some(ratio) = place.ratio {
            self.ratio = ratio;
        }
        if let Some(blend_mode) = place.blend_mode {
            self.blend_mode = blend_mode as u8;
        }
    }

    fn from_baked(baked: &AnimDisplayObject) -> Self {
        Self {
            id: baked.id,
            depth: baked.depth,
            clip_depth: baked.clip_depth,
            blend_mode: baked.blend_mode,
            ratio: baked.ratio,
            place_frame: baked.place_frame,
            matrix: [
                baked.transform.matrix.a,
                baked.transform.matrix.b,
                baked.transform.matrix.c,
                baked.transform.matrix.d,
                baked.transform.matrix.tx,
                baked.transform.matrix.ty,
            ],
            color_transform: [
                baked.transform.color_transform.r_multiply,
                baked.transform.color_transform.g_multiply,
                baked.transform.color_transform.b_multiply,
                baked.transform.color_transform.a_multiply,
                baked.transform.color_transform.r_add,
                baked.transform.color_transform.g_add,
                baked.transform.color_transform.b_add,
                baked.transform.color_transform.a_add,
            ],
        }
    }
}

/// Walks a tag list, producing the expected full display list per frame.
fn walk_tags(
    tags: Vec<Tag<'_>>,
    sprite_id: CharacterId,
    timelines: &mut HashMap<CharacterId, Vec<Vec<ExpectedObject>>>,
) {
    // Persistent across frames: an object placed on frame N is still on stage
    // on frame N+1 unless it is removed.
    let mut stage: BTreeMap<Depth, ExpectedObject> = BTreeMap::new();
    let mut timeline: Vec<Vec<ExpectedObject>> = Vec::new();

    for tag in tags {
        match tag {
            Tag::DefineSprite(sprite) => walk_tags(sprite.tags, sprite.id, timelines),
            Tag::PlaceObject(place) => match place.action {
                swf::PlaceObjectAction::Place(id) => {
                    let mut object = ExpectedObject::new(id);
                    object.apply_place_object(&place);
                    object.place_frame = timeline.len() as u32;
                    stage.insert(place.depth, object);
                }
                swf::PlaceObjectAction::Modify => {
                    if let Some(object) = stage.get_mut(&place.depth) {
                        object.apply_place_object(&place);
                    }
                }
                swf::PlaceObjectAction::Replace(id) => {
                    if let Some(object) = stage.get_mut(&place.depth) {
                        object.id = id;
                        object.apply_place_object(&place);
                        object.place_frame = timeline.len() as u32;
                    }
                }
            },
            Tag::RemoveObject(remove) => {
                stage.remove(&remove.depth);
            }
            Tag::ShowFrame => timeline.push(stage.values().cloned().collect()),
            _ => {}
        }
    }

    timelines.insert(sprite_id, timeline);
}

/// Parses `path` and returns the oracle's expectation of every sprite timeline.
fn oracle_from_swf(path: &Path) -> (HashMap<CharacterId, Vec<Vec<ExpectedObject>>>, u16) {
    let file = std::fs::File::open(path).expect("failed to open SWF fixture");
    let buffer = swf::decompress_swf(std::io::BufReader::new(file)).expect("decompress failed");
    let movie = swf::parse_swf(&buffer).expect("parse failed");

    let mut timelines = HashMap::new();
    walk_tags(movie.tags, 0, &mut timelines);
    (timelines, movie.header.num_frames())
}

// ===========================================================================
// Helpers
// ===========================================================================

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Converts a fixture into a temporary `.vab`, reads it back, and separately
/// obtains the unexpanded timelines for the oracle to compare against.
fn bake_and_read(swf_path: &Path, tag: &str) -> (VabReader, AnimContainer) {
    let output = std::env::temp_dir()
        .join("vatf_oracle")
        .join(format!("{tag}.vab"));
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();

    vatf::convert_swf_to_vab(swf_path, &output).expect("conversion failed");
    let reader = VabReader::open(&output).expect("failed to read back the baked VAB");
    let container = vatf::parse_animation_container(swf_path).expect("oracle container failed");
    (reader, container)
}

const FLOAT_TOLERANCE: f32 = 1e-4;

fn assert_close(actual: f32, expected: f32, context: &str) {
    assert!(
        (actual - expected).abs() <= FLOAT_TOLERANCE,
        "{context}: expected {expected}, got {actual}",
    );
}

/// Mirrors the reference player's whole-pixel rounding of a filter dest rect
/// (`swf_player/src/render.rs`): floor the min, ceil the max.
fn rounded_rect(rect: &swf::Rectangle<swf::Twips>) -> (f32, f32, f32, f32) {
    let x_min = rect.x_min.to_pixels().floor();
    let x_max = rect.x_max.to_pixels().ceil();
    let y_min = rect.y_min.to_pixels().floor();
    let y_max = rect.y_max.to_pixels().ceil();
    (
        x_min as f32,
        y_min as f32,
        (x_max - x_min) as f32,
        (y_max - y_min) as f32,
    )
}

/// Compares every baked sprite timeline against the oracle.
fn assert_baked_matches_oracle(
    container: &AnimContainer,
    expected: &HashMap<CharacterId, Vec<Vec<ExpectedObject>>>,
) {
    assert_eq!(
        container.animations.len(),
        expected.len(),
        "sprite count mismatch",
    );

    for (sprite_id, expected_frames) in expected {
        let baked_frames = container
            .animations
            .iter()
            .find(|(id, _)| id == sprite_id)
            .map(|(_, frames)| frames)
            .unwrap_or_else(|| panic!("sprite {sprite_id} missing from the parsed timelines"));

        assert_eq!(
            baked_frames.len(),
            expected_frames.len(),
            "sprite {sprite_id}: frame count mismatch",
        );

        for (frame_index, (baked_frame, expected_frame)) in
            baked_frames.iter().zip(expected_frames).enumerate()
        {
            let context = format!("sprite {sprite_id} frame {frame_index}");
            assert_eq!(
                baked_frame.entries.len(),
                expected_frame.len(),
                "{context}: object count mismatch (baked ids {:?} vs expected ids {:?})",
                baked_frame.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
                expected_frame.iter().map(|e| e.id).collect::<Vec<_>>(),
            );

            for (baked, expected_object) in baked_frame.entries.iter().zip(expected_frame) {
                let actual = ExpectedObject::from_baked(baked);
                assert_eq!(actual.id, expected_object.id, "{context}: id");
                assert_eq!(actual.depth, expected_object.depth, "{context}: depth");
                assert_eq!(
                    actual.clip_depth, expected_object.clip_depth,
                    "{context}: clip_depth",
                );
                assert_eq!(
                    actual.blend_mode, expected_object.blend_mode,
                    "{context}: blend_mode",
                );
                assert_eq!(actual.ratio, expected_object.ratio, "{context}: ratio");
                assert_eq!(
                    actual.place_frame, expected_object.place_frame,
                    "{context}: place_frame",
                );

                for (index, (actual_value, expected_value)) in actual
                    .matrix
                    .iter()
                    .zip(expected_object.matrix.iter())
                    .enumerate()
                {
                    assert_close(
                        *actual_value,
                        *expected_value,
                        &format!("{context}: matrix[{index}]"),
                    );
                }
                for (index, (actual_value, expected_value)) in actual
                    .color_transform
                    .iter()
                    .zip(expected_object.color_transform.iter())
                    .enumerate()
                {
                    assert_close(
                        *actual_value,
                        *expected_value,
                        &format!("{context}: color_transform[{index}]"),
                    );
                }
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// The regression guard for the delta bug: a `PlaceObject` persists across
/// every later frame even when the later frames only carry `Modify` tags.
///
/// The SWF is synthesized here so the test is independent of any fixture.
#[test]
fn display_list_persists_objects_across_frames() {
    use swf::{Fixed16, Matrix, PlaceObjectAction, Twips};

    let mut tags = Vec::new();
    // This test exercises timeline persistence, but still defines its referenced character.
    tags.push(Tag::DefineShape(swf::Shape {
        version: 3,
        id: 1,
        shape_bounds: swf::Rectangle::default(),
        edge_bounds: swf::Rectangle::default(),
        flags: swf::ShapeFlag::empty(),
        styles: swf::ShapeStyles {
            fill_styles: vec![],
            line_styles: vec![],
        },
        shape: vec![],
    }));
    for frame in 0..4u32 {
        if frame == 0 {
            tags.push(Tag::FrameLabel(swf::FrameLabel {
                label: swf::SwfStr::from_utf8_str("anim_idle"),
                is_anchor: false,
            }));
        }
        if frame == 1 || frame == 3 {
            tags.push(Tag::FrameLabel(swf::FrameLabel {
                label: swf::SwfStr::from_utf8_str("event_hit"),
                is_anchor: false,
            }));
        }
        let matrix = Matrix {
            a: Fixed16::ONE,
            b: Fixed16::ZERO,
            c: Fixed16::ZERO,
            d: Fixed16::ONE,
            tx: Twips::from_pixels(frame as f64 * 10.0),
            ty: Twips::ZERO,
        };
        tags.push(Tag::PlaceObject(Box::new(PlaceObject {
            version: 2,
            action: if frame == 0 {
                PlaceObjectAction::Place(1)
            } else {
                PlaceObjectAction::Modify
            },
            depth: 1,
            matrix: Some(matrix),
            color_transform: None,
            ratio: None,
            name: (frame == 0).then(|| swf::SwfStr::from_utf8_str("hand_part")),
            clip_depth: None,
            class_name: None,
            filters: None,
            background_color: None,
            blend_mode: None,
            clip_actions: None,
            has_image: false,
            is_bitmap_cached: None,
            is_visible: None,
            amf_data: None,
        })));
        tags.push(Tag::ShowFrame);
    }

    let header = swf::Header {
        compression: swf::Compression::None,
        version: 6,
        stage_size: swf::Rectangle {
            x_min: Twips::ZERO,
            x_max: Twips::from_pixels(550.0),
            y_min: Twips::ZERO,
            y_max: Twips::from_pixels(400.0),
        },
        frame_rate: swf::Fixed8::from_f32(24.0),
        num_frames: 4,
    };

    let directory = std::env::temp_dir().join("vatf_oracle");
    std::fs::create_dir_all(&directory).unwrap();
    let swf_path = directory.join("persistence.swf");
    let vab_path = directory.join("persistence.vab");

    let file = std::fs::File::create(&swf_path).unwrap();
    swf::write::write_swf(&header, &tags, file).unwrap();

    vatf::convert_swf_to_vab(&swf_path, &vab_path).unwrap();
    let reader = VabReader::open(&vab_path).unwrap();
    let container = vatf::parse_animation_container(&swf_path).unwrap();

    let (root_id, frames) = container
        .animations
        .iter()
        .find(|(id, _)| *id == 0)
        .expect("root timeline missing");

    assert_eq!(*root_id, 0);
    assert_eq!(frames.len(), 4, "expected four baked frames");
    assert_eq!(container.frame_rate, 24.0, "frame rate must be baked");
    let clip = &reader.baked().clips[0];
    assert_eq!(clip.name, "idle");
    assert_eq!(
        clip.events
            .iter()
            .map(|e| (e.frame, e.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "hit"), (3, "hit")]
    );

    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.entries.len(),
            1,
            "frame {index} must still contain the placed object",
        );
        let entry = &frame.entries[0];
        assert_eq!(entry.id, 1, "frame {index}: id");
        assert_eq!(entry.name.as_deref(), Some("hand_part"));
        assert_eq!(entry.place_frame, 0, "frame {index}: place_frame");
        assert_close(
            entry.transform.matrix.tx,
            index as f32 * 10.0,
            &format!("frame {index}: tx"),
        );
    }
}

/// End-to-end oracle comparison on the real sample asset.
#[test]
fn oracle_matches_baked_sample() {
    let swf_path = fixture_directory().join("spirit2159src.swf");
    let (_reader, container) = bake_and_read(&swf_path, "spirit2159src");

    let (expected, num_frames) = oracle_from_swf(&swf_path);

    let root_frame_count = container
        .animations
        .iter()
        .find(|(id, _)| *id == 0)
        .map(|(_, frames)| frames.len())
        .expect("root timeline missing");
    assert_eq!(
        root_frame_count, num_frames as usize,
        "root timeline length must equal the SWF's frame count",
    );

    assert_baked_matches_oracle(&container, &expected);
}

/// Blur/glow/drop-shadow/bevel dest-rect math must match the `swf` crate.
#[test]
fn filter_dest_rect_matches_swf_crate() {
    use swf::{Fixed16, Rectangle, Twips};
    use vatf::animation::{AnimBlurFilter, AnimDropShadowFilter, AnimFilter};

    let source = Rectangle {
        x_min: Twips::ZERO,
        x_max: Twips::from_pixels(100.0),
        y_min: Twips::ZERO,
        y_max: Twips::from_pixels(50.0),
    };

    let blur_x = 4.0f32;
    let blur_y = 2.0f32;

    for passes in [1u8, 3, 7, 15] {
        // ── Blur ──────────────────────────────────────────────────────────
        let swf_blur = swf::BlurFilter {
            blur_x: Fixed16::from_f32(blur_x),
            blur_y: Fixed16::from_f32(blur_y),
            flags: swf::BlurFilterFlags::from_passes(passes),
        };
        let expected = swf_blur.calculate_dest_rect(source.clone());

        let anim_blur = AnimFilter::BlurFilter(AnimBlurFilter {
            blur_x: (blur_x * 65536.0) as i32,
            blur_y: (blur_y * 65536.0) as i32,
            num_passes: passes,
        });
        let (offset_x, offset_y, width, height) =
            vatf::animation::filter_dest_rect(0.0, 0.0, 100.0, 50.0, &[anim_blur]);

        let (expected_x, expected_y, expected_width, expected_height) = rounded_rect(&expected);
        assert_close(offset_x, expected_x, "blur offset_x");
        assert_close(offset_y, expected_y, "blur offset_y");
        assert_close(width, expected_width, "blur width");
        assert_close(height, expected_height, "blur height");

        // ── Drop shadow (blur + directional offset) ───────────────────────
        let swf_shadow = swf::DropShadowFilter {
            color: swf::Color::WHITE,
            blur_x: Fixed16::from_f32(blur_x),
            blur_y: Fixed16::from_f32(blur_y),
            angle: Fixed16::from_f32(0.5),
            distance: Fixed16::from_f32(6.0),
            strength: swf::Fixed8::ONE,
            flags: swf::DropShadowFilterFlags::from_passes(passes),
        };
        let expected_shadow = swf_shadow.calculate_dest_rect(source.clone());

        let anim_shadow = AnimFilter::DropShadowFilter(AnimDropShadowFilter {
            flags: swf_shadow.flags.bits(),
            color_r: 255,
            color_g: 255,
            color_b: 255,
            color_a: 255,
            blur_x: (blur_x * 65536.0) as i32,
            blur_y: (blur_y * 65536.0) as i32,
            angle: (0.5 * 65536.0) as i32,
            distance: (6.0 * 65536.0) as i32,
            strength: 256,
            num_passes: passes,
        });
        let (offset_x, offset_y, width, height) =
            vatf::animation::filter_dest_rect(0.0, 0.0, 100.0, 50.0, &[anim_shadow]);

        let (expected_x, expected_y, expected_width, expected_height) =
            rounded_rect(&expected_shadow);
        assert_close(offset_x, expected_x, "shadow offset_x");
        assert_close(offset_y, expected_y, "shadow offset_y");
        assert_close(width, expected_width, "shadow width");
        assert_close(height, expected_height, "shadow height");

        // ── Bevel (blur + symmetric offset) ───────────────────────────────
        let swf_bevel = swf::BevelFilter {
            shadow_color: swf::Color::BLACK,
            highlight_color: swf::Color::WHITE,
            blur_x: Fixed16::from_f32(blur_x),
            blur_y: Fixed16::from_f32(blur_y),
            angle: Fixed16::from_f32(0.5),
            distance: Fixed16::from_f32(6.0),
            strength: swf::Fixed8::ONE,
            flags: swf::BevelFilterFlags::from_passes(passes),
        };
        let expected_bevel = swf_bevel.calculate_dest_rect(source.clone());

        let anim_bevel = AnimFilter::BevelFilter(vatf::animation::AnimBevelFilter {
            flags: swf_bevel.flags.bits(),
            shadow_color_r: 0,
            shadow_color_g: 0,
            shadow_color_b: 0,
            shadow_color_a: 255,
            highlight_color_r: 255,
            highlight_color_g: 255,
            highlight_color_b: 255,
            highlight_color_a: 255,
            blur_x: (blur_x * 65536.0) as i32,
            blur_y: (blur_y * 65536.0) as i32,
            angle: (0.5 * 65536.0) as i32,
            distance: (6.0 * 65536.0) as i32,
            strength: 256,
            num_passes: passes,
        });
        let (offset_x, offset_y, width, height) =
            vatf::animation::filter_dest_rect(0.0, 0.0, 100.0, 50.0, &[anim_bevel]);

        let (expected_x, expected_y, expected_width, expected_height) =
            rounded_rect(&expected_bevel);
        assert_close(offset_x, expected_x, "bevel offset_x");
        assert_close(offset_y, expected_y, "bevel offset_y");
        assert_close(width, expected_width, "bevel width");
        assert_close(height, expected_height, "bevel height");
    }
}
