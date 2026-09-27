# 01 · `.vab` 二进制格式规范

文件格式常量定义在 `src/lib.rs:45-60`，读取实现在 `src/reader.rs`。

```rust
pub const MAGIC_BYTES: &[u8; 4] = b"VATF";
pub const VAB_VERSION: u32 = 1;
```

> 注意命名不一致：magic 是 `VATF`，crate 名是 `vatf`，而文件扩展名和文档里到处叫 `.vab`。三者指同一个东西。

---

## 1. 文件布局

```
┌──────────────────────────┐ 偏移 0
│  magic  "VATF"   (4 B)   │
├──────────────────────────┤ 偏移 4
│  version : u32           │  Header
│  length  : u32           │  （8 B，原生小端）
├──────────────────────────┤ 偏移 12
│  chunk 0                 │
│  chunk 1                 │  连续排布，无填充、无对齐
│  …                       │
│  chunk 9                 │
└──────────────────────────┘ 偏移 = header.length
```

`Header.length` 是**整个文件的字节数**，包含 magic 自身的 4 字节：

```rust
// src/lib.rs:526-529
let file_header = Header {
    version: VAB_VERSION,
    length: MAGIC_BYTES.len() as u32 + mem::size_of::<Header>() as u32 + payload_size,
};
```

读取端对此做**强校验**（`src/reader.rs:88-94`）：

```rust
if header.length as usize != bytes.len() {
    bail!("VAB length mismatch: header says {} B but file is {} B", header.length, bytes.len());
}
```

这条约束有两个直接后果：

- **不允许尾部垃圾**。文件必须精确结束在最后一个 chunk 的末尾。
- **不能流式读取**。既然长度必须等于实际可用字节数，`VabReader::open` 只能 `std::fs::read` 整个文件（`src/reader.rs:62-66`），无法只读文件头。

---

## 2. Chunk 布局

```
┌──────────────────────────┐
│  chunk_type : [u8;4]     │  FourCC
│  length     : u32        │  payload 字节数（不含这 8 字节头）
├──────────────────────────┤
│  payload  … (length B)   │
└──────────────────────────┘
紧接着就是下一个 chunk 的 FourCC —— 没有填充字节
```

`CHUNK_HEADER_SIZE = size_of::<ChunkHeader>() = 8`（`src/lib.rs:60`）。

**写入顺序固定**（`src/lib.rs:505-516`），但读取端用 `HashMap<[u8;4], AlignedChunk>` 存储，**顺序无关**：

| # | FourCC | 内容 | 编码 | 说明 |
|---|---|---|---|---|
| 0 | `BAKD` | `BakedMovie` | bincode | **必需**；运行时的绘制树 + 帧率 |
| 1 | `SHAP` | `[ShapeRecord]` | POD | shape id → 网格区间查找表 |
| 2 | `SHME` | `[ShapeMesh]` | POD | 每个 draw call 的几何元数据 |
| 3 | `GRAD` | `[GradientUniforms]` | POD | 渐变着色器 uniform |
| 4 | `BMAP` | `[[f32;6]]` | POD | 位图纹理变换 |
| 5 | `TEXT` | 原始字节 | 无 | 交错的 WebP 纹理池 |
| 6 | `VERT` | `[Vertex]` | POD | 量化顶点 |
| 7 | `INDX` | `[u32]` | POD | 三角形索引 |
| 8 | `MORP` | `[MorphEntry]` | POD | morph 预插值网格表 |

写端**总是写出全部 9 个 chunk**，即使内容为空——`round_trip_empty_file`（`src/reader.rs`）断言了这一点，并额外断言 `ANIM` **不**存在。

> **历史注记**：早期版本有第 10 个 chunk `ANIM`（`AnimContainer`，未展开的 per-sprite 显示列表），与 `BAKD` 并存。它占动画密集型产物 **17%–54%** 的体积，而唯一只有它才有的数据是 `frame_rate` —— 一个 f32。已删除，帧率移入 `BakedMovie::frame_rate`（[07 篇 §8](07-design-notes.md)）。

**重复 FourCC 是硬错误**（`src/reader.rs:122-124`）：

```rust
if chunks.contains_key(&chunk_header.chunk_type) {
    bail!("duplicate chunk {:?}", chunk_header.chunk_type);
}
```

因为存储用的是以 FourCC 为键的 map，重复键若不报错就会静默地"后者覆盖前者"。

---

## 3. POD 结构体字段表

所有几何 chunk 的元素都是 `#[repr(C)] + bytemuck::Pod` 结构体，**原生小端字节序**。大端主机会在文件一打开时就被拒绝（`src/reader.rs:70-72`，写端同样有 `src/lib.rs:479-481` 的守卫）。

