# 02 · SWF → VAB 转换主流程

> 本篇保留源码分析摘录和历史行号；当前编译模式、UI、裁剪与预处理契约见 [08](08-ui-and-compilation.md)。源码行号可能随重构变化。

入口在 `src/main.rs`，核心在 `src/lib.rs` 的 `convert_swf_to_vab`（`src/lib.rs:577-627`）与 `parse_tags`（`src/lib.rs:635-757`）。

---

## 1. CLI

```
vatf <input> [-o|--output <path>]
```

`input` 是文件或目录，行为分两种模式（`src/main.rs:23-34`）：

### 单文件模式

输出路径解析（`resolve_output`，`src/main.rs:107-123`）：

| `--output` | 结果 |
|---|---|
| 未给 | `<输入文件的父目录>/<文件名>.vab` |
| 是**已存在**的目录 | `<该目录>/<文件名>.vab` |
| 其他 | 当作完整输出路径（含 `.vab`） |

### 目录模式

扫描目录下所有 `.swf`（**只扫一层，不递归**），排序后逐个转换：

- 默认输出目录是 `<输入目录>/output`（`src/main.rs:55-56`）。

  > 注意 `--output` 的帮助文本写的是 *"defaults to ./output"*，实际是 `<输入目录>/output`——帮助文本不够精确。

- 逐个打印进度与结果大小，最后汇总 `── Done — N succeeded, M failed ──`。
- **只要有失败就返回 `Err`**（`src/main.rs:97-101`），进程退出码非零。成功与失败的明细在此之前已经打印到 stdout/stderr。

### 输出目录会自动创建

```rust
// src/lib.rs:613-615
if let Some(parent) = output.parent() {
    std::fs::create_dir_all(parent)?;
}
```

---

## 2. `convert_swf_to_vab` 的五个阶段

```
convert_swf_to_vab                                        lib.rs:577
  │
  ├─ 0. 扩展名检查：必须是 .swf                          lib.rs:578-580
  │
  ├─ 1. decompress_swf(BufReader<File>)                 lib.rs:584
  │      解压 CWS/ZWS → 原始 SWF 字节
  │
  ├─ 2. parse_swf(&swf_buf) → Vec<Tag>                  lib.rs:585
  │      纯标签解析，不做像素解码
  │      同时取出 frame_rate = swf.header.frame_rate().to_f32()
  │
  ├─ 3. parse_tags(…)                                   lib.rs:597-604
  │      ┌──────────────────────────────────────────────┐
  │      │ 边解析边做三件事（详见 §3）：                 │
  │      │  · 形状 → 立刻镶嵌落盘                        │
  │      │  · 位图/morph → 只登记，延后处理              │
  │      │  · 显示列表 → 累积成 animations               │
  │      └──────────────────────────────────────────────┘
  │
  ├─ 4. process_morphs(&mut builder, &bitmap)           lib.rs:611
  │      对时间轴里真实出现过的 (morph_id, ratio) 插值+镶嵌
  │      （必须等 parse_tags 结束，因为要先知道哪些 ratio 出现过）
  │
  └─ 5. write_vatf(output)                              lib.rs:617
         组装 10 个 chunk → 烘焙 BAKD → 写文件
```

计时埋点分两段：`parse_tags` 用时记为 `parsed`，`write_vatf` 用时记为 `write`，最后一行日志输出 sprite 数量与两段耗时。

### 一个实现细节：`animations` 为什么单独传

```rust
// src/lib.rs:594-608
let mut animations = HashMap::default();
parse_tags(swf.tags, &mut builder, &mut animations, 0, &mut bitmap, &mut jpeg_tables)?;
let sprite_count = animations.len();
builder.animations = animations;      // ← 结束后才装回 builder
```

`parse_tags` 递归处理 sprite 时也需要写 `builder`（形状、渐变、位图），所以 `animations` 只能作为**独立参数**传下去，等全部递归结束再一次性交给 `builder`。`process_morphs` 依赖 `builder.animations` 才能知道要处理哪些 ratio，所以这一步的先后顺序是必需的。

