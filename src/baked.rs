//! Deterministic game-animation data compiled from a labelled root timeline.
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::animation::{AnimContainer, AnimDisplayObject, AnimFilter, AnimTransform};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BakedMovie {
    /// Frame rate of the source SWF, in frames per second.
    ///
    /// A SWF has exactly one frame rate (on the movie header); sprites have no
    /// frame rate of their own and advance one frame per parent frame. Playback
    /// is driven by the host: one timeline frame per `1.0 / frame_rate` seconds.
    ///
    /// Zero on the empty-movie paths (no root timeline, or an empty root) — such
    /// a movie also has no clips, so nothing can be played from it.
    pub frame_rate: f32,
    pub clips: Vec<BakedClip>,
    pub skins: Vec<BakedSkin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakedClip {
    pub name: String,
    pub start_frame: u32,
    pub frames: Vec<Vec<BakedNode>>,
    pub events: Vec<FrameEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameEvent {
    pub frame: u32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakedSkin {
    pub symbol: u16,
    pub variants: Vec<BakedSkinVariant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakedSkinVariant {
    pub name: String,
    pub nodes: Vec<BakedNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BakedNode {
    Shape {
        id: u16,
        ratio: u16,
        transform: AnimTransform,
    },
    Group {
        children: Vec<BakedNode>,
        filters: Vec<AnimFilter>,
        blend_mode: u8,
    },
    Skin {
        slot: String,
        symbol: u16,
        transform: AnimTransform,
    },
    Mask {
        mask: Vec<BakedNode>,
        children: Vec<BakedNode>,
    },
}

fn compose(parent: AnimTransform, local: AnimTransform) -> AnimTransform {
    AnimTransform {
        matrix: parent.matrix * local.matrix,
        color_transform: parent.color_transform * local.color_transform,
    }
}

/// Ordinary sprite timelines are evaluated here, never by the runtime renderer.
pub fn bake(container: &AnimContainer, extra_events: &[(Box<str>, usize)]) -> Result<BakedMovie> {
    bake_with_skin_variants(container, extra_events, &HashMap::new())
}

/// Bake `skin_<slot>` instances from their directly named frames.
pub fn bake_with_skin_variants(
    container: &AnimContainer,
    extra_events: &[(Box<str>, usize)],
    skin_variants: &HashMap<u16, Vec<(Box<str>, usize)>>,
) -> Result<BakedMovie> {
    let mut timelines = HashMap::new();
    for (id, frames) in &container.animations {
        ensure!(
            timelines.insert(*id, frames.as_slice()).is_none(),
            "duplicate sprite {id}"
        );
    }
    let Some(root) = timelines.get(&0).copied() else {
        return Ok(BakedMovie::default());
    };
    ensure!(root.len() <= u32::MAX as usize, "root timeline too long");
    if root.is_empty() {
        return Ok(BakedMovie::default());
    }
    // Programmatic builders may omit timing; SWF conversion always supplies it.
    ensure!(
        container.frame_rate.is_finite() && container.frame_rate >= 0.0,
        "invalid frame rate"
    );
    let mut starts = Vec::new();
    let mut names = HashSet::new();
    for (label, frame) in &container.labels {
        if label.starts_with("event_") {
            continue;
        }
        let name = label.strip_prefix("anim_").unwrap_or(label);
        ensure!(
            !name.is_empty() && names.insert(name),
            "empty or duplicate animation name: {label}"
        );
        ensure!(
            *frame < root.len(),
            "animation {label} outside root timeline"
        );
        starts.push((*frame, name.to_owned()));
    }
    starts.sort();
    let has_labelled_clips = !starts.is_empty();
    if starts.is_empty() {
        starts.push((0, "default".into()));
    }
    ensure!(
        starts[0].0 == 0,
        "first animation label must start at root frame 0"
    );
    ensure!(
        starts.windows(2).all(|w| w[0].0 != w[1].0),
        "multiple animations start at the same frame"
    );
    let mut events: Vec<_> = container
        .labels
        .iter()
        .chain(extra_events)
        .filter_map(|(name, frame)| {
            name.strip_prefix("event_")
                .map(|name| (*frame, name.to_owned()))
        })
        .collect();
    // Stable sorting preserves source order for multiple events on one frame.
    events.sort_by_key(|event| event.0);
    for (frame, name) in &events {
        ensure!(
            *frame < root.len() && !name.is_empty(),
            "invalid event {name} at frame {frame}"
        );
    }
    let mut compiler = Compiler {
        timelines,
        skin_variants,
        skins: BTreeMap::new(),
        visiting: Vec::new(),
        skin_visiting: HashSet::new(),
        emitted: 0,
        frozen: false,
    };
    let mut clips = Vec::new();
    for (index, (start, name)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(root.len(), |next| next.0);
        let origin = if has_labelled_clips {
            clip_root_origin(root, *start, end, name)?
        } else {
            (0.0, 0.0)
        };
        let root_parent = AnimTransform {
            matrix: crate::animation::AnimMatrix {
                tx: -origin.0,
                ty: -origin.1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut frames = Vec::new();
        for (frame, display) in root.iter().enumerate().take(end).skip(*start) {
            frames.push(compiler.list(&display.entries, frame, root_parent)?);
        }
        clips.push(BakedClip {
            name: name.clone(),
            start_frame: *start as u32,
            frames,
            events: events
                .iter()
                .filter(|(f, _)| *f >= *start && *f < end)
                .map(|(f, name)| FrameEvent {
                    frame: (*f - start) as u32,
                    name: name.clone(),
                })
                .collect(),
        });
    }
    Ok(BakedMovie {
        frame_rate: container.frame_rate,
        clips,
        skins: compiler.skins.into_values().collect(),
    })
}

/// Return the initial placement translation of an action's sole root object.
///
/// Applying its inverse once to every frame removes source-sheet placement
/// while retaining motion relative to the first populated frame. Empty clips
/// have no placement to normalize. Multiple root objects are ambiguous under
/// the labelled-action contract and must be fixed in the source asset.
fn clip_root_origin(
    root: &[crate::animation::AnimFrame],
    start: usize,
    end: usize,
    name: &str,
) -> Result<(f32, f32)> {
    let mut origin = None;
    for (relative_frame, display) in root[start..end].iter().enumerate() {
        ensure!(
            display.entries.len() <= 1,
            "animation {name} root frame {} has {} objects; expected at most one",
            relative_frame,
            display.entries.len()
        );
        if origin.is_none()
            && let Some(object) = display.entries.first()
        {
            let tx = object.transform.matrix.tx;
            let ty = object.transform.matrix.ty;
            ensure!(
                tx.is_finite() && ty.is_finite(),
                "animation {name} has a non-finite root translation"
            );
            origin = Some((tx, ty));
        }
    }
    Ok(origin.unwrap_or_default())
}

struct Compiler<'a> {
    timelines: HashMap<u16, &'a [crate::animation::AnimFrame]>,
    skin_variants: &'a HashMap<u16, Vec<(Box<str>, usize)>>,
    skins: BTreeMap<u16, BakedSkin>,
    visiting: Vec<u16>,
    skin_visiting: HashSet<u16>,
    emitted: usize,
    frozen: bool,
}

impl Compiler<'_> {
    fn list(
        &mut self,
        objects: &[AnimDisplayObject],
        frame: usize,
        parent: AnimTransform,
    ) -> Result<Vec<BakedNode>> {
        ensure!(
            objects.windows(2).all(|w| w[0].depth < w[1].depth),
            "display depths must be unique and ordered"
        );
        let mut result = Vec::new();
        let mut index = 0;
        while index < objects.len() {
            let object = &objects[index];
            let nodes = self.object(object, frame, parent)?;
            if object.clip_depth > object.depth {
                let end = objects[index + 1..]
                    .iter()
                    .position(|o| o.depth > object.clip_depth)
                    .map_or(objects.len(), |n| index + 1 + n);
                // Crossing mask intervals need an explicit stencil-stack representation.
                ensure!(
                    objects[index + 1..end]
                        .iter()
                        .all(|o| o.clip_depth <= object.clip_depth),
                    "crossing mask ranges are unsupported"
                );
                let children = self.list(&objects[index + 1..end], frame, parent)?;
                result.push(BakedNode::Mask {
                    mask: nodes,
                    children,
                });
                index = end;
            } else {
                result.extend(nodes);
                index += 1;
            }
        }
        Ok(result)
    }

    fn object(
        &mut self,
        object: &AnimDisplayObject,
        frame: usize,
        parent: AnimTransform,
    ) -> Result<Vec<BakedNode>> {
        self.emitted += 1;
        ensure!(
            self.emitted <= 10_000_000,
            "baked animation exceeds node budget"
        );
        let transform = compose(parent, object.transform);
        let skin_slot = object
            .name
            .as_deref()
            .and_then(|name| name.strip_prefix("skin_"));
        let mut nodes = if let Some(slot) = skin_slot {
            ensure!(!slot.is_empty(), "empty skin slot name");
            let frames = *self
                .timelines
                .get(&object.id)
                .ok_or_else(|| anyhow::anyhow!("skin {slot} must reference a sprite"))?;
            ensure!(!frames.is_empty(), "skin {slot} has no variants");
            if !self.skins.contains_key(&object.id) {
                ensure!(
                    self.skin_visiting.insert(object.id),
                    "recursive skin {}",
                    object.id
                );
                let frozen = self.frozen;
                self.frozen = true;
                let mut variants = Vec::new();
                let labelled_frames = self.skin_variants.get(&object.id).ok_or_else(|| {
                    anyhow::anyhow!("skin {slot} sprite {} has no frame labels", object.id)
                })?;
                ensure!(
                    !labelled_frames.is_empty(),
                    "skin {slot} has no labelled variants"
                );
                for (name, frame) in labelled_frames {
                    let variant = frames.get(*frame).ok_or_else(|| {
                        anyhow::anyhow!(
                            "skin {slot} variant frame {frame} outside sprite {}",
                            object.id
                        )
                    })?;
                    // A skin frame is a static snapshot; nested ordinary timelines sample at their placement.
                    variants.push(BakedSkinVariant {
                        name: name.to_string(),
                        nodes: self.list(&variant.entries, 0, AnimTransform::default())?,
                    });
                }
                self.skin_visiting.remove(&object.id);
                self.frozen = frozen;
                self.skins.insert(
                    object.id,
                    BakedSkin {
                        symbol: object.id,
                        variants,
                    },
                );
            }
            vec![BakedNode::Skin {
                slot: slot.into(),
                symbol: object.id,
                transform,
            }]
        } else if let Some(frames) = self.timelines.get(&object.id).copied() {
            ensure!(
                self.visiting.len() < 128 && !self.visiting.contains(&object.id),
                "recursive/deep sprite {}",
                object.id
            );
            if frames.is_empty() {
                return Ok(Vec::new());
            }
            let child = if self.frozen {
                0
            } else {
                (frame as i64 - i64::from(object.place_frame)).rem_euclid(frames.len() as i64)
                    as usize
            };
            self.visiting.push(object.id);
            let result = self.list(&frames[child].entries, child, transform)?;
            self.visiting.pop();
            result
        } else {
            vec![BakedNode::Shape {
                id: object.id,
                ratio: object.ratio,
                transform,
            }]
        };
        ensure!(
            object.blend_mode <= 14,
            "unknown blend mode {}",
            object.blend_mode
        );
        if !object.filters.is_empty() || object.blend_mode > 1 {
            nodes = vec![BakedNode::Group {
                children: nodes,
                filters: object.filters.clone(),
                blend_mode: object.blend_mode,
            }];
        }
        Ok(nodes)
    }
}

impl BakedMovie {
    pub fn validate(&self) -> Result<()> {
        // Checked here as well as during baking so that a hand-built BAKD is
        // rejected by the loader rather than producing a zero-length playback step.
        ensure!(
            self.frame_rate.is_finite() && self.frame_rate >= 0.0,
            "invalid frame rate"
        );
        let mut names = HashSet::new();
        for clip in &self.clips {
            ensure!(
                !clip.name.is_empty() && names.insert(&clip.name),
                "duplicate/empty clip name"
            );
            ensure!(!clip.frames.is_empty(), "empty clip {}", clip.name);
            ensure!(
                clip.events
                    .iter()
                    .all(|e| !e.name.is_empty() && (e.frame as usize) < clip.frames.len()),
                "event outside clip {}",
                clip.name
            );
            ensure!(
                clip.events.windows(2).all(|w| w[0].frame <= w[1].frame),
                "unsorted clip events"
            );
        }
        let mut symbols = HashSet::new();
        for skin in &self.skins {
            if skin.variants.is_empty() || !symbols.insert(skin.symbol) {
                bail!("invalid skin {}", skin.symbol);
            }
            let mut variants = HashSet::new();
            ensure!(
                skin.variants
                    .iter()
                    .all(|variant| !variant.name.is_empty() && variants.insert(&variant.name)),
                "duplicate/empty variant name in skin {}",
                skin.symbol
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::{AnimFrame, AnimMatrix};

    fn object(id: u16, depth: u16, tx: f32) -> AnimDisplayObject {
        AnimDisplayObject {
            id,
            depth,
            name: None,
            clip_depth: 0,
            blend_mode: 1,
            transform: AnimTransform {
                matrix: AnimMatrix {
                    tx,
                    ..Default::default()
                },
                ..Default::default()
            },
            filters: vec![],
            ratio: 0,
            place_frame: 0,
        }
    }
    fn frame(entries: Vec<AnimDisplayObject>) -> AnimFrame {
        AnimFrame { entries }
    }

    #[test]
    fn clips_events_and_ordinary_children_are_baked() {
        let container = AnimContainer {
            frame_rate: 24.0,
            labels: vec![("anim_idle".into(), 0), ("anim_attack".into(), 2)],
            animations: vec![
                (0, vec![frame(vec![object(2, 1, 10.0)]); 4]),
                (
                    2,
                    vec![
                        frame(vec![object(1, 1, 1.0)]),
                        frame(vec![object(1, 1, 2.0)]),
                    ],
                ),
            ],
        };
        let result = bake(
            &container,
            &[("event_hit".into(), 2), ("event_hit".into(), 3)],
        )
        .unwrap();
        assert_eq!(result.clips.len(), 2);
        assert_eq!(
            result.clips[1].events,
            vec![
                FrameEvent {
                    frame: 0,
                    name: "hit".into()
                },
                FrameEvent {
                    frame: 1,
                    name: "hit".into()
                }
            ]
        );
        match &result.clips[1].frames[1][0] {
            BakedNode::Shape { id, transform, .. } => {
                assert_eq!(*id, 1);
                assert_eq!(transform.matrix.tx, 2.0);
            }
            _ => panic!("ordinary sprite must be expanded"),
        }
    }

    /// A sub-sprite must show its own frame 0 on the parent frame where it was
    /// placed, advance one frame per parent frame afterwards, and wrap when the
    /// parent runs past the child's length.
    #[test]
    fn sub_sprite_frames_loop_in_phase_with_placement() {
        // Child sprite 5: 3 frames, drawing shape 1 at a different offset each.
        let child: Vec<_> = (0..3)
            .map(|i| frame(vec![object(1, 1, i as f32)]))
            .collect();

        // Root: nothing on frames 0-1, then sprite 5 placed at frame 2 and left on stage.
        let mut root = vec![frame(vec![]), frame(vec![])];
        for _ in 2..8 {
            let mut placed = object(5, 1, 0.0);
            placed.place_frame = 2;
            root.push(frame(vec![placed]));
        }

        let container = AnimContainer {
            frame_rate: 24.0,
            labels: vec![],
            animations: vec![(0, root), (5, child)],
        };
        let result = bake(&container, &[]).unwrap();
        let clip = &result.clips[0];

        let tx = |frame: usize| match clip.frames[frame].as_slice() {
            [] => None,
            [BakedNode::Shape { transform, .. }] => Some(transform.matrix.tx),
            other => panic!("unexpected nodes at frame {frame}: {other:?}"),
        };

        assert_eq!(tx(0), None, "sprite is not placed yet");
        assert_eq!(tx(1), None, "sprite is not placed yet");
        assert_eq!(tx(2), Some(0.0), "placement frame shows child frame 0");
        assert_eq!(tx(3), Some(1.0), "child advances one frame");
        assert_eq!(tx(4), Some(2.0), "child advances one frame");
        assert_eq!(tx(5), Some(0.0), "child loops back to frame 0");
        assert_eq!(tx(6), Some(1.0), "loop continues");
        assert_eq!(tx(7), Some(2.0), "loop continues");
    }

    #[test]
    fn skin_variants_are_shared_and_not_multiplied_into_frames() {
        let mut hand = object(2, 1, 10.0);
        hand.name = Some("skin_hand".into());
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![],
            animations: vec![
                (0, vec![frame(vec![hand]); 3]),
                (
                    2,
                    vec![
                        frame(vec![object(1, 1, 1.0)]),
                        frame(vec![object(3, 1, 2.0)]),
                    ],
                ),
            ],
        };
        let variants = HashMap::from([(
            2,
            vec![(Box::from("default"), 0), (Box::from("red_armor"), 1)],
        )]);
        let result = bake_with_skin_variants(&container, &[], &variants).unwrap();
        assert_eq!(result.skins.len(), 1);
        assert_eq!(result.skins[0].variants.len(), 2);
        assert_eq!(result.skins[0].variants[0].name, "default");
        assert_eq!(result.skins[0].variants[1].name, "red_armor");
        assert_eq!(result.clips[0].frames.len(), 3);
        assert!(
            matches!(&result.clips[0].frames[2][0], BakedNode::Skin { slot, .. } if slot == "hand")
        );
    }

    #[test]
    fn frame_labels_do_not_turn_an_unmarked_instance_into_a_skin() {
        let mut hand = object(2, 1, 10.0);
        hand.name = Some("hand".into());
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![],
            animations: vec![
                (0, vec![frame(vec![hand])]),
                (
                    2,
                    vec![
                        frame(vec![object(1, 1, 1.0)]),
                        frame(vec![object(3, 1, 2.0)]),
                        frame(vec![object(4, 1, 3.0)]),
                    ],
                ),
            ],
        };
        let variants =
            HashMap::from([(2, vec![(Box::from("default"), 0), (Box::from("gold"), 2)])]);
        let result = bake_with_skin_variants(&container, &[], &variants).unwrap();

        assert!(result.skins.is_empty());
        assert!(matches!(
            &result.clips[0].frames[0][0],
            BakedNode::Shape { id: 1, .. }
        ));
    }

    #[test]
    fn cycles_and_ambiguous_animation_starts_are_errors() {
        let mut container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![],
            animations: vec![(0, vec![frame(vec![object(0, 1, 0.0)])])],
        };
        assert!(bake(&container, &[]).is_err());
        container.animations[0].1[0].entries.clear();
        container.labels = vec![("anim_a".into(), 0), ("anim_b".into(), 0)];
        assert!(bake(&container, &[]).is_err());
    }

    #[test]
    fn every_non_event_root_label_defines_an_animation() {
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![
                ("IDLE".into(), 0),
                ("event_ready".into(), 1),
                ("anim_attack".into(), 2),
            ],
            animations: vec![(0, vec![frame(vec![]); 4])],
        };

        let result = bake(&container, &[]).unwrap();
        assert_eq!(
            result
                .clips
                .iter()
                .map(|clip| clip.name.as_str())
                .collect::<Vec<_>>(),
            ["IDLE", "attack"]
        );
        assert_eq!(
            result.clips[0].events,
            [FrameEvent {
                frame: 1,
                name: "ready".into()
            }]
        );
    }

    #[test]
    fn each_animation_removes_only_its_initial_root_translation() {
        let mut first_a = object(1, 1, 100.0);
        first_a.transform.matrix.ty = 40.0;
        let mut second_a = object(1, 1, 112.0);
        second_a.transform.matrix.ty = 35.0;
        let mut first_b = object(2, 1, 500.0);
        first_b.transform.matrix.ty = -30.0;
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![("A".into(), 0), ("B".into(), 2)],
            animations: vec![(
                0,
                vec![
                    frame(vec![first_a]),
                    frame(vec![second_a]),
                    frame(vec![first_b]),
                ],
            )],
        };

        let result = bake(&container, &[]).unwrap();
        let translation = |clip: usize, frame: usize| match &result.clips[clip].frames[frame][0] {
            BakedNode::Shape { transform, .. } => (transform.matrix.tx, transform.matrix.ty),
            other => panic!("unexpected node: {other:?}"),
        };
        assert_eq!(translation(0, 0), (0.0, 0.0));
        assert_eq!(translation(0, 1), (12.0, -5.0));
        assert_eq!(translation(1, 0), (0.0, 0.0));
    }

    #[test]
    fn multiple_root_objects_in_an_animation_are_rejected() {
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![("IDLE".into(), 0)],
            animations: vec![(0, vec![frame(vec![object(1, 1, 0.0), object(2, 2, 0.0)])])],
        };

        let error = bake(&container, &[]).unwrap_err().to_string();
        assert!(error.contains("IDLE root frame 0 has 2 objects"), "{error}");
    }

    #[test]
    fn an_unlabelled_root_remains_a_general_scene() {
        let container = AnimContainer {
            frame_rate: 30.0,
            labels: vec![],
            animations: vec![(0, vec![frame(vec![object(1, 1, 10.0), object(2, 2, 20.0)])])],
        };

        let result = bake(&container, &[]).unwrap();
        assert_eq!(result.clips[0].name, "default");
        assert_eq!(result.clips[0].frames[0].len(), 2);
        match &result.clips[0].frames[0][0] {
            BakedNode::Shape { transform, .. } => assert_eq!(transform.matrix.tx, 10.0),
            other => panic!("unexpected node: {other:?}"),
        }
    }
}