### `Header` — 8 B

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | `u32` | `version` | 必须等于 `3` |
| 4 | `u32` | `length` | 整个文件长度 |

### `ChunkHeader` — 8 B

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | `[u8;4]` | `chunk_type` |
| 4 | `u32` | `length` |

### `Color` — 4 B

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | `u8` | `r` |
| 1 | `u8` | `g` |
| 2 | `u8` | `b` |
| 3 | `u8` | `a` |

### `Vertex` — 8 B

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | `i16` | `x` | **量化坐标**，见 §4 |
| 2 | `i16` | `y` | |
| 4 | `Color` | `color` | 仅 `material::COLOR` 时有意义 |

### `ShapeRecord` — 8 B

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | `u16` | `id` | SWF character id |
| 2 | `u16` | `sub_shape_count` | 该 shape 产生的 `ShapeMesh` 个数 |
| 4 | `u32` | `sub_shape_offset` | 在 `SHME` 中的起始下标 |

一个 SWF shape 会摊平成**多个** mesh（每个 draw call 一个），所以这是一段区间 `[offset, offset + count)`。

### `ShapeMesh` — 48 B

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | `u32` | `vertex_count` | |
| 4 | `u32` | `vertex_offset` | 在 `VERT` 中的起始下标 |
| 8 | `u32` | `index_count` | 是 3 的倍数 |
| 12 | `u32` | `index_offset` | 在 `INDX` 中的起始下标 |
| 16 | `u16` | `material_type` | `0=COLOR` `1=GRADIENT` `2=BITMAP` |
| 18 | `u16` | `sampler_flags` | bit0 = smoothed，bit1 = repeating |
| 20 | `u32` | `texture_offset` | 在 `TEXT` 中的字节偏移 |
| 24 | `u32` | `texture_length` | 字节长度 |
| 28 | `u32` | `material_offset` | 在 `GRAD`（渐变）或 `BMAP`（位图）中的下标 |
| 32 | `f32` | `bounds_half_x` | 反量化参数，见 §4 |
| 36 | `f32` | `bounds_half_y` | |
| 40 | `f32` | `bounds_center_x` | |
| 44 | `f32` | `bounds_center_y` | |

> 结构体大小是按 `#[repr(C)]` 逐字段推导的（最大对齐 4，无填充）。这里还有一个**可自行验证的旁证**：这些结构体都 `derive(bytemuck::Pod)`，而 `Pod` 的 derive **禁止任何填充字节**——所以只要代码能编译，就说明布局里确实没有 padding，大小必然等于字段大小之和（48 能被对齐 4 整除，自洽）。如需机器确认，加一个 `assert_eq!(size_of::<ShapeMesh>(), 48)` 即可。

### `MorphEntry` — 24 B

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | `u16` | `morph_id` |
| 2 | `u16` | `ratio` |
| 4 | `u32` | `_pad`（显式填充，见下） |
| 8 | `u32` | `vertex_offset` |
| 12 | `u32` | `vertex_count` |
| 16 | `u32` | `index_offset` |
| 20 | `u32` | `index_count` |

`_pad` 是 `#[repr(C)]` 下本来就会有的对齐填充（两个 `u16` 之后要放 `u32`），这里**显式写出来**，好处是结构体定义本身就说明了布局、且 `Pod` 的 derive 不会因为"存在隐式填充字节"而变得微妙。

### `GradientUniforms` — 40 B

| 偏移 | 类型 | 字段 | 说明 |
|---|---|---|---|
| 0 | `f32` | `focal_point` | 已钳位到 `±0.98` |
| 4 | `i32` | `interpolation` | `0`=原生 sRGB，`1`=LinearRgb |
| 8 | `i32` | `shape` | `1`=Linear `2`=Radial `3`=Focal |
| 12 | `i32` | `repeat` | `1`=Pad `2`=Reflect `3`=Repeat |
| 16 | `[f32;6]` | `texture_transform` | 纹理坐标仿射 |

着色器枚举值刻意与 SWF 的原始判别值**不同**：

```rust
// src/lib.rs:157-175
shape: match gradient.gradient_type {
    GradientType::Linear => 1, GradientType::Radial => 2, GradientType::Focal => 3,
},
repeat: match gradient.repeat_mode {
    GradientSpread::Pad => 1, GradientSpread::Reflect => 2, GradientSpread::Repeat => 3,
},
focal_point: gradient.focal_point.to_f32().clamp(-0.98, 0.98),
```

