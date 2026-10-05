use std::{fs, path::PathBuf};
use swf::*;
use vatf::{baked::BakedNode, convert_swf_ui_to_vab, reader::VabReader};

fn shape(fill: FillStyle) -> Tag<'static> {
    let rect = Rectangle {
        x_min: Twips::from_pixels(10.0),
        y_min: Twips::from_pixels(20.0),
        x_max: Twips::from_pixels(110.0),
        y_max: Twips::from_pixels(60.0),
    };
    let mut records = vec![ShapeRecord::StyleChange(Box::new(StyleChangeData {
        move_to: Some(Point {
            x: rect.x_min,
            y: rect.y_min,
        }),
        fill_style_0: Some(1),
        fill_style_1: None,
        line_style: None,
        new_styles: None,
    }))];
    for (dx, dy) in [(100.0, 0.0), (0.0, 40.0), (-100.0, 0.0), (0.0, -40.0)] {
        records.push(ShapeRecord::StraightEdge {
            delta: PointDelta {
                dx: Twips::from_pixels(dx),
                dy: Twips::from_pixels(dy),
            },
        });
    }
    Tag::DefineShape(Shape {
        version: 3,
        id: 1,
        shape_bounds: rect.clone(),
        edge_bounds: rect,
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![fill],
            line_styles: vec![],
        },
        shape: records,
    })
}
fn place(id: u16) -> Tag<'static> {
    Tag::PlaceObject(Box::new(PlaceObject {
        version: 2,
        action: PlaceObjectAction::Place(id),
        depth: 1,
        matrix: Some(Matrix {
            tx: Twips::from_pixels(200.0),
            ty: Twips::from_pixels(300.0),
            ..Matrix::IDENTITY
        }),
        color_transform: None,
        ratio: None,
        name: None,
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
    }))
}
fn tags(frames: u16) -> Vec<Tag<'static>> {
    let mut children = vec![place(1)];
    children.extend((0..frames).map(|_| Tag::ShowFrame));
    vec![
        shape(FillStyle::Color(Color {
            r: 230,
            g: 130,
            b: 40,
            a: 255,
        })),
        Tag::DefineSprite(Sprite {
            id: 2,
            num_frames: frames,
            tags: children,
        }),
        Tag::ExportAssets(vec![
            ExportedAsset {
                id: 2,
                name: SwfStr::from_utf8_str("button_background"),
            },
            ExportedAsset {
                id: 1,
                name: SwfStr::from_utf8_str("mesh_0"),
            },
        ]),
        Tag::ShowFrame,
    ]
}
fn write_case(name: &str, tags: &[Tag<'_>]) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("vatf_ui_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let input = dir.join(format!("{name}.swf"));
    let output = dir.join(format!("{name}.vab"));
    let mut header = Header::default_with_swf_version(10);
    header.num_frames = 1;
    header.frame_rate = Fixed8::from_f32(24.0);
    write_swf(&header, tags, fs::File::create(&input).unwrap()).unwrap();
    (input, output)
}

#[test]
fn unused_and_replaced_ui_definitions_do_not_change_output() {
    let clean = tags(1);
    let mut dirty = tags(1);
    let mut unused = shape(FillStyle::Color(Color {
        r: 1,
        g: 2,
        b: 3,
        a: 255,
    }));
    if let Tag::DefineShape(shape) = &mut unused {
        shape.id = 3;
    }
    if let Tag::DefineSprite(sprite) = &mut dirty[1] {
        // It is a source dependency, but replaced before the only ShowFrame.
        sprite.tags.insert(0, place(3));
    }
    dirty.insert(0, unused);
    let (input, output) = write_case("pruned_ui", &dirty);
    let report = vatf::convert_swf_ui_to_vab_with_report(&input, &output).unwrap();
    let (baseline_input, baseline_output) = write_case("clean_ui", &clean);
    convert_swf_ui_to_vab(&baseline_input, &baseline_output).unwrap();
    assert_eq!((report.before.meshes, report.after.meshes), (2, 1));
    assert_eq!(
        fs::read(output).unwrap(),
        fs::read(baseline_output).unwrap()
    );
}

#[test]
fn unused_animation_definitions_do_not_change_baked_output() {
    let mut clean = tags(1);
    clean.insert(clean.len() - 1, place(2));
    let mut dirty = tags(1);
    dirty.insert(dirty.len() - 1, place(2));
    let mut unused = shape(FillStyle::Color(Color {
        r: 1,
        g: 2,
        b: 3,
        a: 255,
    }));
    if let Tag::DefineShape(shape) = &mut unused {
        shape.id = 3;
    }
    dirty.insert(0, unused);
    dirty.insert(dirty.len() - 2, place(3));
    let (input, output) = write_case("pruned_animation", &dirty);
    let report = vatf::convert_swf_to_vab_with_report(&input, &output).unwrap();
    let (baseline_input, baseline_output) = write_case("clean_animation", &clean);
    vatf::convert_swf_to_vab(&baseline_input, &baseline_output).unwrap();
    assert_eq!((report.before.meshes, report.after.meshes), (2, 1));
    assert_eq!(
        fs::read(output).unwrap(),
        fs::read(baseline_output).unwrap()
    );
}
#[test]
fn exported_symbols_work_without_stage_placement_and_are_centered() {
    let (input, output) = write_case("ui_demo", &tags(1));
    convert_swf_ui_to_vab(&input, &output).unwrap();
    let reader = VabReader::open(&output).unwrap();
    assert!(reader.baked().clips.is_empty());
    let graphics = reader.graphics().unwrap();
    assert_eq!(graphics.len(), 2);
    let graphic = &graphics[0];
    assert_eq!(graphic.name, "button_background");
    assert_eq!(graphic.source_bounds, [210.0, 320.0, 310.0, 360.0]);
    let BakedNode::Shape { transform, .. } = &graphic.frames[0][0] else {
        panic!("expected shape")
    };
    assert_eq!((transform.matrix.tx, transform.matrix.ty), (-60.0, -40.0));
    println!("UI fixture: {}", input.display());
}

fn native_button_tags(version: u8) -> Vec<Tag<'static>> {
    let records = [
        ButtonState::UP,
        ButtonState::OVER,
        ButtonState::DOWN,
        ButtonState::HIT_TEST,
    ]
    .into_iter()
    .zip([0.0, 100.0, 200.0, 1000.0])
    .map(|(states, x)| ButtonRecord {
        states,
        id: 1,
        depth: 2,
        matrix: Matrix {
            tx: Twips::from_pixels(x),
            ..Matrix::IDENTITY
        },
        color_transform: ColorTransform::default(),
        filters: vec![],
        blend_mode: BlendMode::Normal,
    })
    .collect();
    let button = Box::new(Button {
        id: 2,
        is_track_as_menu: false,
        records,
        actions: if version == 1 {
            vec![ButtonAction {
                conditions: ButtonActionCondition::OVER_DOWN_TO_OVER_UP,
                action_data: &[0],
            }]
        } else {
            vec![]
        },
    });
    vec![
        shape(FillStyle::Color(Color {
            r: 255,
            g: 30,
            b: 20,
            a: 255,
        })),
        if version == 1 {
            Tag::DefineButton(button)
        } else {
            Tag::DefineButton2(button)
        },
        Tag::ExportAssets(vec![ExportedAsset {
            id: 2,
            name: SwfStr::from_utf8_str("login"),
        }]),
        Tag::ShowFrame,
    ]
}
#[test]
fn native_buttons_keep_state_transforms_and_hit_is_not_a_display_state() {
    for version in [1, 2] {
        let (input, output) =
            write_case(&format!("button_v{version}"), &native_button_tags(version));
        convert_swf_ui_to_vab(&input, &output).unwrap();
        let reader = VabReader::open(&output).unwrap();
        let buttons = reader.buttons().unwrap();
        assert_eq!(buttons.len(), 1);
        let button = &buttons[0];
        assert_eq!(button.name, "login");
        assert_eq!(button.hit_test.as_deref(), Some("login/hit"));
        let graphics = reader.graphics().unwrap();
        for (label, tx) in [
            (&button.up, -160.0),
            (&button.over, -60.0),
            (&button.down, 40.0),
        ] {
            let graphic = graphics.iter().find(|g| &g.name == label).unwrap();
            assert_eq!(graphic.source_bounds, [10.0, 20.0, 310.0, 60.0]);
            assert_eq!(graphic.frames.len(), 1);
            let BakedNode::Shape { transform, .. } = graphic.frames[0][0] else {
                panic!("expected shape");
            };
            assert_eq!(transform.matrix.tx, tx);
            assert_eq!(transform.matrix.ty, -40.0);
        }
        let hit = graphics.iter().find(|g| g.name == "login/hit").unwrap();
        let BakedNode::Shape { transform, .. } = hit.frames[0][0] else {
            panic!("expected hit shape");
        };
        assert_eq!(transform.matrix.tx, 840.0);
    }
}
#[test]
fn native_button_missing_states_fallback_and_names_are_validated() {
    let mut tags = native_button_tags(2);
    if let Tag::DefineButton2(b) = &mut tags[1] {
        b.records.retain(|r| r.states == ButtonState::UP);
    }
    let (input, output) = write_case("button_fallback", &tags);
    convert_swf_ui_to_vab(&input, &output).unwrap();
    let buttons = VabReader::open(&output).unwrap().buttons().unwrap();
    assert_eq!(buttons[0].over, buttons[0].up);
    assert_eq!(buttons[0].down, buttons[0].up);
    assert!(buttons[0].hit_test.is_none());
    if let Tag::ExportAssets(entries) = &mut tags[2] {
        entries.push(ExportedAsset {
            id: 1,
            name: SwfStr::from_utf8_str("login/up"),
        });
    }
    let (input, output) = write_case("button_collision", &tags);
    assert!(
        format!("{:#}", convert_swf_ui_to_vab(&input, &output).unwrap_err()).contains("collision")
    );
}
#[test]
fn rejects_multiframe_and_recursive_children() {
    let (input, output) = write_case("animated", &tags(2));
    let error = format!("{:#}", convert_swf_ui_to_vab(&input, &output).unwrap_err());
    assert!(
        error.contains("button_background") && error.contains("exactly one frame"),
        "{error}"
    );
    let mut tags = tags(1);
    if let Tag::DefineSprite(sprite) = &mut tags[1] {
        sprite.tags[0] = place(2);
    }
    let (input, output) = write_case("recursive", &tags);
    assert!(
        format!("{:#}", convert_swf_ui_to_vab(&input, &output).unwrap_err()).contains("recursive")
    );
}
#[test]
fn rejects_duplicate_reserved_and_missing_export_names() {
    for name in ["button_background", "__vab/mesh_0", "bad#label", ""] {
        let mut tags = tags(1);
        if let Tag::ExportAssets(exports) = &mut tags[2] {
            exports.push(ExportedAsset {
                id: 1,
                name: SwfStr::from_utf8_str(name),
            });
        }
        let (input, output) = write_case(&format!("name_{}", name.len()), &tags);
        assert!(
            convert_swf_ui_to_vab(&input, &output).is_err(),
            "accepted {name:?}"
        );
    }
    let mut tags = tags(1);
    tags.remove(2);
    let (input, output) = write_case("no_exports", &tags);
    assert!(convert_swf_ui_to_vab(&input, &output).is_err());
}
#[test]
fn rejects_unsupported_bitmap_character() {
    let mut tags = tags(1);
    if let Tag::ExportAssets(exports) = &mut tags[2] {
        exports[0].id = 999;
    }
    let (input, output) = write_case("missing", &tags);
    assert!(format!("{:#}", convert_swf_ui_to_vab(&input, &output).unwrap_err()).contains("999"));
}

