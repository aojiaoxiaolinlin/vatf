# 06 · 读取端 API 与播放语义

本篇是写播放器/渲染器的人需要的那部分：怎么打开文件、怎么取数据、每帧要做什么。

实现全部在 `src/reader.rs`。

---

## 1. `VabReader` API

```rust
pub struct VabReader {
    header: Header,
    chunks: HashMap<[u8; 4], AlignedChunk>,
    anim:   Option<AnimContainer>,
    baked:  BakedMovie,
}
```

### 构造

| 方法 | 位置 | 说明 |
|---|---|---|
| `open(path) -> Result<Self>` | `reader.rs:62` | `std::fs::read` 整个文件后转 `from_bytes` |
| `from_bytes(&[u8]) -> Result<Self>` | `reader.rs:69` | 全部解析与校验在这里 |

### 元信息

| 方法 | 位置 | 返回 |
|---|---|---|
| `header() -> &Header` | `reader.rs:172` | `{ version, length }` |
| `has_chunk(&[u8;4]) -> bool` | `reader.rs:177` | chunk 是否存在 |
| `chunk_types() -> impl Iterator<Item = &[u8;4]>` | `reader.rs:182` | 所有 FourCC（**顺序不定**，底层是 HashMap） |

### 动画数据

| 方法 | 位置 | 返回 |
|---|---|---|
| `baked() -> &BakedMovie` | `reader.rs:148` | **唯一的动画数据源**，已解析且已 `validate()` |
| `into_baked() -> BakedMovie` | `reader.rs:156` | 无克隆地取走 |
| `frame_rate() -> f32` | `reader.rs:200` | 转发到 `baked.frame_rate` |


### 静态几何（POD 切片）

| 方法 | 位置 | 元素类型 | 大小 |
|---|---|---|---|
| `shape_records()` | `reader.rs:205` | `ShapeRecord` | 8 B |
| `shape_meshes()` | `reader.rs:210` | `ShapeMesh` | 48 B |
| `gradient_uniforms()` | `reader.rs:215` | `GradientUniforms` | 40 B |
| `bitmap_uniforms()` | `reader.rs:220` | `[f32; 6]` | 24 B |
| `vertices()` | `reader.rs:231` | `Vertex` | 8 B |
| `indices()` | `reader.rs:236` | `u32` | 4 B |
| `morph_entries()` | `reader.rs:260` | `MorphEntry` | 24 B |
| `texture_data()` | `reader.rs:226` | `u8`（原始字节） | —— |