- `shape` / `repeat` 从 **1** 开始编号。这通常是给 shader 里的 `switch` 用的——0 保留给"未初始化/非法"。
- `focal_point` 钳到 `±0.98` 是为了避开 1.0 处的奇点（焦点圆半径趋零 → 着色器里除以零）。

---

## 4. 顶点量化

顶点只存 `i16` —— 每顶点 8 字节（4 字节位置 + 4 字节颜色），比 `f32` 位置省一半。

### 编码（`src/lib.rs:185-206`）

```rust
fn quantize_vertex(x: f32, y: f32, bounds_min: (f32, f32), bounds_max: (f32, f32)) -> (i16, i16) {
    let center_x = (bounds_max.0 + bounds_min.0) * 0.5;
    let half_x   = (bounds_max.0 - bounds_min.0) * 0.5;
    // …y 同理

    let q_x = if half_x > 0.0 { ((x - center_x) / half_x) * 32767.0 } else { 0.0 };
    let q_y = if half_y > 0.0 { ((y - center_y) / half_y) * 32767.0 } else { 0.0 };

    (q_x.round().clamp(-32767.0, 32767.0) as i16,
     q_y.round().clamp(-32767.0, 32767.0) as i16)
}
```

即：把顶点映射到 shape 自身包围盒的 `[-1, 1]` 归一化空间，再乘 32767 取整。

### 解码（运行时，`src/lib.rs:117-118` 的注释）

```
local_x = q_x / 32767 * bounds_half_x + bounds_center_x
local_y = q_y / 32767 * bounds_half_y + bounds_center_y
```

反量化所需的 4 个 `f32` 参数**直接存在 `ShapeMesh` 里**（偏移 32..48），运行时不需要任何额外查表或全局上下文——拿到一个 `ShapeMesh` 就能独立解出它的顶点。

### 为什么这样设计

- **为什么是 32767 而不是 32768**：`i16` 的范围是 `[-32768, 32767]`。用 32767 作系数，`-q` 和 `+q` 都落在合法范围内且**对称**，不会因为 `-(-32768)` 溢出而翻车。代价是相对精度上限 `0.5 / 32767 ≈ 1.5e-5`（半宽的一个万分之一点五），远低于像素精度需求。
- **为什么退化轴返回 0**：单点、纯水平/垂直线等形状的某个轴半宽为 0。此时除法会得到 `inf`/`NaN`，`clamp` 也救不回来。直接返回 0 表示"该轴上所有点都在中心"。
- **`clamp` 的代价**：落在包围盒之外的几何会被**钳死**在 `±32767` 上并永久丢失信息。所以包围盒必须取对——见下一条。
- **必须用 `edge_bounds` 而不是 `shape_bounds`**（`src/lib.rs:311-313` 的注释）：

  > *"Quantise against `edge_bounds` (which includes stroke widths) rather than `shape_bounds` (fill outline only) — otherwise vertices produced by stroke expansion get clamped to ±32767 and thick strokes flatten."*

  描边三角化会向外扩张出半个笔宽。若用只含填充轮廓的 `shape_bounds` 做归一化，这些外扩顶点就会全部撞上 `±32767` 被压平，粗描边的形状直接变形。这是一个**很容易踩、且症状隐蔽**的坑。

---

## 5. 材质与纹理

### 三种材质

```rust
pub mod material {
    pub const COLOR: u16 = 0;
    pub const GRADIENT: u16 = 1;
    pub const BITMAP: u16 = 2;
}
```

调度规则：

| `material_type` | 取 uniform | 取纹理 |
|---|---|---|
| `COLOR` | 无 | 无（直接用 `Vertex.color`） |
| `GRADIENT` | `GRAD[material_offset]` | `TEXT[texture_offset..+texture_length]`（256×1 WebP 斜坡） |
| `BITMAP` | `BMAP[material_offset]`（`[f32;6]`） | `TEXT[texture_offset..+texture_length]`（WebP 位图） |

### `sampler_flags` 位域

```rust
sampler_flags: u16::from(bm.is_smoothed) | (u16::from(bm.is_repeating) << 1)
```

| 位 | 含义 |
|---|---|
| 0 | `is_smoothed`（线性过滤） |
| 1 | `is_repeating`（`Repeat` 寻址；否则 `Clamp`） |

### TEXT 是交错池，不是一个纹理

`TEXT` 里按顺序塞着所有纹理的字节，**渐变斜坡和位图混在一起**，靠 `ShapeMesh` 的 `texture_offset` / `texture_length` 切片。写端按**字节内容**去重（`VatfBuilder::intern_texture`，`src/lib.rs:293-301`）：