#[test]
fn rejects_bitmap_fills_before_decoding_and_ignores_unrelated_root() {
    let mut input_tags = tags(1);
    input_tags[0] = shape(FillStyle::Bitmap {
        id: 999,
        matrix: Matrix::IDENTITY,
        is_smoothed: true,
        is_repeating: false,
    });
    let (input, output) = write_case("bitmap_fill", &input_tags);
    let error = format!("{:#}", convert_swf_ui_to_vab(&input, &output).unwrap_err());
    assert!(
        error.contains("bitmap fill") && error.contains("button_background"),
        "{error}"
    );
    let mut input_tags = tags(1);
    input_tags.push(place(999)); // Invalid/unrelated root content is not a UI dependency.
    let (input, output) = write_case("root_ignored", &input_tags);
    convert_swf_ui_to_vab(&input, &output).unwrap();
    assert_eq!(
        VabReader::open(output).unwrap().graphics().unwrap().len(),
        2
    );
}

#[test]
fn editable_text_is_omitted_without_retaining_replaced_geometry() {
    let mut input_tags = tags(1);
    if let Tag::DefineSprite(sprite) = &mut input_tags[1] {
        let mut retained = place(1);
        if let Tag::PlaceObject(p) = &mut retained {
            p.depth = 2;
        }
        let mut text = place(7);
        if let Tag::PlaceObject(p) = &mut text {
            p.action = PlaceObjectAction::Replace(7);
        }
        sprite.tags = vec![place(1), text, retained, Tag::ShowFrame];
    }
    input_tags.insert(0, Tag::DefineEditText(Box::new(EditText::new().with_id(7))));
    let (input, output) = write_case("editable_text", &input_tags);
    convert_swf_ui_to_vab(&input, &output).unwrap();
    let graphics = VabReader::open(output).unwrap().graphics().unwrap();
    assert_eq!(
        graphics[0].frames[0].len(),
        1,
        "replaced shape must not remain"
    );
    assert_eq!(graphics[0].source_bounds, [210.0, 320.0, 310.0, 360.0]);
}