字段含义见 [01-format.md](01-format.md#3-pod-结构体字段表)。

---

## 2. `AlignedChunk`：为什么不是"真零拷贝"

```rust
// reader.rs:12-41
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
        Self { words, length: data.len() }
    }

    fn as_bytes(&self) -> &[u8] {
        let bytes: &[u8] = bytemuck::cast_slice(&self.words);
        &bytes[..self.length]
    }
}
```

这是 `reader.rs` 里设计感最强的一段。要理解它，先看清问题：

`.vab` 里的 payload 是**紧凑排布、无填充**的（[01 篇 §2](01-format.md#2-chunk-布局)）。假设某个 `VERT` chunk 的 payload 有 37 字节，下一个 chunk 的头紧接着它——**没有任何对齐保证**。

如果直接把它存进 `Vec<u8>`：
- `Vec<u8>` 的堆指针**由分配器决定**，可能是奇数地址；
- `bytemuck::try_cast_slice::<u8, Vertex>` 要求指针对齐到 `align_of::<Vertex>()`（= 4）；
- 不满足时它返回 **`Err`**，我们的 `cast_pod_slice` 转成 `None`。

于是**同一个文件、同样的内容，可能因为分配器恰好给了一个奇数地址而"解析失败"**——而且报的还是"文件损坏"这种误导性错误。这类 bug 的复现概率还很暧昧（时快时慢、因平台而异）。

**解法**：用 `Vec<u64>` 承载。`Vec<u64>` 的堆指针必然 8 字节对齐（分配器的 ABI 保证），而格式里最大的 `Pod` 对齐是 4，所以 8 字节对齐**覆盖所有情况**。读取时再 `cast_slice` 回 `&[u8]`，然后用 `length` 把尾部补的零切掉。

**代价**：

- **每个 chunk 有一次拷贝**，不是真正的零拷贝。真相是**"拷一次，之后全程零拷贝"**——`shape_meshes()` / `vertices()` 等访问器返回的都是这些缓冲区上的切片视图，不再有解析开销。
- 每个 chunk 最多浪费 7 字节尾部填充。

对于"文件本来就是从磁盘读进内存"的场景，这次拷贝是便宜的，而它换到的是**对齐的确定性**。这个交换很值。

> 一个隐含的推论：`AlignedChunk::as_bytes()` 返回的切片**长度是原始的 `length`**，所以 `cast_pod_slice` 看到的字节数与文件里完全一致，长度校验（[01 篇 §8](01-format.md#pod-元素的隐式长度校验)）依然有效。

---

## 3. 解析校验阶梯

`from_bytes` 从头到尾的检查，**顺序很重要**（先便宜的、先能排除大面积错误的）：

| # | 检查 | 位置 | 失败信息 |
|---|---|---|---|
| 1 | 大端主机 | `reader.rs:70-72` | `"VATF POD chunks require a little-endian host"` |
| 2 | `len >= 4 + size_of::<Header>()`（12） | `reader.rs:74-76` | `"VAB file too short"` |
| 3 | magic == `b"VATF"` | `reader.rs:77-79` | `"Not a VAB file: bad magic bytes"` |
| 4 | `version == VAB_VERSION` | `reader.rs:82-87` | `"Unsupported VAB version N (this build expects M)"` |
| 5 | `header.length == bytes.len()` | `reader.rs:88-94` | `"VAB length mismatch …"` |
| 6 | 每个 chunk 头不越界 | `reader.rs:102-104` | `"Truncated chunk header at offset …"` |
| 7 | `offset + length` 不溢出 | `reader.rs:110-112` | `"chunk length overflow"` |
| 8 | chunk 数据不越界 | `reader.rs:113-120` | `"Truncated chunk …"` |
| 9 | FourCC 不重复 | `reader.rs:122-124` | `"duplicate chunk …"` |
| 10 | **BAKD 必需** | `reader.rs:132` | `"VAB missing BAKD chunk"` |
| 11 | BAKD bincode 解析 | `reader.rs:133-138` | `"Corrupt BAKD chunk"` |
| 12 | **`baked.validate()`** | `reader.rs:139` | 各种不变量错误 |

第 1 条放在最前面是刻意的——**在碰任何字节之前**就确定字节序假设成立，避免后面所有 `pod_read_unaligned` 读到错误的值。

第 4 条是**陈旧文件探测器**：产物是旧布局时，这里会让你看到一句能直接照做的错误，而不是让 bincode 去按新布局解一堆旧字节（[01 篇 §8](01-format.md#8-版本与兼容性)）。

第 12 条意味着**手工构造的 BAKD 是读不进来的**：校验器是加载器的一部分，不只是写端的自检。

### 头字段用 `pod_read_unaligned` 读

```rust
let header: Header = bytemuck::pod_read_unaligned(&bytes[4..12]);
…
let chunk_header: ChunkHeader =
    bytemuck::pod_read_unaligned(&chunk_data[offset..offset + CHUNK_HEADER_SIZE]);
```

文件偏移 4 处对 `Header`（`align_of` = 4）**碰巧**是对齐的，但 chunk 头出现在任意偏移，不能假定对齐。`pod_read_unaligned` 走的是逐字节拷贝路径，对任意偏移都安全——**这是唯一正确的选择**。

### `cast_pod_slice` 的两点语义

```rust
// reader.rs:197-202
fn cast_pod_slice<T: bytemuck::Pod>(data: &[u8]) -> Option<&[T]> {
    if data.is_empty() { return Some(&[]); }
    bytemuck::try_cast_slice(data).ok()
}
```

1. **空 chunk → `Some(&[])`，缺失 chunk → `None`**。要区分"没写"和"写了但是空的"，得配合 `has_chunk`。
2. **`try_cast_slice` 的失败就是长度校验**——payload 长度不是 `size_of::<T>()` 的整数倍时返回 `Err`，转成 `None`。**失败方向是安全的**（`None` 而不是 UB），所以这里不需要额外的长度检查。

> ⚠️ 副作用：**"缺失"和"格式非法"在 API 上都表现为 `None`**。调用方无法从返回值区分"这个 chunk 没写"和"这个 chunk 写坏了"。

---

## 4. 帧 → 绘制列表

BAKD 路径下，**取到某一帧的绘制列表就是一个数组索引**：

```rust
let clip  = movie.clips.iter().find(|c| c.name == "idle").unwrap();
let nodes = &clip.frames[playhead];      // ← 这就是该帧的完整绘制列表
```

`frames[i]` **就是**第 `i` 帧的全部内容——没有差分、没有增量、没有"应用这一帧的变更"。这个性质由 [02 篇 §3](02-pipeline.md#3-显示列表模型--parse_tags) 的 `ShowFrame` 快照保证。

### clip 帧号 ↔ 根帧号

```rust
// baked.rs:158-164
for (frame, display) in root.iter().enumerate().take(end).skip(*start) {
    frames.push(compiler.list(&display.entries, frame, …)?);
}
clips.push(BakedClip { name: …, start_frame: *start as u32, frames, events: … });
```

所以：

```
clip.frames.len() == end - start
clip.frames[i]    对应根时间轴的第 (start_frame + i) 帧
```

**事件也是同一套索引**——`FrameEvent::frame` 已经是 clip 相对帧（[04 篇 §3.2](04-animation.md#32-事件重定基)）。所以"派发当前帧的事件"就是：

```rust
for ev in clip.events.iter().filter(|e| e.frame as usize == playhead) {
    dispatch(&ev.name);
}
```

### 运行时不需要记录任何 per-instance 状态

这是 BAKD 最重要的性质：运行时需要维护的状态只有 **clip 播放头 + 每个 `Skin.symbol` 选中的变体索引**。

因为嵌套 sprite 已经在**烘焙期**按每个根帧展开成了子树（[04 篇 §2](04-animation.md#2-子时间轴播放公式)），运行时看到的是一个已经"拍平到当前瞬间"的树——不需要每个 sprite 实例的独立时间轴位置，也不需要每个实例的变换栈。

### 遍历伪代码

```rust
fn draw(nodes: &[BakedNode], parent_transform: AnimTransform, t: &mut RenderTarget) {
    for node in nodes {
        match node {
            // ① 世界变换已经乘好了，直接用
            BakedNode::Shape { id, ratio, transform } => {
                let mesh = resolve_shape(*id, *ratio, &t.morph_table, &t.shape_table);   // 见 §7
                t.draw_mesh(mesh, *transform);
            }

            // ② 变体是【局部空间】的，要乘上 Skin 自己的世界变换
            BakedNode::Skin { symbol, transform, .. } => {
                let variant = &t.skins[*symbol].variants[t.variant_for(*symbol)];
                draw(&variant.nodes, compose(*transform, AnimTransform::default()), t);
                //                                             ↑ 等价于：把 transform 作为新的父变换
            }

            // ③ Group：子节点已带世界变换，这里只负责滤镜/混合
            BakedNode::Group { children, filters, blend_mode } => {
                t.push_offscreen(filters);
                draw(children, AnimTransform::default(), t);   // 不再传变换
                t.pop_offscreen(*blend_mode);
            }

            // ④ Mask：先画 mask 建 stencil，再画 children
            BakedNode::Mask { mask, children } => {
                t.begin_stencil();
                draw(mask, AnimTransform::default(), t);
                t.end_stencil();
                draw(children, AnimTransform::default(), t);
            }
        }
    }
}
```

注意 ③④ 里传给子节点的父变换是**单位阵**——因为它们的子节点已经带了世界变换，再乘一次就重复了。而 ② 必须把 `Skin.transform` 传下去，因为变体是局部空间的。

---

## 5. 变换下发规则

回到 [04 篇 §3.5](04-animation.md#35-bakednode-的四种节点) 的表，具体到渲染：

| 节点 | `transform` 的空间 | 渲染时 |
|---|---|---|
| `Shape` | 世界 | 直接用 |
| `Skin` | 世界 | 用它作为**新的父变换**去渲染变体（变体内部是局部空间） |
| `Group` | —— | 不需要；子节点自带世界变换 |
| `Mask` | —— | `mask` 和 `children` 都在世界空间 |

### 组合函数

世界变换的组合就是 `AnimMatrix` / `AnimColorTransform` 的乘法：

```rust
fn compose(parent: AnimTransform, local: AnimTransform) -> AnimTransform {
    AnimTransform {
        matrix: parent.matrix * local.matrix,
        color_transform: parent.color_transform * local.color_transform,
    }
}
```

乘法语义是"**local 先作用**"（[04 篇 §1](04-animation.md#animtransform-与两个乘法)）。`AnimDisplayObject.transform` 里的 `tx` / `ty` **单位是像素**，不是 twips。

### 顶点反量化

`Shape` 只给了 shape id，顶点要自己解：

```rust
let mesh = &shape_meshes[mesh_index];
let v = &vertices[mesh.vertex_offset + i];
let local_x = v.x as f32 / 32767.0 * mesh.bounds_half_x + mesh.bounds_center_x;
let local_y = v.y as f32 / 32767.0 * mesh.bounds_half_y + mesh.bounds_center_y;
```

公式与推导见 [01 篇 §4](01-format.md#4-顶点量化)。反量化参数**就在 `ShapeMesh` 里**，不需要任何外部上下文。

### 材质

```rust
match mesh.material_type {
    material::COLOR    => t.draw_color(&verts, &indices, transform, mesh, vertex_colors),
    material::GRADIENT => {
        let u = &gradient_uniforms[mesh.material_offset];
        let tex = decode_webp(&texture[mesh.texture_offset..][..mesh.texture_length]);
        t.draw_gradient(&verts, &indices, transform, u, tex);
    }
    material::BITMAP   => {
        let m = &bitmap_uniforms[mesh.material_offset];      // [f32;6]
        let tex = decode_webp(&texture[mesh.texture_offset..][..mesh.texture_length]);
        t.draw_bitmap(&verts, &indices, transform, m, tex, mesh.sampler_flags);
    }
}
```

`sampler_flags`：bit0 = smoothed（线性过滤），bit1 = repeating（`Repeat` 否则 `Clamp`）。

### 纹理坐标从哪来

**顶点里没有 UV**。渐变/位图的纹理坐标要**在着色器里**用 `GradientUniforms::texture_transform` / `bitmap_uniforms` 那个 6 元素仿射作用于（反量化后的）顶点位置算出来。

> ⚠️ 这个 6 元素的排布在仓库内**存在一处文档与实现的不一致**，见 [03 篇 §8.3](03-geometry.md#83-渐变矩阵--f3233) 的推导与警告。写渲染器时以实测为准。

---

## 6. Mask 的渲染语义

```rust
BakedNode::Mask { mask: Vec<BakedNode>, children: Vec<BakedNode> }
```

- `mask` 和 `children` **都在世界空间**（`Mask` 节点本身不带变换）。
- 渲染顺序：先画 `mask` 建立 stencil，再画 `children`（只在 stencil 通过的区域）。
- **不需要 stencil 栈**。格式在烘焙期就禁止了交叉/嵌套的遮罩区间（[04 篇 §4.1](04-animation.md#41-list--mask-扫描)），所以任意时刻**最多只有一层活跃的遮罩**——一个深度/模板缓冲即可。

不过注意 `children` 内部**可以**再有 `Mask` 节点（那是顺序关系而非嵌套区间），所以渲染循环里递归处理即可，栈深度仍然受控。

`mask` 是个 `Vec` 而不是单个节点，因为遮罩对象本身可能是个 sprite（展开成子树）或被包进了 `Group`。

---

## 7. Morph 查表

`BakedNode::Shape` 带 `ratio` 字段，但**它同时用于普通形状和 morph 形状**——编译器不查 MORP 表（[04 篇 §4.2](04-animation.md#分支-c叶子形状)）。运行时必须自己判别：

```
传入 Shape { id, ratio }：
    若 MORP 里有 id  →  按 (id, ratio) 查 MORP
    否则             →  按 id 查 SHAP
```

### 两级 map 是必需的

```rust
// reader.rs:255-262 的文档注释推荐
FnvHashMap<u16, FnvHashMap<u16, &MorphEntry>>
```

**同一个 `(morph_id, ratio)` 会有多行**——一个 morph 形状可能产生多个 draw call（每个 mesh 一行）。所以：

- ❌ 扁平切片上 `filter(|e| e.morph_id == id && e.ratio == r).next()` —— **只对单 draw 的 morph 正确**，多 draw 时会漏掉后续 mesh，形状缺块。
- ✅ 两级 map，第二级拿到的是 `Vec<&MorphEntry>`（或按插入顺序遍历）。

### ratio 只能吸附，不能插值

只有**时间轴里真实出现过**的 ratio 被烘焙（[02 篇 §7](02-pipeline.md#7-process_morphs--惰性插值)）。如果宿主想做"ratio = 30000"这种中间值，表里查不到——只能取最接近的已烘焙值。

### `MorphEntry` 不带材质

`MorphEntry` 只有顶点/索引区间，**没有 `material_type`**。要么通过 `vertex_offset` 反查对应的 `ShapeMesh`，要么约定 morph 只产生单一材质。

---

## 8. 播放与定时

### 帧率来自 `BakedMovie`

```rust
// reader.rs
pub fn frame_rate(&self) -> f32 {
    self.baked.frame_rate
}
```

**全文件只有一个帧率**——SWF 的帧率在 movie header 上，`DefineSprite` 没有自己的帧率，子 sprite 每父帧推进一帧（[04 篇 §3.4](04-animation.md#34-定时信息在-bakedmovieframe_rate)）。所以播放器只需要取一个值：

```rust
let fps = reader.frame_rate();
let seconds_per_frame = 1.0 / fps;                    // 注意 fps 可能是 0（空 movie）

// 每帧：
accumulator += delta_time;
while accumulator >= seconds_per_frame {
    accumulator -= seconds_per_frame;
    playhead = (playhead + 1) % clip.frames.len();
    // 注意：跨越的每一帧都要派发它的事件，不能只派发最终那一帧
}
```

> ⚠️ 用 `while` 而不是 `if`：如果某一帧渲染耗时超过一帧时长（掉帧），需要**补跑**多个逻辑帧，否则动画会变慢而不是跳帧。这是所有固定步长播放器都要处理的。

> ⚠️ `frame_rate()` 返回 `f32` 而不是 `Option`，因为 `BAKD` 是必需 chunk。但它**可能合法地为 0**——没有根时间轴、或根时间轴为空的 movie 会返回 `BakedMovie::default()`，此时也没有 clip 可播。有 clip 就必然有有效帧率（`validate()` 保证有限且非负，烘焙期保证来自真实 SWF）。所以先判断 `clips.is_empty()`，或者给一个 `DEFAULT_FRAME_RATE` 兜底。

### 选 clip

```rust
// 按名字
let clip = movie.clips.iter().find(|c| c.name == name)?;

// 或者按根帧号定位（用于"从时间轴第 N 帧开始播"）
let clip = movie.clips.iter().rev().find(|c| c.start_frame <= root_frame)?;
let local_playhead = root_frame - clip.start_frame;
```

`BakedClip::start_frame` 是 clip 在**根时间轴**上的起始帧。

### clip 是铺满根时间轴的

clip 区间是 `[start_i, start_{i+1})`，最后一个到根时间轴末尾（[04 篇 §3.1](04-animation.md#31-clip-发现)）。所以：

- 任意根帧都**恰好属于一个** clip，没有空隙；
- 没有 `anim_` 标签时会有唯一的 `"default"` clip 覆盖全程。

### 事件

```rust
for ev in clip.events.iter().filter(|e| e.frame as usize == playhead) {
    dispatch(&ev.name);      // 名字已经剥掉了 "event_" 前缀
}
```

`clip.events` 按帧号**非降序**（`validate()` 保证了这一点），同帧多个事件**保持源顺序**（烘焙时用了稳定排序）。

---

## 9. 交给宿主的部分

`.vab` 只负责"几何 + 每帧的绘制列表 + 变换 + 材质参数"。以下都需要渲染器实现：

| 事项 | 说明 |
|---|---|
| **滤镜的实际渲染** | 格式只给参数（`AnimFilter`）和离屏尺寸算法（`filter_dest_rect`）；模糊/投影/斜角要自己实现 |
| **混合模式合成** | `Group::blend_mode` 是 `swf::BlendMode` 的判别值（`Normal=0`, `Layer=2`, `Multiply=3`, …, `HardLight=14`） |
| **`Group` 的离屏渲染** | 有滤镜或非 Normal 混合时先渲到离屏纹理，再合成回去 |
| **WebP 纹理解码** | `TEXT` 里全是 WebP 字节 |
| **几何抗锯齿** | 顶点里没有法线（[03 篇 §7](03-geometry.md#顶点构造没有-uv没有法线)），需要 MSAA 或后处理 |
| **遮罩的 stencil 实现** | 一层即可，但要自己搭 |
| **色彩空间处理** | `GradientUniforms::interpolation == 1` 表示要在线性空间插值（[03 篇 §8.1](03-geometry.md#81-去重的粒度斜坡全局共享矩阵每-draw-一份)） |

另外，`AnimFilter` 上提供的 `scale` / `impotent`（`animation.rs:160-178`）是**给这些宿主实现的钩子**——本仓库里没有调用者（[05 篇 §7](05-assets.md#三个方法是死代码)）。`impotent()` 的用途是"这个滤镜不会产生任何效果，可以跳过"，能省掉一次离屏渲染。

---

## 10. 一页速查

```
打开
  VabReader::open(path)?                        // 会做全部校验 + baked.validate()

取静态数据（零拷贝切片）
  shape_records()  → id → [sub_shape_offset, +sub_shape_count)
  shape_meshes()   → vertex/index/材质/纹理区间 + 反量化参数
  vertices()       → i16 量化坐标 + 颜色
  indices()        → u32 三角形
  gradient_uniforms() / bitmap_uniforms() / texture_data()
  morph_entries()  → 构建 morph_id → ratio → [entry] 两级 map

取动画
  baked()          → BakedMovie { frame_rate, clips, skins }

每帧
  nodes = clip.frames[playhead]                // 完整绘制列表
  递归绘制 nodes（Shape / Skin / Group / Mask）
  派发 clip.events 中 frame == playhead 的项

播放头推进
  playhead = (playhead + 1) % clip.frames.len()
  步长 = 1.0 / frame_rate
```