```rust
fn intern_texture(&mut self, bytes: &[u8]) -> (u32, u32) {
    if let Some(entry) = self.texture_lookup.get(bytes) { return *entry; }
    let entry = (self.texture.len() as u32, bytes.len() as u32);
    self.texture.extend_from_slice(bytes);
    self.texture_lookup.insert(bytes.to_vec(), entry);
    entry
}
```

所以两个 shape 用同一个位图、或两个 draw 用同一条渐变斜坡，只会在 `TEXT` 里存一份。

### 纹理统一是 WebP

不管来源是 SWF 位图还是渐变斜坡，最终都编码成 **WebP**（`src/lib.rs:430-434` / `556-562`）。这样做的好处是运行时只需要一条纹理解码路径；代价是**有损**（WebP 默认是有损压缩）。

---

## 6. `MORP` 表与 morph 查表

morph 形状（SWF 里同一角色在两个形状间变形）在转换期就按 ratio 插值 + 镶嵌好了，运行时**不做任何插值**，只查表。

```rust
// src/lib.rs:896-907
for mi in mesh_start..builder.shape_meshes.len() as u32 {
    let mesh = &builder.shape_meshes[mi as usize];
    builder.morph_entries.push(MorphEntry {
        morph_id, ratio, _pad: 0,
        vertex_offset: mesh.vertex_offset, vertex_count: mesh.vertex_count,
        index_offset: mesh.index_offset, index_count: mesh.index_count,
    });
}
```

两个必须记住的性质：

1. **同一个 `(morph_id, ratio)` 会有多行**。上面是"每个产生的 mesh 记一行"，而一个 morph shape 完全可能产生多个 draw call（比如同时有填充和描边）。所以**不能**用扁平切片取第一个匹配项——那只对单 draw 的 morph 正确。正确做法是两级 map：
   ```rust
   // src/reader.rs:255-262 的文档注释推荐
   FnvHashMap<u16, FnvHashMap<u16, &MorphEntry>>
   ```
2. **只有"真实出现过"的 ratio 才存在**。写端先扫一遍时间轴收集唯一的 `(morph_id, ratio)` 对再处理（`src/lib.rs:861-880`），没出现过的 ratio 在表里查不到。运行时只能**吸附**到最近的已烘焙 ratio，不能自由插值。

### 怎么区分 morph 和普通 shape

`MorphEntry` 只给顶点/索引区间，**不带 `material_type`**——材质要从对应的 `ShapeMesh` 拿（按 `vertex_offset` 反查，或干脆约定 morph 只产生一种材质）。

判别规则：拿到一个 `Shape { id, ratio }`，**先查 `MORP` 里有没有 `id`**；有就按 `(id, ratio)` 查 MORP，没有才回落到 `SHAP` 查普通 shape。

---

## 7. BAKD 的编码细节

`BAKD` **不是 POD**，是 `bincode` 序列化。

### 必须是 fixint 编码

写端用的是 `bincode::serialize`（`src/lib.rs`），读端显式指定（`src/reader.rs:133-138`）：

```rust
bincode::DefaultOptions::new()
    .with_fixint_encoding()
    .with_limit(bytes.len() as u64)
    .reject_trailing_bytes()
    .deserialize::<BakedMovie>(baked_data.as_bytes())
```

> ⚠️ **这是一处隐蔽的耦合。** `bincode 1.3` 里 `DefaultOptions::new()` 默认是 **varint**，而顶层函数 `bincode::serialize` 明确强制了 **fixint**（`bincode-1.3.3/src/lib.rs`：`DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().serialize(value)`）。两端因此才碰巧对得上。
>
> 如果谁把写端改成 `DefaultOptions::new().serialize(...)`，读端会**静默解析出错误结果**（不一定报错）。改这边必须两边一起改。

读端在两处比写端更严格：

- `with_limit(bytes.len() as u64)` —— 反序列化炸弹防护。恶意长度前缀最多只能要求分配**整个文件大小**的内存量。注意限额用的是**文件总长**而不是该 chunk 的长度，所以粒度偏松（见 07 篇）。
- `reject_trailing_bytes()` —— chunk 必须**恰好**解码完，多一字节都算损坏。

### BAKD 是必需的，且读取时校验

```rust
// src/reader.rs
let baked_data = chunks.get(b"BAKD").context("VAB missing BAKD chunk")?;
let baked = bincode::DefaultOptions::new()…deserialize(baked_data.as_bytes())
    .context("Corrupt BAKD chunk")?;
baked.validate()?;
```

