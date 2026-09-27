# 04 · 时间轴解析与 BAKD 烘焙

本篇讲时间轴数据的两代表示，以及"未展开的逐帧显示列表"如何变成"已展开的绘制树"。

涉及文件：`src/animation.rs`、`src/baked.rs`、`src/matrix.rs`、`src/transform.rs`、`tests/swf_oracle.rs`。

---

## 1. 两代数据模型

```
解析期（swf 类型）                    线格式（纯基元）
─────────────────────                ─────────────────────
lib.rs: DisplayObject          ──►   animation.rs: AnimDisplayObject
  id: CharacterId                      id: u16
  name: Option<Box<str>>               name: Option<String>
  depth / clip_depth: Depth            depth / clip_depth: u16
  blend_mode: swf::BlendMode           blend_mode: u8
  transform: Transform                 transform: AnimTransform
  filters: Box<[Filter]>               filters: Vec<AnimFilter>
  ratio: u16                           ratio: u16
  place_frame: u32                     place_frame: u32
```

`animation.rs:8-10` 的模块注释说明了这次"降级"的动机：

> *"Wire types — decomposed to primitives only, no swf crate type dependencies"*

两个好处：

1. **跨版本稳定**。`.vab` 的字节布局不随 `swf` crate 升级而改变。
2. **bincode 自描述**。全部是 `u8` / `u16` / `u32` / `f32` / `String` / `Vec`，没有 enum-with-payload、没有 `Option<Box<…>>` 嵌套，序列化形式简单且可预测。

### `AnimContainer` —— 交接到烘焙器的中间类型

```rust
// animation.rs:12-20
pub struct AnimContainer {
    pub animations: Vec<(u16, Vec<AnimFrame>)>,   // 按 sprite id 升序
    pub labels: Vec<(Box<str>, usize)>,            // 按 (frame, name) 升序
    /// Frame rate of the source SWF, in frames per second.
    ///
    /// Carried through to `BakedMovie::frame_rate`, where the runtime reads it.
    pub frame_rate: f32,
}
```