#[test]
fn stroke_vertices_are_not_clamped_to_edge_bounds() {
    let Tag::DefineShape(mut shape) = shape(FillStyle::Color(Color::WHITE)) else {
        unreachable!()
    };
    shape.version = 4;
    shape.styles.line_styles.push(
        LineStyle::new()
            .with_width(Twips::from_pixels(12.0))
            .with_color(Color::BLACK),
    );
    if let ShapeRecord::StyleChange(change) = &mut shape.shape[0] {
        change.line_style = Some(1);
    }
    // Even imperfect author-supplied bounds cannot clip tessellated strokes.
    let mut builder = vatf::VatfBuilder::default();
    builder.process_shape_geometry(&shape, &Default::default());
    let mesh = &builder.shape_meshes[0];
    assert!(mesh.bounds_center_x - mesh.bounds_half_x <= 4.01);
    assert!(mesh.bounds_center_x + mesh.bounds_half_x >= 115.99);
    assert!(mesh.bounds_center_y - mesh.bounds_half_y <= 14.01);
    assert!(mesh.bounds_center_y + mesh.bounds_half_y >= 65.99);
}

fn animated_tags(periods: &[u16]) -> Vec<Tag<'static>> {
    let mut result = vec![shape(FillStyle::Color(Color::WHITE))];
    let mut root = Vec::new();
    for (index, &length) in periods.iter().enumerate() {
        let id = index as u16 + 10;
        let mut timeline = Vec::new();
        for frame in 0..length {
            let mut tag = place(1);
            if let Tag::PlaceObject(p) = &mut tag {
                if frame != 0 {
                    p.action = PlaceObjectAction::Modify;
                }
                p.matrix.as_mut().unwrap().tx = Twips::from_pixels(frame as f64 * 10.0);
            }
            timeline.extend([tag, Tag::ShowFrame]);
        }
        result.push(Tag::DefineSprite(Sprite {
            id,
            num_frames: length,
            tags: timeline,
        }));
        let mut tag = place(id);
        if let Tag::PlaceObject(p) = &mut tag {
            p.depth = index as u16 + 1;
        }
        root.push(tag);
    }
    root.push(Tag::ShowFrame);
    result.extend([
        Tag::DefineSprite(Sprite {
            id: 2,
            num_frames: 1,
            tags: root,
        }),
        Tag::ExportAssets(vec![ExportedAsset {
            id: 2,
            name: SwfStr::from_utf8_str("sparkles"),
        }]),
        Tag::ShowFrame,
    ]);
    result
}
#[test]
fn animated_ui_lcm_advances_children_and_keeps_one_origin() {
    let (input, output) = write_case("animated_ui", &animated_tags(&[2, 3]));
    vatf::convert_swf_animated_ui_to_vab(&input, &output).unwrap();
    let g = VabReader::open(&output)
        .unwrap()
        .graphics()
        .unwrap()
        .remove(0);
    assert_eq!(g.frames.len(), 6);
    let translations: Vec<_> = g
        .frames
        .iter()
        .map(|nodes| {
            nodes
                .iter()
                .map(|n| match n {
                    BakedNode::Shape { transform, .. } => transform.matrix.tx,
                    _ => panic!("shape expected"),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for frame in 0..6 {
        assert_eq!(
            translations[frame][0] - translations[0][0],
            (frame % 2) as f32 * 10.0
        );
        assert_eq!(
            translations[frame][1] - translations[0][1],
            (frame % 3) as f32 * 10.0
        );
    }
    assert!(vatf::convert_swf_ui_to_vab(&input, &output).is_err());
    println!("animated fixture: {}", input.display());
}
#[test]
fn animated_ui_rejects_excessive_cycle() {
    let (input, output) = write_case("cycle_limit", &animated_tags(&[67, 71]));
    let error = format!(
        "{:#}",
        vatf::convert_swf_animated_ui_to_vab(&input, &output).unwrap_err()
    );
    assert!(error.contains("4096"), "{error}");
}

#[test]
fn animated_ui_single_frame_wrapper_preserves_time_and_parent_loop_resets_children() {
    let mut tags = animated_tags(&[2, 3]);
    tags.insert(
        0,
        Tag::DefineSprite(Sprite {
            id: 25,
            num_frames: 1,
            tags: vec![place(2), Tag::ShowFrame],
        }),
    );
    if let Tag::ExportAssets(exports) = tags
        .iter_mut()
        .find(|t| matches!(t, Tag::ExportAssets(_)))
        .unwrap()
    {
        exports[0].id = 25;
    }
    let (input, output) = write_case("nested_single", &tags);
    vatf::convert_swf_animated_ui_to_vab(&input, &output).unwrap();
    assert_eq!(
        VabReader::open(output).unwrap().graphics().unwrap()[0]
            .frames
            .len(),
        6
    );
    // The 4-frame parent resets the child's phase instead of requiring LCM(4,2,3).
    for tag in &mut tags {
        if let Tag::DefineSprite(s) = tag
            && s.id == 2
        {
            s.num_frames = 4;
            s.tags
                .extend([Tag::ShowFrame, Tag::ShowFrame, Tag::ShowFrame]);
        }
    }
    let (input, output) = write_case("parent_resets", &tags);
    vatf::convert_swf_animated_ui_to_vab(&input, &output).unwrap();
    let g = VabReader::open(output)
        .unwrap()
        .graphics()
        .unwrap()
        .remove(0);
    assert_eq!(g.frames.len(), 4);
    let tx = |frame: usize| match &g.frames[frame][1] {
        BakedNode::Shape { transform, .. } => transform.matrix.tx,
        _ => panic!(),
    };
    assert_eq!(tx(2) - tx(0), 20.0);
    assert_eq!(tx(3), tx(0));
}

#[test]
fn animated_ui_replacement_restarts_child_at_placement_frame() {
    let mut tags = animated_tags(&[3]);
    for tag in &mut tags {
        if let Tag::DefineSprite(sprite) = tag
            && sprite.id == 2
        {
            let mut replacement = place(10);
            if let Tag::PlaceObject(p) = &mut replacement {
                p.action = PlaceObjectAction::Replace(10);
            }
            sprite.num_frames = 4;
            sprite.tags = vec![
                place(10),
                Tag::ShowFrame,
                Tag::ShowFrame,
                replacement,
                Tag::ShowFrame,
                Tag::ShowFrame,
            ];
        }
    }
    let (input, output) = write_case("replacement_phase", &tags);
    vatf::convert_swf_animated_ui_to_vab(&input, &output).unwrap();
    let g = VabReader::open(output)
        .unwrap()
        .graphics()
        .unwrap()
        .remove(0);
    let tx = |i: usize| match &g.frames[i][0] {
        BakedNode::Shape { transform, .. } => transform.matrix.tx,
        _ => panic!(),
    };
    assert_eq!(g.frames.len(), 4);
    assert_eq!(tx(0), tx(2));
    assert_eq!(tx(1), tx(3));
    assert_eq!(tx(1) - tx(0), 10.0);
}