- `parse_tags` 对**每个** sprite（含根，`sprite_id = 0`）都会 `animations.insert(sprite_id, timeline)`（`src/lib.rs:755`）。
- 所以 `sprite_count`（日志里的 "N sprites"）是**所有时间轴的总数**，含根。

---

## 3. 显示列表模型 —— `parse_tags`

这是整个转换器的核心状态机。它把 SWF 那套"命令式"的标签流，还原成"每一帧都在场的完整对象列表"。

### 状态（`src/lib.rs:647-648`）

```rust
let mut current_frame: BTreeMap<Depth, DisplayObject> = BTreeMap::new();
let mut timeline: Vec<Vec<DisplayObject>> = Vec::new();
```

源码注释把关键点说得很清楚：

> *"`BTreeMap` used during frame construction for sorted insert/lookup/remove by depth. Cloned into a `Vec` at each `ShowFrame` — depth ordering is preserved. Objects **persist across frames until an explicit `RemoveObject`**, so the map is *not* cleared at frame boundaries."*

这两句话解释了为什么必须用 `BTreeMap`：

1. **按 depth 排序**。`BTreeMap` 的迭代顺序就是 depth 升序，而 SWF 的绘制顺序正是 depth 升序——所以 `ShowFrame` 时直接 `.values().cloned().collect()` 得到的就是正确的绘制顺序。
2. **按 depth 定点查改**。`Modify` / `Replace` / `RemoveObject` 都是"按 depth 找到那个对象"，`BTreeMap` 的 `get_mut` / `remove` 是 O(log n)，而 `Vec` 得线性扫。

