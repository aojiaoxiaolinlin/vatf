use crate::{VatfBuilder, animation::AnimContainer, baked::BakedNode};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Reserved for loader-owned meshes, materials and render assets.
pub const INTERNAL_LABEL_PREFIX: &str = "__vab/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graphic {
    pub name: String,
    /// Original symbol-local geometric AABB: xmin, ymin, xmax, ymax, in pixels.
    pub source_bounds: [f32; 4],
    /// Static nodes centered on the geometry bounds, in Flash's Y-down space.
    pub frames: Vec<Vec<BakedNode>>,
    pub frame_rate: f32,
}

/// Named native button; state graphics share one registration/layout rectangle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Button {
    pub name: String,
    pub up: String,
    pub over: String,
    pub down: String,
    pub hit_test: Option<String>,
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && !name.contains('#')
            && !name.chars().any(char::is_control)
            && !name.starts_with(INTERNAL_LABEL_PREFIX),
        "invalid/reserved UI export name {name:?}"
    );
    Ok(())
}

#[derive(Default)]
pub(crate) struct Sources {
    exports: BTreeMap<String, u16>,
    allowed: HashSet<u16>,
    scripted: HashSet<u16>,
    bitmap_shapes: HashSet<u16>,
    editable_texts: HashSet<u16>,
    animated: bool,
    buttons: Vec<Button>,
    button_states: BTreeMap<u16, Vec<swf::ButtonRecord>>,
    reserved: HashSet<u16>,
}
impl Sources {
    pub fn collect(tags: &[swf::Tag<'_>], animated: bool) -> Result<Self> {
        fn visit(tags: &[swf::Tag<'_>], owner: u16, out: &mut Sources) -> Result<()> {
            for tag in tags {
                match tag {
                    swf::Tag::DefineButton(button) | swf::Tag::DefineButton2(button) => {
                        out.reserved.insert(button.id);
                        out.reserved.extend(button.records.iter().map(|r| r.id));
                        out.button_states.insert(button.id, button.records.clone());
                    }
                    swf::Tag::ExportAssets(entries) => {
                        for entry in entries {
                            let name = entry.name.to_str_lossy(swf::UTF_8).into_owned();
                            validate_name(&name)?;
                            ensure!(
                                out.exports.insert(name.clone(), entry.id).is_none(),
                                "duplicate UI export {name:?}"
                            );
                        }
                    }
                    swf::Tag::DefineShape(shape) => {
                        out.allowed.insert(shape.id);
                        let has_bitmap = |styles: &swf::ShapeStyles| {
                            styles
                                .fill_styles
                                .iter()
                                .chain(styles.line_styles.iter().map(|line| line.fill_style()))
                                .any(|fill| matches!(fill, swf::FillStyle::Bitmap { .. }))
                        };
                        if has_bitmap(&shape.styles) || shape.shape.iter().any(|record| {
                            matches!(record, swf::ShapeRecord::StyleChange(change) if change.new_styles.as_ref().is_some_and(has_bitmap))
                        }) { out.bitmap_shapes.insert(shape.id); }
                    }
                    swf::Tag::DefineEditText(text) => {
                        out.editable_texts.insert(text.id());
                    }
                    swf::Tag::DefineSprite(sprite) => {
                        out.allowed.insert(sprite.id);
                        visit(&sprite.tags, sprite.id, out)?;
                    }
                    swf::Tag::DoInitAction { id, .. } => {
                        out.scripted.insert(*id);
                    }
                    swf::Tag::PlaceObject(place) if owner != 0 && place.clip_actions.is_some() => {
                        out.scripted.insert(owner);
                    }
                    swf::Tag::DoAction(_) if owner != 0 => {
                        out.scripted.insert(owner);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        let mut result = Self {
            animated,
            ..Self::default()
        };
        visit(tags, 0, &mut result)?;
        ensure!(
            !result.exports.is_empty(),
            "UI conversion requires ExportAssets entries"
        );
        result.reserved.extend(result.allowed.iter().copied());
        result.reserved.extend(result.exports.values().copied());
        result
            .reserved
            .extend(result.editable_texts.iter().copied());
        fn reserve(tags: &[swf::Tag<'_>], ids: &mut HashSet<u16>) {
            for tag in tags {
                match tag {
                    swf::Tag::PlaceObject(p) => {
                        if let swf::PlaceObjectAction::Place(id)
                        | swf::PlaceObjectAction::Replace(id) = p.action
                        {
                            ids.insert(id);
                        }
                    }
                    swf::Tag::DefineSprite(s) => reserve(&s.tags, ids),
                    _ => {}
                }
            }
        }
        reserve(tags, &mut result.reserved);
        let native = std::mem::take(&mut result.button_states);
        for (name, id) in result.exports.clone() {
            let Some(records) = native.get(&id) else {
                continue;
            };
            result.exports.remove(&name);
            let mut names = Vec::new();
            for (suffix, state) in [
                ("up", swf::ButtonState::UP),
                ("over", swf::ButtonState::OVER),
                ("down", swf::ButtonState::DOWN),
                ("hit", swf::ButtonState::HIT_TEST),
            ] {
                let mut selected: Vec<_> = records
                    .iter()
                    .filter(|r| r.states.contains(state))
                    .cloned()
                    .collect();
                selected.sort_by_key(|r| r.depth);
                ensure!(
                    selected.windows(2).all(|w| w[0].depth != w[1].depth),
                    "button {name:?} has duplicate {suffix} depths"
                );
                if selected.is_empty() {
                    ensure!(suffix != "up", "button {name:?} has no up state");
                    names.push(None);
                    continue;
                }
                let label = format!("{name}/{suffix}");
                ensure!(
                    !result.exports.contains_key(&label),
                    "button state label collision {label:?}"
                );
                let synthetic = (1..=u16::MAX)
                    .rev()
                    .find(|id| !result.reserved.contains(id))
                    .context("no free character id for button state")?;
                result.reserved.insert(synthetic);
                result.allowed.insert(synthetic);
                result.exports.insert(label.clone(), synthetic);
                result.button_states.insert(synthetic, selected);
                names.push(Some(label));
            }
            let up = names[0].clone().unwrap();
            result.buttons.push(Button {
                name,
                up: up.clone(),
                over: names[1].clone().unwrap_or_else(|| up.clone()),
                down: names[2].clone().unwrap_or_else(|| up.clone()),
                hit_test: names[3].clone(),
            });
        }
        Ok(result)
    }

    /// Collect only definitions reachable from exported UI symbols. The root
    /// timeline and unrelated animation/bitmap resources do not enter the UI file.
    pub fn select<'a>(&self, tags: Vec<swf::Tag<'a>>) -> Result<Vec<swf::Tag<'a>>> {
        fn definitions<'a>(tags: Vec<swf::Tag<'a>>, out: &mut BTreeMap<u16, swf::Tag<'a>>) {
            for mut tag in tags {
                match &mut tag {
                    swf::Tag::DefineShape(shape) => {
                        out.insert(shape.id, tag);
                    }
                    swf::Tag::DefineSprite(sprite) => {
                        let (nested, timeline): (Vec<_>, Vec<_>) = std::mem::take(&mut sprite.tags)
                            .into_iter()
                            .partition(|tag| {
                                matches!(tag, swf::Tag::DefineShape(_) | swf::Tag::DefineSprite(_))
                            });
                        definitions(nested, out);
                        sprite.tags = timeline;
                        out.insert(sprite.id, tag);
                    }
                    _ => {}
                }
            }
        }
        fn visit(
            id: u16,
            source: &Sources,
            defs: &BTreeMap<u16, swf::Tag<'_>>,
            active: &mut Vec<u16>,
            used: &mut HashSet<u16>,
        ) -> Result<()> {
            ensure!(
                id != 0 && active.len() < 128 && !active.contains(&id),
                "recursive/deep UI dependency {active:?} -> {id}"
            );
            if used.contains(&id) {
                return Ok(());
            }
            ensure!(
                !source.scripted.contains(&id),
                "Sprite {id} contains ActionScript"
            );
            ensure!(
                !source.bitmap_shapes.contains(&id),
                "Shape {id} contains bitmap fill"
            );
            let tag = defs.get(&id).with_context(|| format!("character {id} is not a Shape or Sprite (bitmap/text/morph/button/import unsupported)"))?;
            active.push(id);
            if let swf::Tag::DefineSprite(sprite) = tag {
                let frames = sprite
                    .tags
                    .iter()
                    .filter(|tag| matches!(tag, swf::Tag::ShowFrame))
                    .count();
                ensure!(
                    frames > 0
                        && sprite.num_frames as usize == frames
                        && (source.animated || frames == 1),
                    "Sprite {id} must have exactly one frame, got {} declared/{frames} actual",
                    sprite.num_frames
                );
                for tag in &sprite.tags {
                    if let swf::Tag::PlaceObject(place) = tag
                        && let swf::PlaceObjectAction::Place(child)
                        | swf::PlaceObjectAction::Replace(child) = place.action
                    {
                        if source.editable_texts.contains(&child) {
                            continue;
                        }
                        visit(child, source, defs, active, used)
                            .with_context(|| format!("Sprite {id} -> character {child}"))?;
                    }
                }
            }
            active.pop();
            used.insert(id);
            Ok(())
        }
        let mut defs = BTreeMap::new();
        definitions(tags, &mut defs);
        for (id, records) in &self.button_states {
            let mut tags = Vec::new();
            for r in records {
                tags.push(swf::Tag::PlaceObject(Box::new(swf::PlaceObject {
                    version: 3,
                    action: swf::PlaceObjectAction::Place(r.id),
                    depth: r.depth,
                    matrix: Some(r.matrix),
                    color_transform: Some(r.color_transform),
                    filters: Some(r.filters.clone()),
                    blend_mode: Some(r.blend_mode),
                    ratio: None,
                    name: None,
                    clip_depth: None,
                    class_name: None,
                    background_color: None,
                    clip_actions: None,
                    has_image: false,
                    is_bitmap_cached: None,
                    is_visible: None,
                    amf_data: None,
                })));
            }
            tags.push(swf::Tag::ShowFrame);
            defs.insert(
                *id,
                swf::Tag::DefineSprite(swf::Sprite {
                    id: *id,
                    num_frames: 1,
                    tags,
                }),
            );
        }
        let mut used = HashSet::new();
        for (name, id) in &self.exports {
            visit(*id, self, &defs, &mut Vec::new(), &mut used)
                .with_context(|| format!("UI export {name:?}"))?;
        }
        Ok(defs
            .into_iter()
            .filter(|(id, _)| used.contains(id))
            .map(|(_, mut tag)| {
                if let swf::Tag::DefineSprite(sprite) = &mut tag {
                    // A text placement still replaces the old occupant at this depth.
                    // Remove it rather than skipping the tag and retaining stale geometry.
                    for tag in &mut sprite.tags {
                        if let swf::Tag::PlaceObject(place) = tag
                            && let swf::PlaceObjectAction::Place(id)
                            | swf::PlaceObjectAction::Replace(id) = place.action
                            && self.editable_texts.contains(&id)
                        {
                            *tag = swf::Tag::RemoveObject(swf::RemoveObject {
                                depth: place.depth,
                                character_id: None,
                            });
                        }
                    }
                    sprite.tags.retain(|tag| {
                        matches!(
                            tag,
                            swf::Tag::PlaceObject(_)
                                | swf::Tag::RemoveObject(_)
                                | swf::Tag::ShowFrame
                        )
                    });
                }
                tag
            })
            .collect())
    }
    pub fn buttons(&self) -> &[Button] {
        &self.buttons
    }
    pub fn compile(self, builder: &VatfBuilder) -> Result<Vec<Graphic>> {
        let mut container =
            AnimContainer::from_parts(&builder.animations, &Default::default(), builder.frame_rate);
        // Instance names have no playback/skin meaning in a static library.
        for (_, frames) in &mut container.animations {
            for frame in frames {
                for object in &mut frame.entries {
                    object.name = None;
                }
            }
        }
        fn check(
            id: u16,
            source: &Sources,
            builder: &VatfBuilder,
            path: &mut Vec<u16>,
        ) -> Result<()> {
            ensure!(
                id != 0 && source.allowed.contains(&id),
                "character {id} is not a Shape or Sprite (bitmap/text/morph/button/import unsupported)"
            );
            ensure!(
                path.len() < 128 && !path.contains(&id),
                "recursive/deep UI dependency {path:?} -> {id}"
            );
            ensure!(
                !source.scripted.contains(&id),
                "Sprite {id} contains ActionScript"
            );
            ensure!(
                !source.bitmap_shapes.contains(&id),
                "Shape {id} contains bitmap fill"
            );
            path.push(id);
            if let Some(frames) = builder.animations.get(&id) {
                ensure!(
                    !frames.is_empty() && (source.animated || frames.len() == 1),
                    "Sprite {id} must have exactly one frame, got {}",
                    frames.len()
                );
                for object in frames.iter().flatten() {
                    check(object.id, source, builder, path)
                        .with_context(|| format!("Sprite {id} -> character {}", object.id))?;
                }
            } else {
                let shape = builder
                    .shape_records
                    .iter()
                    .find(|s| s.id == id)
                    .context("missing shape geometry")?;
                let start = shape.sub_shape_offset as usize;
                let meshes = builder
                    .shape_meshes
                    .get(start..start + shape.sub_shape_count as usize)
                    .context("invalid shape mesh range")?;
                ensure!(
                    meshes.iter().all(|m| matches!(
                        m.material_type,
                        crate::material::COLOR | crate::material::GRADIENT
                    )),
                    "Shape {id} contains bitmap/unsupported fill"
                );
            }
            path.pop();
            Ok(())
        }
        fn bounds(nodes: &[BakedNode], builder: &VatfBuilder, rect: &mut [f32; 4]) {
            for node in nodes {
                match node {
                    BakedNode::Shape { id, transform, .. } => {
                        let shape = builder.shape_records.iter().find(|s| s.id == *id).unwrap();
                        let start = shape.sub_shape_offset as usize;
                        for mesh in
                            &builder.shape_meshes[start..start + shape.sub_shape_count as usize]
                        {
                            let m = transform.matrix;
                            for x in [
                                mesh.bounds_center_x - mesh.bounds_half_x,
                                mesh.bounds_center_x + mesh.bounds_half_x,
                            ] {
                                for y in [
                                    mesh.bounds_center_y - mesh.bounds_half_y,
                                    mesh.bounds_center_y + mesh.bounds_half_y,
                                ] {
                                    let (x, y) =
                                        (m.a * x + m.c * y + m.tx, m.b * x + m.d * y + m.ty);
                                    rect[0] = rect[0].min(x);
                                    rect[1] = rect[1].min(y);
                                    rect[2] = rect[2].max(x);
                                    rect[3] = rect[3].max(y);
                                }
                            }
                        }
                    }
                    BakedNode::Group { children, .. } => bounds(children, builder, rect),
                    BakedNode::Mask { mask, children } => {
                        bounds(mask, builder, rect);
                        bounds(children, builder, rect);
                    }
                    BakedNode::Skin { .. } => unreachable!("static compilation never emits skins"),
                }
            }
        }
        fn center(nodes: &mut [BakedNode], x: f32, y: f32) {
            for node in nodes {
                match node {
                    BakedNode::Shape { transform, .. } => {
                        transform.matrix.tx -= x;
                        transform.matrix.ty -= y;
                    }
                    BakedNode::Group { children, .. } => center(children, x, y),
                    BakedNode::Mask { mask, children } => {
                        center(mask, x, y);
                        center(children, x, y);
                    }
                    BakedNode::Skin { .. } => unreachable!(),
                }
            }
        }
        fn period(id: u16, builder: &VatfBuilder) -> Result<usize> {
            let Some(frames) = builder.animations.get(&id) else {
                return Ok(1);
            };
            if frames.len() > 1 {
                return Ok(frames.len());
            }
            let mut result = 1;
            for object in &frames[0] {
                let child = period(object.id, builder)?;
                let (mut a, mut b) = (result, child);
                while b != 0 {
                    (a, b) = (b, a % b);
                }
                result = (result / a)
                    .checked_mul(child)
                    .context("UI cycle overflow")?;
                ensure!(
                    result <= 4096,
                    "UI cycle {result} exceeds 4096 frames; split long cycles in the source"
                );
            }
            Ok(result)
        }
        fn node_count(nodes: &[BakedNode]) -> usize {
            nodes
                .iter()
                .map(|node| {
                    1 + match node {
                        BakedNode::Group { children, .. } => node_count(children),
                        BakedNode::Mask { mask, children } => {
                            node_count(mask) + node_count(children)
                        }
                        _ => 0,
                    }
                })
                .sum()
        }
        let mut total_nodes = 0usize;
        let mut graphics: Vec<Graphic> = self
            .exports
            .iter()
            .map(|(name, id)| {
                check(*id, &self, builder, &mut Vec::new())
                    .with_context(|| format!("UI export {name:?}"))?;
                let length = period(*id, builder)?;
                ensure!(length <= 4096, "UI cycle exceeds 4096 frames");
                let mut frames = Vec::with_capacity(length);
                for frame in 0..length {
                    let nodes = crate::baked::bake_ui_symbol(&container, *id, frame)?;
                    total_nodes = total_nodes
                        .checked_add(node_count(&nodes))
                        .context("UI node count overflow")?;
                    ensure!(
                        total_nodes <= 1_000_000,
                        "UI export library exceeds 1000000 baked nodes"
                    );
                    frames.push(nodes);
                }
                let mut rect = [
                    f32::INFINITY,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    f32::NEG_INFINITY,
                ];
                for nodes in &frames {
                    bounds(nodes, builder, &mut rect);
                }
                ensure!(
                    rect.iter().all(|v| v.is_finite()) && rect[2] > rect[0] && rect[3] > rect[1],
                    "UI export {name:?} has empty/invalid geometry bounds"
                );
                for nodes in &mut frames {
                    center(nodes, (rect[0] + rect[2]) * 0.5, (rect[1] + rect[3]) * 0.5);
                }
                Ok(Graphic {
                    name: name.clone(),
                    source_bounds: rect,
                    frames,
                    frame_rate: builder.frame_rate,
                })
            })
            .collect::<Result<_>>()?;
        for button in &self.buttons {
            let states = [&button.up, &button.over, &button.down];
            ensure!(
                graphics
                    .iter()
                    .filter(
                        |g| states.contains(&&g.name) || button.hit_test.as_ref() == Some(&g.name)
                    )
                    .all(|g| g.frames.len() == 1),
                "button {:?} contains an animated state; only static state subtrees are supported",
                button.name
            );
            let mut union = [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ];
            for graphic in graphics.iter().filter(|g| states.contains(&&g.name)) {
                for i in 0..2 {
                    union[i] = union[i].min(graphic.source_bounds[i]);
                    union[i + 2] = union[i + 2].max(graphic.source_bounds[i + 2]);
                }
            }
            for graphic in graphics
                .iter_mut()
                .filter(|g| states.contains(&&g.name) || button.hit_test.as_ref() == Some(&g.name))
            {
                let old = graphic.source_bounds;
                for nodes in &mut graphic.frames {
                    center(
                        nodes,
                        (union[0] + union[2] - old[0] - old[2]) * 0.5,
                        (union[1] + union[3] - old[1] - old[3]) * 0.5,
                    );
                }
                // Hit geometry retains its own bounds; its nodes use the button registration.
                if states.contains(&&graphic.name) {
                    graphic.source_bounds = union;
                }
            }
        }
        Ok(graphics)
    }
}