`validate()` 在**写之前**（`src/lib.rs`）和**读之后**都跑。这意味着手工构造一个违反不变量的 `BAKD` 是**读不进来的**——校验器是加载器的一部分。

### `BakedMovie` 的字段

```rust
pub struct BakedMovie {
    pub frame_rate: f32,           // 源 SWF 的帧率
    pub clips: Vec<BakedClip>,
    pub skins: Vec<BakedSkin>,
}
```

`frame_rate` 由烘焙器从 `AnimContainer` 带过来。全文件只有一个帧率——SWF 的帧率在 movie header 上，`DefineSprite` 没有自己的帧率，子 sprite 每父帧推进一帧。空 movie（无根时间轴／根为空）的 `frame_rate` 是 `0.0`，此时也没有 clip 可播。

---

## 8. 版本与兼容性

| 机制 | 位置 | 行为 |
|---|---|---|
| magic 校验 | `src/reader.rs:77-79` | 不等于 `b"VATF"` 直接 bail |
| 版本校验 | `src/reader.rs:82-87` | 必须**精确等于** `VAB_VERSION`（当前 `1`） |
| 长度校验 | `src/reader.rs:88-94` | `header.length` 必须等于实际文件长度 |
| 大端守卫 | `src/reader.rs:70-72`、`src/lib.rs:479-481` | 大端主机直接拒绝 |
| 重复 chunk | `src/reader.rs:122-124` | 报错 |
| 截断保护 | `src/reader.rs:110-120` | `checked_add` 防 `u32` 溢出，越界报错 |
| 长度乘积校验 | `src/reader.rs:197-202` | 见下 |

### 版本策略：编号 = 布局世代，不是兼容性策略

`version` 与 `VAB_VERSION` 不相等就一律拒绝，**没有**"读旧版本"或"向前兼容"的路径。这个常量是 [01 篇 §1](01-format.md#1-文件布局) 里那个 `VAB_VERSION`：

```rust
// src/lib.rs
/// 这个常量是**陈旧文件探测器，不是兼容性策略**。
/// 只要 `BAKD` 的 schema 或某个 POD 结构体的布局变了，就把它加一。
pub const VAB_VERSION: u32 = 1;
```

要点：

- 格式**从未发布**，所以编号直接重置为 `1`，不必续接历史。校验是精确相等，编号本身不承载语义。
- **它的职责是"让陈旧文件报出清楚的错"**，而不是维护兼容。去掉它并不会让你免于重新转换——布局真变了的时候旧文件的字节就是错的，无论如何都得重转；校验只是决定这件事是"报一句 `Unsupported VAB version 3 (this build expects 1)`"还是"读出一堆垃圾"。
- 改布局时把常量加一即可，成本是一行。**别忘了重新生成所有产物**。

### POD 元素的隐式长度校验

`cast_pod_slice`（`src/reader.rs:197-202`）**没有**显式检查 payload 长度：

```rust
fn cast_pod_slice<T: bytemuck::Pod>(data: &[u8]) -> Option<&[T]> {
    if data.is_empty() { return Some(&[]); }
    bytemuck::try_cast_slice(data).ok()
}
```

`try_cast_slice` 本身就是校验：payload 长度不是 `size_of::<T>()` 整数倍时返回 `Err` → `None`。所以这里"**失败即拒绝**"，而且失败方向是安全的（返回 `None` 而不是产生 UB）。

两点语义要留意：

- **空 chunk → `Some(&[])`，缺失 chunk → `None`**。要区分二者得用 `has_chunk`。
- **"缺失"与"格式非法"都会表现成 `None`**。调用方无法从返回值区分"这个 chunk 没写"和"这个 chunk 写坏了"。

---

## 9. 为什么用 `#[repr(C)]` + `Pod`

几何数据（顶点、索引、mesh 元数据）在文件里和在内存里是**同一套字节**，解析时不需要逐个字段读取——只要把字节切片 `cast` 成 `&[T]` 就行。这是相当可观的性能优势：一个几万顶点的模型，解析代价接近零。

代价是两条硬约束：

1. **字节序被钉死**（原生小端），因此需要大端守卫。
2. **对齐要求**。`bytemuck` 的 `cast_slice` 要求指针满足 `T` 的对齐，而文件里的 payload 是**紧凑排布、无填充**的——直接 `Vec<u8>` 承载时对齐取决于分配器，会**间歇性**失败。读取端的 `AlignedChunk` 就是为解决这个问题而存在的，详见 [06-runtime.md](06-runtime.md#2-alignedchunk为什么不是真零拷贝)。