**跨帧持久**是这个模型最重要的性质：SWF 里一个对象只要没被 `RemoveObject` 就一直在舞台上，后续帧不重复发 `PlaceObject`。这和"每帧发一遍所有对象"的直觉模型完全不同——`tests/swf_oracle.rs` 就是专门为了守住这条语义而存在的（见 [04 篇](04-animation.md#5-差分测试oracle)）。

### `PlaceObject` 的三态（`src/lib.rs:687-707`）

```rust
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
```

| 动作 | 语义 | `place_frame` |
|---|---|---|
| `Place(id)` | 在 depth 上**新建**一个实例 | 设为当前帧 |
| `Modify` | **就地合并**字段到已有实例 | 保持不动 |
| `Replace(id)` | 换角色 id + 合并字段 | **重置**为当前帧 |

三个要点：

- **`apply_place_object` 是"部分更新"语义**（`src/lib.rs:238-261`）：只覆盖 `PlaceObject` 里**显式给了**的字段（`matrix` / `color_transform` / `ratio` / `blend_mode` / `filters` 都是 `Option`，给了才覆盖），其余字段保持原值。这正是 `Modify` 能只改一个矩阵而不丢名字、不丢滤镜的原因。
- **`Replace` 为什么要重置 `place_frame`**：换角色相当于换了个新实例，它的子时间轴应该从头开始跑。源码注释直接写了这一点。
- **`place_frame = timeline.len()`**：`timeline.len()` 是"已经完成的帧数"，也就是**当前正在构建的那一帧的序号**。所以 `place_frame` 的语义是"这个实例首次出现在第几帧"。它在 [04 篇](04-animation.md#2-子时间轴播放公式) 会和父帧号一起参与子时间轴播放位置的计算。

### `RemoveObject`

```rust
Tag::RemoveObject(remove_object) => { current_frame.remove(&remove_object.depth); }
```

### `ShowFrame` —— 唯一的快照点

```rust
Tag::ShowFrame => {
    // Full display list for this frame: every object still on stage,
    // in ascending depth order (BTreeMap iteration order = SWF paint order).
    timeline.push(current_frame.values().cloned().collect());
}
```

每遇到一个 `ShowFrame` 就把**当前所有在场对象整套克隆**进 `timeline`。

> 这意味着内存占用是 `O(帧数 × 每帧对象数)`，而且**没有做任何差分压缩**——每一帧存的都是完整列表。这是刻意的：运行时因此不需要"重放差分"，代价是文件更大。`baked.rs:71` 的注释表达的也是同一个取向。

### 一个例子

假设标签流是：

```
frame 0: PlaceObject(depth=1, id=A), PlaceObject(depth=3, id=B), ShowFrame
frame 1: PlaceObject(depth=1, Modify, matrix=M), ShowFrame
frame 2: RemoveObject(depth=3), ShowFrame
```

`timeline` 的结果：

| 帧 | 内容 |
|---|---|
| 0 | `[(1, A), (3, B)]` |
| 1 | `[(1, A'), (3, B)]` — A 的矩阵变成 M，B 原样保留 |
| 2 | `[(1, A')]` — B 被移除 |

注意第 1、2 帧**没有**重新 `PlaceObject` A 和 B，但它们在列表里依然存在——这就是"跨帧持久"。

---

## 4. `FrameLabel` 的三类分流

帧标签按**所在时间轴**和**名字前缀**分流成三种完全不同的东西（`src/lib.rs:720-749`）。

### 根时间轴（`sprite_id == 0`）

```rust
if name.starts_with("event_") {
    builder.event_labels.push((name, timeline.len()));
} else if builder.frame_labels.insert(name.clone(), timeline.len()).is_some()
    && name.starts_with("anim_")
{
    bail!("duplicate animation label {name}");
}
```

| 前缀 | 去向 | 容器 | 重复处理 |
|---|---|---|---|
| `event_*` | `event_labels` | `Vec<(Box<str>, usize)>` | **允许**同帧多个事件（所以是 Vec） |
| `anim_*` | `frame_labels` | `HashMap` | **报错** |
| 其他 | `frame_labels` | `HashMap` | 静默覆盖 |

注意 `event_*` 不进入动作标签表。当前烘焙器将所有非事件根标签作为动作，`anim_` 只是可选的剥离前缀。上面的解析分支仍只对原始 `anim_*` 重复名称直接报错，其他原始同名标签会覆盖；制作端必须保持动作名唯一，不能依赖覆盖行为。烘焙器另会拒绝剥离前缀后的名称冲突、空动作名及同帧多个动作。

### 非根时间轴（sprite 自己的时间轴）

```rust
} else {
    ensure!(!name.is_empty(), "empty frame label on sprite {sprite_id}");
    let variants = builder.skin_variants.entry(sprite_id).or_default();
    ensure!(variants.iter().all(|(v, _)| v != &name), "duplicate frame label {name} …");
    ensure!(variants.last().is_none_or(|(_, f)| *f != timeline.len()),
            "multiple frame labels on sprite {sprite_id} frame {}", timeline.len());
    variants.push((name, timeline.len()));
}
```

这些标签成为 **skin 变体的候选**（[04 篇](04-animation.md#42-object--三条分支)）。写入前就强制了两条不变量：

- 同一 sprite 上**变体名不可重名**；
- 同一 sprite 的**同一帧上至多一个标签**。

> ⚠️ 关键语义：**有帧标签 ≠ 是 skin**。标签只是"登记了候选变体"，真正决定它是不是 skin 的是**实例名有没有 `skin_` 前缀**（`src/baked.rs:245-248`）。`baked.rs:487` 的测试 `frame_labels_do_not_turn_an_unmarked_instance_into_a_skin` 专门固定了这一点。

---

## 5. 位图：只登记，不解码

解析阶段**不做像素解码**，只把原始压缩数据存起来（`src/lib.rs:660-672`）：

| 标签 | 处理 |
|---|---|
| `JpegTables` | `register_jpeg_tables` —— 全局一张表，存到 `jpeg_tables` |
| `DefineBits` (JPEG1) | 与 JPEGTables 粘连，只解**尺寸** |
| `DefineBitsJpeg2` | 只解尺寸 |
| `DefineBitsJpeg3` | 只解尺寸 + 存 alpha 原始数据 |
| `DefineBitsLossless` | 原样存（连 zlib 都不解） |

也就是说，转换阶段为了构造 `ShapeMesh` 需要知道尺寸的，就只解到尺寸为止（`decode_define_bits_jpeg_dimensions`）；真正昂贵的全像素解码推迟到 **tessellate 用到这个位图填充时**才发生（`src/lib.rs:415`），而且一旦解码就立刻转 WebP（`src/lib.rs:430-434`）。

这套延迟策略的意义：**SWF 里没被任何形状引用的位图，永远不会被解码**。

关于 JPEGTables 那套字节手术，见 [05 篇](05-assets.md#3-jpegtables为什么需要字节手术)。

---

## 6. `write_vatf` 的组装

### 从内存里的容器直接烘焙

```rust
// src/lib.rs
let morph_bytes: &[u8] = bytemuck::cast_slice(&self.morph_entries);

let container = animation::AnimContainer::from_parts(
    &self.animations, &self.frame_labels, self.frame_rate,
);
let baked = baked::bake_with_options(&container, &self.event_labels, &self.skin_variants, self.root_translation)?;
baked.validate()?;
let baked_bytes = bincode::serialize(&baked)?;
```

`from_parts` 里的排序归一化（sprite id 升序，再 `(frame, name)` 升序）**是烘焙输出确定性的来源**——它把 `HashMap` 的不确定迭代序变成确定的 `Vec` 序。`BakedMovie.frame_rate` 也是从这里带过去的。

> **历史注记**：早期版本会把这份容器**序列化成 `ANIM` chunk 再反序列化回来**喂给烘焙器（代号"往返"）。它有两个理由——"让 ANIM 与 BAKD 由构造保证同源"和"顺带回归测试编解码器"。`ANIM` 删除后这两个理由都消失了，而每次转换还要为 0.5–2.4 MB 的动画数据白付一次完整序列化+解析，所以改成直接调用 `from_parts`。

### chunk 列表与长度计算

```rust
// src/lib.rs
let chunks: [ChunkSpec; 9] = [
    (b"BAKD", &baked_bytes), (b"SHAP", shape_records), (b"SHME", shape_meshes),
    (b"GRAD", gradient_uniforms), (b"BMAP", bitmap_uniforms), (b"TEXT", texture),
    (b"VERT", vertices), (b"INDX", indices), (b"MORP", morph_bytes),
];

let payload_size: u32 = chunks.iter()
    .map(|(_, data)| data.len() as u32 + CHUNK_HEADER_SIZE as u32)
    .sum();
```

文件总长 = magic(4) + header(8) + Σ(chunk 头 8 + payload)。然后：

```rust
file.write_all(&[MAGIC_BYTES, bytemuck::cast_slice(&[file_header])].concat())?;
```

先写文件头，再逐 chunk 组装到 `raw_payload` 里一次性写出。

### `bake_with_options` 是序列化入口的烘焙调用点

```rust
pub fn bake(container: &AnimContainer, extra_events: &[(Box<str>, usize)]) -> Result<BakedMovie> {
    bake_with_skin_variants(container, extra_events, &HashMap::new())
}
```

`bake` 就是"没有 skin 变体"的便利包装；写端用的是完整版，把 `self.skin_variants` 传进去（[04 篇](04-animation.md#42-object--三条分支)）。

---

## 7. `process_morphs` —— 惰性插值

morph 形状不能在解析时立刻处理，因为**要先知道哪些 ratio 在时间轴里真实出现过**。

```rust
// src/lib.rs:862-873
let mut pairs: Vec<(u16, u16)> = Vec::new();
for frames in builder.animations.values() {
    for frame in frames {
        for obj in frame {
            if builder.morph_shapes.contains_key(&obj.id) && !pairs.contains(&(obj.id, obj.ratio)) {
                pairs.push((obj.id, obj.ratio));
            }
        }
    }
}
```

这是一次 `O(总对象数 × 已有对数)` 的扫描（`contains` 是线性查找，没有用 set 去重）。对一个中等规模的 SWF 无所谓，但这是源码里明显可优化的一处。

然后排序（`pairs.sort()`）并对每对做"插值 → 镶嵌 → 记录"：

```rust
// src/lib.rs:888-908
for &(morph_id, ratio) in &pairs {
    let morph_data = &builder.morph_shapes[&morph_id];
    let shape = morph::interpolate(&morph_data.start, &morph_data.end, ratio);

    // Morph meshes are addressed through MORP, so they must not emit a
    // ShapeRecord (which would pollute `shape_map`, notably key 0).
    let (mesh_start, _mesh_count) = builder.process_shape_geometry(&shape, bitmap);
    …
}
```

> **注意这里用的是 `process_shape_geometry` 而不是 `process_swf_shape`**。后者会额外 push 一条 `ShapeRecord`。morph 网格必须**不写** `ShapeRecord`，否则会污染 `shape_map`——尤其 morph 插值产生的 shape 的 `id` 是 **0**（`src/morph.rs:130`），而 0 是查找表里的一个合法键。
>
> 代价是运行时**无法只靠 `SHAP` 判断一个 id 是不是 morph**，必须查 `MORP` 表（见 [01 篇 §6](01-format.md#6-morp-表与-morph-查表)）。

关于插值算法本身（直线↔曲线升格、`edge_bounds` 为什么必须 lerp），见 [03 篇](03-geometry.md#9-morph-插值)。

---

## 8. 错误与校验点

转换阶段的失败策略是**"能报错就报错，能继续就继续"**：

| 情况 | 行为 | 位置 |
|---|---|---|
| 输入不是 `.swf` | `bail!` | `lib.rs:578-580` |
| 重复的 `anim_` 标签 | `bail!` | `lib.rs:731-732` |
| sprite 上空标签 | `ensure!` | `lib.rs:734` |
| sprite 上变体重名 | `ensure!` | `lib.rs:736-739` |
| sprite 同帧多标签 | `ensure!` | `lib.rs:740-746` |
| 位图解码失败 | `error!` + `continue`（丢弃该 mesh） | `lib.rs:418-420` |
| 镶嵌失败 | `error!`（不中断） | `tessellator.rs:205-216` |
| 多个 `JPEGTables` | `eprintln!` 警告 | `lib.rs:764-766` |
| 未知/不关心的标签 | `_ => {}` 静默跳过 | `lib.rs:751` |

⚠️ **位图解码失败的那一条值得注意**：`error!` 之后 `continue`，结果是一个**形状少了某个填充**，而转换整体仍然成功。日志里能看到，但文件看起来"正常"。

其余值得注意的失败点（`ensure!`）都在烘焙阶段，汇总在 [04 篇 §4.3](04-animation.md#43-validate)。

---

## 9. 转换期的去重与体积控制

转换器压缩体积的手段全在这三处：

| 手段 | 位置 | 效果 |
|---|---|---|
| 顶点量化成 `i16` | `lib.rs:185-206` | 每顶点位置 8 B → 4 B |
| 纹理按字节内容 intern | `lib.rs:293-301` | 相同位图/斜坡只存一份 |
| 渐变**斜坡**去重（不含矩阵） | `tessellator.rs:74-76` | 同一色带跨 shape 共享一张 256×1 纹理 |
| 索引用 `u32` | `lib.rs:363` | 没有按顶点数降级到 `u16` |

对比之下**没有**做的：帧间差分压缩、顶点/索引的跨 shape 复用、索引宽度自适应。这些都是可以继续优化的方向（见 [07 篇](07-design-notes.md)）。

## 根平移策略更新（2026-10-07）

默认编译保留原始根平移（`RootTranslationPolicy::Preserve`）。上述自动归零说明
仅适用于显式选择 `NormalizeClipStart` 的动作素材表；CLI 使用
`--normalize-clip-start`，共享 `SwfCompileSettings` 使用 `root_translation` 字段。
设置缺省字段兼容旧元数据，但缺省语义现在为保留。UI 导出和无标签场景不受影响。
编译器修订号更新为 2，VAB 格式版本仍为 1。