> **这个类型不是线格式**——早期版本会用 `ANIM` chunk 把它写进 `.vab`，那个 chunk 已删除（[01 篇 §2](01-format.md#2-chunk-布局)）。它现在的作用是**从 `parse_tags` 交接到 `bake_with_skin_variants` 的中间表示**，`from_parts` 是唯一的构造入口。

用 `Vec<(k, v)>` 而不是 `HashMap` 是因为下游的烘焙需要**确定性顺序**——`HashMap` 的迭代顺序不确定，会让同样输入产生不同的 `BakedMovie` 字节。排序在 `from_parts` 里强制（`animation.rs`）：

```rust
anim_entries.sort_by_key(|entry| entry.0);                                 // sprite id 升序
label_list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));      // (frame, name) 升序
```

### `place_frame` —— 全场最重要的一个字段

```rust
// animation.rs:38-44
/// Index (0-based) of the *parent* timeline frame at which this instance
/// was placed.
///
/// Sub-sprite timelines are driven relative to this: a sprite placed on
/// parent frame `place_frame` shows its own frame
/// `(parent_frame - place_frame) % child_frame_count`.
pub place_frame: u32,
```

这个公式是**运行时（或烘焙器）展开嵌套 sprite 的唯一依据**。详见 §2。

### `AnimTransform` 与两个乘法

```rust
pub struct AnimTransform { pub matrix: AnimMatrix, pub color_transform: AnimColorTransform }
```

矩阵（`animation.rs:80-93`）：

```rust
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
```

沿用 SWF 的列向量约定 `x' = a·x + c·y + tx`。**`self * rhs` 表示"先应用 rhs，再应用 self"**（因为 `tx` 那一行把 rhs 的平移先过了一遍 self 的线性部分）。所以在 `compose(parent, local) = parent * local` 中，**local 先作用**——这正是层级变换该有的语义。

颜色变换（`animation.rs:108-123`）：

```rust
r_multiply: self.r_multiply * rhs.r_multiply,
r_add:      self.r_add + self.r_multiply * rhs.r_add,
```

即 `(self ∘ rhs)(c) = self.multiply · (rhs.multiply · c + rhs.add) + self.add`——同样是"rhs 先作用"。

**`tx` / `ty` 的单位是像素，不是 twips**，在 `From<&Transform>`（`animation.rs:395-418`）里就换算好了：

```rust
tx: t.matrix.tx.to_pixels() as f32,
// 颜色变换的 add 归一化到 [0,1]
r_add: f32::from(rgba.r_add) / 255.0,
```

所以线格式里**没有 twips 这个概念**，所有位置量都是像素或无量纲比例。

---

## 2. 子时间轴播放公式

```rust
// baked.rs:309-314
let child = if self.frozen {
    0
} else {
    (frame as i64 - i64::from(object.place_frame)).rem_euclid(frames.len() as i64) as usize
};
```

含义：**子 sprite 的当前帧 = (父时间轴当前帧 − 放置帧) mod 子时间轴长度**。

- `frame` 是**调用方时间轴的帧号**。在根层它就是根帧号；递归下去之后它是那个 sprite 自己解析出来的帧号（注意 `baked.rs:316` 传的是 `child` 而不是根帧号）。
- `place_frame` 是**父时间轴**帧号，所以两者可以相减。
- **用 `rem_euclid` 而不是 `%`**：当父帧号小于 `place_frame` 时（比如父时间轴循环回到开头，而子 sprite 是在中间某帧才放上去的），差值是负数。`%` 在 Rust 里对负数返回负值，会导致索引 panic；`rem_euclid` 保证结果落在 `[0, len)` 并**正确地回绕到子时间轴的尾部帧**。
- `frozen` 时固定取第 0 帧（见 §4 skin）。

一个例子：子 sprite 有 3 帧、在第 2 帧被放置。

| 父帧 | `child` |
|---|---|
| 0 | `(0-2).rem_euclid(3)` = 1 |
| 1 | `(1-2).rem_euclid(3)` = 2 |
| 2 | 0 |
| 3 | 1 |
| 4 | 2 |

父帧 0 时子 sprite 显示的是第 1 帧——因为在父时间轴"倒回"之前它已经跑了一帧。这个细节和 Flash 的行为一致。

---

## 3. BAKD 烘焙

入口：`bake_with_skin_variants`（`baked.rs:77-179`）。`bake`（`baked.rs:72-74`）只是不传 skin 变体的包装。

### 3.1 clip 发现

BAKD 不存"所有 sprite 的所有帧"，只存**少量命名 clip**。clip 的边界由根时间轴上的 **`anim_` 前缀标签**决定：

```rust
// baked.rs:101-127
for (label, frame) in &container.labels {
    if let Some(name) = label.strip_prefix("anim_") {
        ensure!(!name.is_empty() && names.insert(name), "empty or duplicate animation name: {label}");
        ensure!(*frame < root.len(), "animation {label} outside root timeline");
        starts.push((*frame, name.to_owned()));
    }
}
starts.sort();
if starts.is_empty() { starts.push((0, "default".into())); }
ensure!(starts[0].0 == 0, "first anim_ label must start at root frame 0");
ensure!(starts.windows(2).all(|w| w[0].0 != w[1].0), "multiple animations start at the same frame");
```

规则：

| 规则 | 说明 |
|---|---|
| 只有 `anim_` 前缀的标签成为 clip | 其它标签对 BAKD 不可见 |
| **第一个 clip 必须从根帧 0 开始** | 否则报错——不允许根时间轴开头有"没有归属"的帧 |
| **不允许两个 clip 从同一帧开始** | 否则区间歧义 |
| 无 `anim_` 标签时 | 合成一个名为 **`"default"`** 的 clip，覆盖整条根时间轴 |
| `anim_` 前缀在烘焙时被**剥离** | 落盘的 `BakedClip::name` 是 `"idle"` 而不是 `"anim_idle"` |

clip 的区间是 `[start_i, start_{i+1})`，最后一个到 `root.len()`。所以 **clip 恰好铺满根时间轴，无缝无重叠**。

clip 命名冲突在解析期就已经拦过一次（[02 篇 §4](02-pipeline.md#4-framelabel-的三类分流)），这里是第二道防线。

### 3.2 事件重定基

```rust
// baked.rs:128-144
let mut events: Vec<_> = container.labels.iter().chain(extra_events)
    .filter_map(|(name, frame)| name.strip_prefix("event_").map(|name| (*frame, name.to_owned())))
    .collect();
// Stable sorting preserves source order for multiple events on one frame.
events.sort_by_key(|event| event.0);
```

`container.labels` 是 `frame_labels`（不含 `event_`），`extra_events` 是 `event_labels`（[02 篇 §4](02-pipeline.md#4-framelabel-的三类分流)）。`.chain()` 把两者合并再统一过滤——所以即使调用方把 `event_` 标签放错了容器也能工作。

**稳定排序**是刻意的：同一帧上的多个事件必须保持源顺序（`sort_by_key` 是稳定排序）。`event_` 前缀同样在落盘前被剥离。

然后是**重定基**（`baked.rs:164-172`）：

```rust
events: events.iter()
    .filter(|(f, _)| *f >= *start && *f < end)
    .map(|(f, name)| FrameEvent { frame: (*f - start) as u32, name: name.clone() })
    .collect(),
```

事件的帧号从**绝对根帧**变成**clip 相对帧**。于是运行时"派发第 n 帧的事件"就是 `clip.events` 里 `frame == playhead` 的项——和 `clip.frames[playhead]` 用同一套索引。

### 3.3 变换在烘焙期就乘完

```rust
// baked.rs:158-160
for (frame, display) in root.iter().enumerate().take(end).skip(*start) {
    frames.push(compiler.list(&display.entries, frame, AnimTransform::default())?);
}
```

注意 `frame` 传的是**绝对根帧号**（不是 `frame - start`），父变换从 `AnimTransform::default()`（单位阵）开始。

`compose` 在 `Compiler::object` 里逐层累积（`baked.rs:244`）：

```rust
let transform = compose(parent, object.transform);
```

所以**每个 `BakedNode` 携带的都是世界变换**——父链已经乘完了。这是 BAKD 的核心卖点：运行时不需要变换栈。

### 3.4 定时信息在 `BakedMovie::frame_rate`

```rust
// baked.rs:9-20
pub struct BakedMovie {
    pub frame_rate: f32,
    pub clips: Vec<BakedClip>,
    pub skins: Vec<BakedSkin>,
}
```

**全文件只有一个帧率**，因为 SWF 就是这样：帧率在 movie header 上，`DefineSprite` 没有自己的帧率字段，子 sprite 每父帧推进一帧。所以这是一个 f32 的量，直接放在 `BakedMovie` 上，而不值得为它单独保留一个 chunk。

`frame_rate` 由烘焙器从 `AnimContainer` 带过来；两个空 movie 的提前返回路径（无根时间轴 / 根为空）会拿 `Default` 的 `0.0`，那时也没有 clip 可播。`validate()` 会检查它是有限且非负的——这条检查同时覆盖手工构造的文件。

播放步长由宿主导：每 `1.0 / frame_rate` 秒推进一个时间轴帧（[06 篇 §8](06-runtime.md#8-播放与定时)）。

> **历史注记**：早期版本 `BakedMovie` 不含帧率，唯一来源是 `ANIM` chunk 的 `AnimContainer.frame_rate`。这导致"运行时要为 4 个字节读一个占 17%–54% 体积的 chunk"，已修复。

### 3.5 `BakedNode` 的四种节点

```rust
// baked.rs:41-62
pub enum BakedNode {
    Shape { id: u16, ratio: u16, transform: AnimTransform },
    Group { children: Vec<BakedNode>, filters: Vec<AnimFilter>, blend_mode: u8 },
    Skin  { slot: String, symbol: u16, transform: AnimTransform },
    Mask  { mask: Vec<BakedNode>, children: Vec<BakedNode> },
}
```

| 节点 | 变换 | 空间 |
|---|---|---|
| `Shape` | 有 | **世界空间** |
| `Group` | **无** | —— 子节点已经带世界变换，所以它不需要 |
| `Skin` | 有 | **世界空间**（但变体内部的节点是 **skin 局部空间**，见 §4） |
| `Mask` | **无** | `mask` 与 `children` 都在世界空间 |

`Group` 和 `Mask` 不带变换不是偷懒，而是因为它们的子节点已经各自带好了世界变换——再乘一次就重复了。

---

## 4. `Compiler` —— 递归展开

```rust
// baked.rs:181-189
struct Compiler<'a> {
    timelines: HashMap<u16, &'a [AnimFrame]>,
    skin_variants: &'a HashMap<u16, Vec<(Box<str>, usize)>>,
    skins: BTreeMap<u16, BakedSkin>,      // BTreeMap → 输出顺序确定
    visiting: Vec<u16>,                   // 普通 sprite 的递归守卫 + 深度计数
    skin_visiting: HashSet<u16>,          // skin 的递归守卫
    emitted: usize,                       // 全局节点预算
    frozen: bool,                         // 在 skin 变体内 → 静态快照
}
```

两个独立的递归守卫是必需的，因为 skin 和普通 sprite 走的是**不同的代码路径**，用同一个栈无法同时表达"正在展开这个 skin 的变体"和"正在展开这个 sprite 的帧"。

### 4.1 `list` —— Mask 扫描

```rust
// baked.rs:192-231
fn list(&mut self, objects: &[AnimDisplayObject], frame: usize, parent: AnimTransform)
    -> Result<Vec<BakedNode>> {
    ensure!(objects.windows(2).all(|w| w[0].depth < w[1].depth),
            "display depths must be unique and ordered");
    let mut result = Vec::new();
    let mut index = 0;
    while index < objects.len() {
        let object = &objects[index];
        let nodes = self.object(object, frame, parent)?;
        if object.clip_depth > object.depth {
            let end = objects[index + 1..].iter()
                .position(|o| o.depth > object.clip_depth)
                .map_or(objects.len(), |n| index + 1 + n);
            // Crossing mask intervals need an explicit stencil-stack representation.
            ensure!(objects[index + 1..end].iter().all(|o| o.clip_depth <= object.clip_depth),
                    "crossing mask ranges are unsupported");
            let children = self.list(&objects[index + 1..end], frame, parent)?;
            result.push(BakedNode::Mask { mask: nodes, children });
            index = end;
        } else {
            result.extend(nodes);
            index += 1;
        }
    }
    Ok(result)
}
```

**遮罩语义**：`clip_depth > depth` 的对象是遮罩。它吞掉后面**所有** `depth <= clip_depth` 的兄弟作为 `children`，自己变成 `mask`。

这里的扫描逻辑**依赖"深度严格递增"**——它靠 `position(|o| o.depth > object.clip_depth)` 找区间右端。这条不变量在上游是免费得到的：`parse_tags` 用 `BTreeMap<Depth, DisplayObject>` 累积，迭代顺序天然就是深度升序（[02 篇 §3](02-pipeline.md#3-显示列表模型--parse_tags)）。`ensure!` 只是把它变成显式契约。

**交叉/嵌套的遮罩区间直接报错**。源码注释给了理由：

> *"Crossing mask intervals need an explicit stencil-stack representation."*

也就是说，格式选择了"扁平的一层遮罩"这种**无需运行时 stencil 栈**的表示，代价是不支持嵌套遮罩区间。注意 `children` 是**递归**地走 `list` 得到的，所以 `children` 内部**可以**再有 `Mask` 节点——被禁止的只是"区间互相交叠"，不是"嵌套遮罩"。

### 4.2 `object` —— 三条分支

```rust
// baked.rs:233-339
self.emitted += 1;
ensure!(self.emitted <= 10_000_000, "baked animation exceeds node budget");
let transform = compose(parent, object.transform);
let skin_slot = object.name.as_deref().and_then(|name| name.strip_prefix("skin_"));
```

**全局 1000 万节点预算**：防止病态 SWF（大量深层嵌套 sprite）把展开过程变成内存炸弹。注意这是**跨越所有 clip 和所有 skin 的全局计数**。

#### 分支 A：skin 实例（`baked.rs:249-299`）

```rust
let mut nodes = if let Some(slot) = skin_slot {
    ensure!(!slot.is_empty(), "empty skin slot name");
    let frames = *self.timelines.get(&object.id)
        .ok_or_else(|| anyhow!("skin {slot} must reference a sprite"))?;
    ensure!(!frames.is_empty(), "skin {slot} has no variants");
    if !self.skins.contains_key(&object.id) {          // ★ 全局只烘焙一次
        ensure!(self.skin_visiting.insert(object.id), "recursive skin {}", object.id);
        let frozen = self.frozen;
        self.frozen = true;                             // ★ 变体内部冻结
        let mut variants = Vec::new();
        let labelled_frames = self.skin_variants.get(&object.id)
            .ok_or_else(|| anyhow!("skin {slot} sprite {} has no frame labels", object.id))?;
        ensure!(!labelled_frames.is_empty(), "skin {slot} has no labelled variants");
        for (name, frame) in labelled_frames {
            let variant = frames.get(*frame).ok_or_else(|| anyhow!(
                "skin {slot} variant frame {frame} outside sprite {}", object.id))?;
            // A skin frame is a static snapshot; nested ordinary timelines sample at their placement.
            variants.push(BakedSkinVariant {
                name: name.to_string(),
                nodes: self.list(&variant.entries, 0, AnimTransform::default())?,
            });
        }
        self.skin_visiting.remove(&object.id);
        self.frozen = frozen;
        self.skins.insert(object.id, BakedSkin { symbol: object.id, variants });
    }
    vec![BakedNode::Skin { slot: slot.into(), symbol: object.id, transform }]
} else if …
```

关于 skin 的几个要点：

**① 识别条件是实例名，不是帧标签。** `object.name` 有 `skin_` 前缀才算。`baked.rs:487` 的测试 `frame_labels_do_not_turn_an_unmarked_instance_into_a_skin` 固定了这一点：同一个 sprite 有命名帧，但实例名只是 `"hand"` → **不产生任何 skin**，节点退化成一个普通 `Shape`。

**② skin 的身份是 sprite 角色 id，不是槽位名。** 变体烘焙一次后按 `object.id` 缓存（`self.skins` 是 `BTreeMap<u16, BakedSkin>`）。同一个 sprite 被两个不同槽位（`skin_hand` 和 `skin_foot`）放置时**共享同一份变体**；`slot` 字符串只是运行时用来选变体的**选择器名字**，不参与烘焙。

**③ `frozen` 让变体成为静态快照。** `self.frozen = true` 在整个变体展开期间生效，其唯一作用是让嵌套的普通 sprite 分支固定取第 0 帧（`baked.rs:309-313`）。这就是"一份变体树服务所有帧"的前提——否则每个变体都得对每个播放帧展开一份，变成笛卡尔积。源码注释：*"A skin frame is a static snapshot; nested ordinary timelines sample at their placement."*

**④ 变体在自己的局部空间。** `self.list(&variant.entries, 0, AnimTransform::default())` —— 父变换是单位阵、帧号是 0。所以变体内的节点是 **skin 局部空间**，运行时需要 `Skin.transform * variant_node.transform`（[06 篇](06-runtime.md#5-变换下发规则)）。

**⑤ `skins` 用 `BTreeMap`** 是为了输出顺序按 symbol 确定，保证同样输入产生同样字节。

#### 分支 B：普通 sprite（`baked.rs:300-318`）

```rust
} else if let Some(frames) = self.timelines.get(&object.id).copied() {
    ensure!(self.visiting.len() < 128 && !self.visiting.contains(&object.id),
            "recursive/deep sprite {}", object.id);
    if frames.is_empty() { return Ok(Vec::new()); }      // ← 注意：直接 return
    let child = if self.frozen { 0 } else {
        (frame as i64 - i64::from(object.place_frame)).rem_euclid(frames.len() as i64) as usize
    };
    self.visiting.push(object.id);
    let result = self.list(&frames[child].entries, child, transform)?;
    self.visiting.pop();
    result
}
```

- 一条 `ensure!` 同时承担**深度上限 128** 和**环检测**：`visiting.len() < 128` 是硬上限，`!contains` 是环检测。
- 空 sprite 提前 `return`，注意这会**跳过下面的 Group 包装**——空 sprite 的滤镜/混合模式直接丢失。逻辑上说得通（没内容，包装没意义），但值得知道。
- **每层只展开一帧**：`frames[child].entries`。整个子树对应的是"父时间轴这一帧"这一个瞬间的状态。

#### 分支 C：叶子形状（`baked.rs:319-325`）

```rust
} else {
    vec![BakedNode::Shape { id: object.id, ratio: object.ratio, transform }]
};
```

**任何不在 `timelines` 里的 id 都变成 `Shape`**。这包括普通形状、也包括 morph 形状——**编译器从不查 MORP 表**，`ratio` 只是原样带过去，让运行时自己决定查 MORP 还是 SHAP（[01 篇 §6](01-format.md#6-morp-表与-morph-查表)）。

#### Group 包装（`baked.rs:326-337`）

```rust
ensure!(object.blend_mode <= 14, "unknown blend mode {}", object.blend_mode);
if !object.filters.is_empty() || object.blend_mode > 1 {
    nodes = vec![BakedNode::Group {
        children: nodes, filters: object.filters.clone(), blend_mode: object.blend_mode,
    }];
}
```

| 条件 | 是否包装 |
|---|---|
| `filters` 非空 | 是 |
| `blend_mode > 1` | 是 |
| `blend_mode <= 1` | 否 |

> **`> 1` 这个边界看着别扭，原因是**：`swf::BlendMode` 的判别值是 `Normal = 0`、`Layer = 2`、`Multiply = 3` … `HardLight = 14`——**没有判别值 1**。所以条件等价于"Normal 不包装，Layer 及其它所有真实混合模式都包装"。`<= 14` 则拒绝未知/保留值。
>
> 结果是**判别值 1 会被当作 Normal 放过**（测试里就用 `blend_mode: 1` 来避免节点被包装，见 `baked.rs:394`）。这不是有效值，但格式对它是宽容的。

包装发生在三条分支**之后**，所以 `Skin` 节点同样可能被包进 `Group`。

### 4.3 `validate()`

```rust
// baked.rs:342-379
pub fn validate(&self) -> Result<()> {
    let mut names = HashSet::new();
    for clip in &self.clips {
        ensure!(!clip.name.is_empty() && names.insert(&clip.name), "duplicate/empty clip name");
        ensure!(!clip.frames.is_empty(), "empty clip {}", clip.name);
        ensure!(clip.events.iter().all(|e| !e.name.is_empty() && (e.frame as usize) < clip.frames.len()),
                "event outside clip {}", clip.name);
        ensure!(clip.events.windows(2).all(|w| w[0].frame <= w[1].frame), "unsorted clip events");
    }
    let mut symbols = HashSet::new();
    for skin in &self.skins {
        if skin.variants.is_empty() || !symbols.insert(skin.symbol) { bail!("invalid skin {}", skin.symbol); }
        let mut variants = HashSet::new();
        ensure!(skin.variants.iter().all(|v| !v.name.is_empty() && variants.insert(&v.name)),
                "duplicate/empty variant name in skin {}", skin.symbol);
    }
    Ok(())
}
```

**检查了**：clip 名唯一非空；每个 clip 至少一帧；事件名非空、帧号在 `clip.frames` 范围内、按帧号非降序（同帧多个允许）；skin 至少一个变体、symbol 唯一；变体名唯一非空。

**没检查**（这些是运行时要自己兜住的假设）：

| 没检查的 | 后果 |
|---|---|
| `Shape.id` 是否真的在 SHAP / MORP 里 | 运行时会查不到网格 |
| `Skin.symbol` 是否有对应的 `skins` 项 | 运行时会找不到变体 |
| `Group.blend_mode <= 14` | 编译期查过，手工构造的文件没查 |
| 遮罩区间的一致性 | 编译期查过 |
| `BakedClip.start_frame` 是否与根时间轴长度自洽 | 帧号可能越界 |
| clip 之间是否重叠/无缝 | 编译期保证 |

（`frame_rate` 曾经是这张表里的一员——它是 `ANIM` 的字段，不在 `BakedMovie` 上。现在它移入 `BakedMovie` 并被 `validate()` 覆盖：）

```rust
ensure!(self.frame_rate.is_finite() && self.frame_rate >= 0.0, "invalid frame rate");
```

---

## 5. 差分测试：oracle

`tests/swf_oracle.rs` 是这份代码里最值得学习的测试设计之一。

### 它解决什么问题

BAKD 是"展开后"的产物，很难直接验证——拿它做基准，就得在测试里重写一遍展开逻辑，那等于用同一份逻辑验证自己。所以 oracle 需要一个"**未展开的、per-sprite、局部变换、带 `place_frame`**"的表示，而这种表示正好是 `AnimContainer` 的形状。

于是做法是：

> 用**另写一遍**的实现，从原始 SWF 标签推出每帧的显示列表，再和 `convert_swf_to_vab` 产出的容器逐项比对。

### 容器从哪来

`ANIM` chunk 删除后，容器不再能从文件里读。测试改用 `vatf::parse_animation_container`（`src/lib.rs`）——它复用同一套解析流程，但**在进程内返回容器、不写文件**：

```rust
pub fn parse_animation_container(input: &Path) -> Result<animation::AnimContainer>
```

这样测试能力完全保留，而格式不必为测试背一个 chunk。

> ⚠️ **一个依赖关系**：这条路径让 oracle 测试的存续依赖 `AnimContainer` 这个类型继续存在。如果将来把它也删了，差分测试就失去基准。测试文件顶部有注释说明这一点。

模块注释（`swf_oracle.rs:1-20`）明确说出了它的意图：

> *"The oracle is written from SWF semantics (an object stays on stage until an explicit `RemoveObject`), **not** by copying `parse_tags`. That is what makes it able to catch a compiler that emits per-frame deltas instead of the full display list."*

也就是说：**这段代码刻意不和被测代码共享实现**，否则它只能验证"代码和自己一致"。它要抓的正是"某天有人把显示列表改成每帧发差量"这类回归。

### 独立实现

```rust
// swf_oracle.rs:118-160
let mut stage: BTreeMap<Depth, ExpectedObject> = BTreeMap::new();   // 跨帧持久
let mut timeline: Vec<Vec<ExpectedObject>> = Vec::new();
for tag in tags {
    match tag {
        Tag::DefineSprite(sprite) => walk_tags(sprite.tags, sprite.id, timelines),
        Tag::PlaceObject(place) => match place.action {
            Place(id)   => { … o.place_frame = timeline.len() as u32; stage.insert(place.depth, o); }
            Modify      => { … o.apply_place_object(&place) }
            Replace(id) => { … o.id = id; … o.place_frame = timeline.len() as u32; }
        },
        Tag::RemoveObject(remove) => { stage.remove(&remove.depth); }
        Tag::ShowFrame => timeline.push(stage.values().cloned().collect()),
        _ => {}
    }
}
```

结构上和 `parse_tags` 相似，但它是从 SWF 语义出发写的，而且**把对象投影成可比较的扁平值**（`ExpectedObject` 带 `[f32;6]` 矩阵和 `[f32;8]` 颜色变换），逐项断言：

```rust
// swf_oracle.rs:222-305 的比对阶梯
1. container.animations.len() == expected.len()          // sprite 数量
2. 按 id 找到对应 sprite
3. baked_frames.len() == expected_frames.len()           // 帧数
4. 每帧 entries.len() 相等（不等时打印两边的 id 列表）
5. 每项：id / depth / clip_depth / blend_mode / ratio / place_frame 精确相等
   + 矩阵 6 个分量、颜色变换 8 个分量，容差 1e-4
```

身份类字段**精确相等**、浮点**容差 1e-4**（`FLOAT_TOLERANCE`，`swf_oracle.rs:197-204`）——这个容差正好覆盖 twips→像素的换算误差。

### 三个测试

| 测试 | 内容 |
|---|---|
| `display_list_persists_objects_across_frames` | **在内存里合成一个 SWF**（不需要素材文件），4 帧：第 0 帧 `Place`，1–3 帧用 `Modify` 改矩阵。断言每一帧都**仍有**这个对象且 `tx == index * 10.0`。这正是"差量 bug"的回归守卫 |
| `oracle_matches_baked_sample` | 用真实素材 `../bevy_flash/assets/spirit2159src.swf`，先把 `root timeline 长度 == swf.header.num_frames()` 作为强结构断言，再跑完整比对 |
| `filter_dest_rect_matches_swf_crate` | 拿真正的 `swf::BlurFilter` / `DropShadowFilter` / `BevelFilter` 调上游 `calculate_dest_rect`，和本仓库的 `filter_dest_rect` 对比——**对移植代码的差分验证** |

第一个测试的合成 SWF 手法值得学：用 `swf::write::write_swf` 现场造一个最小 SWF，避开对外部素材的依赖。而第二个测试反过来依赖外部素材，**缺失时打印 "skipping" 直接返回**（`swf_oracle.rs:182-186`）——CI 上不会红，但会**静默失去覆盖**。

---

## 6. `matrix.rs` / `transform.rs`

这两个模块是**解析期**的仿射变换工具（Ruffle vendored），走的是 twips 精度路线。它们**不参与镶嵌**（镶嵌在形状局部像素空间做），只服务于 `Transform` → `AnimTransform` 的转换和变换叠加。

### 表示

```rust
pub struct Matrix { pub a: f32, pub b: f32, pub c: f32, pub d: f32, pub tx: Twips, pub ty: Twips }
```

字段名沿用 SWF 的序列化名：`a` = scale_x，`b` = rotate_skew_0，`c` = rotate_skew_1，`d` = scale_y。映射是标准的列向量形式：

```
x' = a·x + c·y + tx
y' = b·x + d·y + ty
```

> ⚠️ 注意 `a,c` 在**第一行**、`b,d` 在**第二行**。行列式是 `a·d − b·c`。按字段名直觉读成 2×2 矩阵 `[[a,b],[c,d]]` 会全部搞错。

### `round_to_i32` —— Flash 的取整约定

```rust
// matrix.rs:288-304
/// Implements the IEEE-754 "Round to nearest, ties to even" rounding rule.
/// (e.g., both 1.5 and 2.5 will round to 2).
/// This is the rounding method used by Flash for the above transforms.
/// This also clamps out-of-range values and NaN to `i32::MIN`.
fn round_to_i32(f: f32) -> i32 {
    if f.is_finite() {
        if f < 2_147_483_648.0_f32 { f.round_ties_even() as i32 }
        else { i32::MIN }      // Out-of-range clamps to MIN.
    } else { 0 }               // NaN/Infinity goes to 0.
}
```

**注意文档注释与实现不符**：注释说"把越界值和 NaN 都钳到 `i32::MIN`"，但实现里 NaN/±∞ 走的是 `else` 分支返回 **0**。只有"有限但 ≥ 2³¹"才返回 `i32::MIN`。按实现描述应当是：

| 输入 | 结果 |
|---|---|
| 有限且 `< 2³¹` | `round_ties_even`（1.5 → 2，2.5 → 2） |
| 有限且 `≥ 2³¹` | `i32::MIN` |
| NaN / ±∞ | **`0`** |

`round_ties_even`（银行家舍入）是 Flash 的实际行为——`f32::round()` 是"远离零"，两者在 `.5` 上不同，会累积出可见偏差。

### 变换的每个环节都取整到 twips

```rust
// matrix.rs:193-203
fn mul(self, point: Point<Twips>) -> Point<Twips> {
    let out_x = Twips::new(round_to_i32(self.a * x + self.c * y).wrapping_add(self.tx.get()));
    let out_y = Twips::new(round_to_i32(self.b * x + self.d * y).wrapping_add(self.ty.get()));
    …
}
```

**每次变换后位置都被舍入回整数 twips**。这是 Flash 的行为（它的坐标本身就是整数 twips），复现它是为了对齐表现，代价是多次叠加会累积舍入误差（1/20 像素的量级）。

`Mul<Rectangle>`（`matrix.rs:217-236`）对四个角分别变换再取 AABB——这是唯一正确的做法（有旋转/斜切时直接缩放宽高会错）。空矩形返回 `Default::default()`。

### ⚠️ `Mul` 与 `MulAssign` 的加法不一致

```rust
// Mul (matrix.rs:179-180)
tx: round_to_i32(…).wrapping_add(self.tx.get()),
// MulAssign (matrix.rs:248-249)
tx: round_to_i32(…).wrapping_add…  ← 实际是普通 `+`
```

`Mul` 用 `wrapping_add`，而 `MulAssign` 用的是普通 `+`：

```rust
let (out_tx, out_ty) = (
    round_to_i32(self.a * rhs_tx + self.c * rhs_ty) + self.tx.get(),   // ← 普通加法
    round_to_i32(self.b * rhs_tx + self.d * rhs_ty) + self.ty.get(),
);
```

两份数学完全相同，但溢出行为不同：debug 构建下 `MulAssign` 会在 twips 溢出时 **panic**，而 `Mul` 会静默回绕。实际触发需要坐标累积到 ±2³¹ twips（约 ±10⁸ 像素），现实中不太可能——但这是模块里唯一的算术 panic 隐患，且是不一致引入的。

### `TransformStack` 未被使用

```rust
// transform.rs
pub struct TransformStack(Vec<Transform>);
impl TransformStack {
    pub fn push(&mut self, transform: &Transform) {
        let cur = self.transform();
        let matrix = cur.matrix * transform.matrix;                    // 父 × 子
        let color_transform = cur.color_transform * transform.color_transform;
        self.0.push(Transform { matrix, color_transform });            // 压入【绝对】变换
    }
    pub fn pop(&mut self) { assert!(self.0.len() > 1, "Transform stack underflow"); self.0.pop(); }
    pub fn transform(&self) -> Transform { self.0[self.0.len() - 1] }
}
```

设计是"压栈时就乘好绝对变换"，所以 `transform()` 是 O(1) 的，根变换永远不会被弹出（栈预先塞了一个单位阵）。做法是对的，但**全仓库没有任何地方引用它**——变换叠加实际发生在 `baked.rs:64-69` 的 `compose` 和 `animation.rs` 的 `AnimMatrix::mul` 里。

---

## 7. `filter_dest_rect` —— 离屏纹理尺寸

滤镜需要把对象渲染到一张离屏纹理再做模糊/投影，而这张纹理得开多大，取决于滤镜的几何扩张量。

```rust
// animation.rs:563-569
pub fn filter_dest_rect(off_x: f32, off_y: f32, w: f32, h: f32, filters: &[AnimFilter])
    -> (f32, f32, f32, f32)
```

输入是像素空间的 AABB，输出是扩张后的 `(offset_x, offset_y, pixel_width, pixel_height)`。

### 为什么不直接调 `swf` crate

文档注释写得很清楚（`animation.rs:558-562`）：

> *"Expand a pixel-space AABB per filter, matching swf crate's `calculate_dest_rect` math but operating directly in f64 pixels. … Pure math, no swf crate dependency."*

**目的**：让下游消费者（渲染器）能在**不链接 `swf` crate** 的前提下自己算离屏尺寸。这是运行时要独立于转换器的一个前提。代价是这套数学得**复制一遍**——所以配了 `filter_dest_rect_matches_swf_crate` 这个差分测试来保证不走样。

### 模糊扩张

```rust
// animation.rs:554-583
const PASS_SCALES: [f64; 15] = [
    1.0, 2.1, 2.7, 3.1, 3.5, 3.8, 4.0, 4.2, 4.4, 4.6, 5.0, 6.0, 6.0, 7.0, 7.0,
];

let blur_expand = |raw_blur: i32, num_passes: u8| -> f64 {
    let pass_index = num_passes.clamp(1, 15) as usize - 1;
    (raw_blur as f64 / 65536.0).max(0.0) * PASS_SCALES[pass_index]
};
```

- `raw_blur` 是 **16.16 定点**（`Fixed16`），除 65536 得到像素。
- **多趟模糊的累积半径不是线性的**：Flash 对 N 趟模糊用一个经验放大表 `PASS_SCALES`，`1 趟 = 1.0`、`2 趟 = 2.1`、`15 趟 = 7.0`。这张表来自 `swf` crate（它自己的注释说 *"very approximate to Flash, and not 100% exact"*）。
- `clamp(1, 15)`：`num_passes == 0`（不模糊）也被映射到下标 0（系数 1.0），避免除零/越界。负的模糊半径被 `.max(0.0)` 钳掉。

> `num_passes` 是**已经解码好的趟数**，不是原始 bitflags（见 [05 篇 §7](05-assets.md#7-滤镜的两套类型)）。

### 各滤镜的扩张方式

| 滤镜 | 扩张 |
|---|---|
| `BlurFilter` | 对称：`x0 -= bx; x1 += bx`（y 同理） |
| `GlowFilter` | 同 Blur |
| `DropShadowFilter` | 模糊扩张 **+ 单边偏移** |
| `BevelFilter` | 模糊扩张 **+ 双边 `abs()` 偏移** |
| `GradientGlowFilter` | 同 DropShadow |
| `GradientBevelFilter` | 同 Bevel |
| `ColorMatrixFilter` / `ConvolutionFilter` | **无几何扩张** |

**单边 vs 双边**是这里最实质的区别：

```rust
// DropShadow：影子只落在一侧
let dx = distance * angle.cos();
if dx < 0.0 { x0 += dx; } else { x1 += dx; }

// Bevel：斜角向四周对称扩散
let dx = (angle.cos() * distance).abs();
x0 -= dx;  x1 += dx;
```

DropShadow 按角度算出偏移向量，**只扩张影子落向的那一边**；Bevel 取绝对值后**两边都扩**。搞反了会导致离屏纹理要么裁掉影子、要么开得过大。

### 取整约定

```rust
// animation.rs:684-692
// Round the expanded rect out to whole pixels, matching the reference
// player (`swf_player/src/render.rs`): floor the min, ceil the max, then
// take the difference — `ceil(x1 - x0)` would under-size by up to 1 px.
(
    off_x + x0.floor() as f32,
    off_y + y0.floor() as f32,
    (x1.ceil() - x0.floor()) as f32,
    (y1.ceil() - y0.floor()) as f32,
)
```

**`floor(min)` / `ceil(max)` 后作差**，而不是 `ceil(max - min)`。后者在 `min` 有小数部分时会少算最多 1 像素（纹理边缘裁切）。注释指明这个行为对齐的是参考播放器的实现，`swf_oracle.rs` 的 `rounded_rect` 辅助函数也用了同样的取整，以便和上游做对比。

### 滤镜在线格式里的编码

这部分细节（`num_passes` 与 `flags` 的冗余、各位域的含义）归 [05 篇 §7](05-assets.md#7-滤镜的两套类型)。
