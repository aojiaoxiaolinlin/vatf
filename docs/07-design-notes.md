# 07 · 关键设计决策、已知问题、未接线代码

本篇是前六篇的收束：把散落在各处的"为什么"集中起来，并如实记录通读代码时核实过的缺陷与死代码。

> **本篇只记录，不修复。** 所有"已知问题"都是核实过的现状，改动它们需要单独的决策。

---

## 1. 核心权衡：把求值全部前移

整个项目可以用一句话概括：

> **把 Flash 播放器的运行时求值，全部搬到离线转换阶段。**

| 原本运行时要做的事 | 现在的去处 |
|---|---|
| `PlaceObject` 逐帧累积成显示列表 | 转换期算好，每帧存完整列表 |
| 嵌套 sprite 各跑各的时间轴 | 烘焙期按每个根帧展开成子树 |
| 层级变换逐层相乘 | 烘焙期乘成世界变换 |
| 形状路径 → 三角形 | 转换期镶嵌好 |
| morph 按 ratio 插值 | 转换期按出现过的 ratio 预烘焙 |
| 渐变/位图矩阵 → 纹理坐标 | 转换期算成 uniform |
| 滤镜离屏尺寸估算 | 提供 `filter_dest_rect` 纯函数 |

### 换到了什么

- **运行时极简**：取一帧的绘制列表就是 `clip.frames[playhead]`，不需要变换栈、不需要 per-instance 时间轴状态、不需要"应用差分"的逻辑。
- **体积控制**：顶点量化成 `i16`；纹理按字节 intern；渐变**斜坡**跨 draw 共享一张 256×1 纹理；同一 morph ratio 只烘一次。
- **可复现**：`BTreeMap` / 排序的 `Vec` / 确定的遍历顺序保证了同样输入产生同样字节（见 §5）。

### 付出了什么

- **文件更大**。每帧存**完整**列表，没有帧间差分；索引用 `u32` 而非按需 `u16`。
- **转换慢**。每个 shape 重新建 `ShapeTessellator`；每个唯一纹理一次 WebP 编码。
- **不可增量更新**。改了源 SWF 只能整体重转。
- **烘焙期的展开可能爆炸**。所以才有 1000 万节点预算和深度 128 上限。

对"从 SWF 迁移游戏素材"这个用例，这个交换是划算的：转换是一次性的，播放是长期的。

---

## 2. 逐条"为什么"

### 2.1 顶点为什么量化到 ±32767 而不是 ±32768

`i16` 的有效范围是 `[-32768, 32767]`。用 32767：

- `-q` 和 `+q` 都合法且**对称**；
- 避免 `-(-32768)` 溢出。

代价是相对精度 `0.5/32767 ≈ 1.5e-5`（半宽的一个万分之一点五）。远低于像素精度需求，几乎免费。详见 [01 篇 §4](01-format.md#4-顶点量化)。

### 2.2 为什么量化用 `edge_bounds` 而不是 `shape_bounds`

描边三角化会向外扩张**半个笔宽**。`shape_bounds` 只含填充轮廓，不含这个扩张量。若用它做归一化，所有描边外扩顶点都会撞上 `±32767` 被 `clamp` 压平——**粗描边的形状会明显变形**。

源码注释（`lib.rs:311-313`）专门写了这一点，说明这是个踩过的坑。代价是填充区域的精度略微下降（包围盒变大了），但这是正确的取舍。

**这条约束一路传导到 morph**：`morph.rs:126` 必须 **lerp 源包围盒**而不是从插值后的记录重算——因为插值记录不携带描边半宽。见 [03 篇 §9.4](03-geometry.md#94-edge_bounds-必须插值源包围盒)。

### 2.3 morph 网格为什么不写 `ShapeRecord`

```rust
// lib.rs:892-894
// Morph meshes are addressed through MORP, so they must not emit a
// ShapeRecord (which would pollute `shape_map`, notably key 0).
```

两个理由叠加：

1. morph 网格是通过 `MORP` 表寻址的，不需要 `SHAP` 索引；
2. **`morph::interpolate` 产出的 shape 的 `id` 是 0**（`morph.rs:130`），而 0 是 `shape_map` 里的一个合法键。如果写进去，会污染"id = 0 的普通形状"的查找。

代价是运行时无法只靠 `SHAP` 判断一个 id 是不是 morph，必须查 `MORP`（[01 篇 §6](01-format.md#6-morp-表与-morph-查表)）。

### 2.4 skin 变体为什么必须 `frozen`

如果没有 `frozen`，skin 变体内部的嵌套 sprite 会按**当前播放帧**取样。而变体是**跨所有帧共享**的一份数据（`baked.rs:256` 的 `if !self.skins.contains_key(...)` 保证只烘一次）——两者矛盾。

`frozen` 的做法是：变体内部的嵌套 sprite **一律固定在第 0 帧**（`baked.rs:309-313`），使变体成为与播放头无关的**静态快照**。

否则每个变体都要对每个根帧展开一份，代价是 `变体数 × 帧数` 的笛卡尔积。

### 2.5 为什么曾经保留 `ANIM`，以及为什么最终删掉它

早期版本同时存 `ANIM`（未展开的 per-sprite 逐帧显示列表）与 `BAKD`（已展开的绘制树）。保留 ANIM 的理由看起来有两个：

1. **帧率只存在于 ANIM**。`BakedMovie` 完全没有定时信息。
2. **ANIM 是差分测试的基准**。oracle 测试按 SWF 语义独立推导显示列表来比对 ANIM（[04 篇 §5](04-animation.md#5-差分测试oracle)）——BAKD 是展开后的产物，不适合这种逐帧差分。

**但这两条都经不起推敲：**

- 理由 1 暴露的是**设计缺陷**而不是理由。SWF 的帧率在 movie header 上，`DefineSprite` 根本没有帧率字段——子 sprite 就是每父帧推进一帧。所以整个格式只有**一个** f32 的帧率。为它保留一个占 **17%–54%** 体积的 chunk 是荒谬的。现已移入 `BakedMovie::frame_rate`。
- 理由 2 是**测试的便利性**，不是格式的需求。容器可以从进程内拿到（`parse_animation_container`），不必为它付落盘成本。

实测收益（工作区全部 7 个产物）：

| 文件 | 大小 | ANIM 占比 |
|---|---|---|
| `yue_se.vab` | 6.0 MB | **54.2%** |
| `spirit3021src.vab` | 9.2 MB | **23.1%** |
| `spirit2159src.vab` | 2.6 MB | **19.5%** |
| `spirit2954src.vab` | 14.3 MB | **17.1%** |
| `hanghai.vab` / `wu_kong.vab` / `attack.vab` | — | 0.2%–0.5%（TEXT 或几何主导） |

`yue_se.vab` 删掉 ANIM 后直接**减半**。7 个产物的 ANIM 实测字节数合计 **8,516,351 B ≈ 8.5 MB**（其中 yue_se 一个就占 3.2 MB）。

> 收益和文件大小**不成正比**：`attack.vab` 是最大的产物（67.6 MB），但因为 97% 是 TEXT，它的 ANIM 只有 135 KB；反过来 `yue_se.vab` 只有 6 MB，ANIM 却占 3.2 MB。**决定因素是动画数据的密度，不是产物体积。**

**这条经验值得记住**：当一个"为了 X 而保留"的结构只在极少数场合才真正需要 X 时，先问 X 本身该不该在那里——这个案例里，正确答案是把帧率搬到它该在的地方，而不是给 chunk 找更多理由。

### 2.6 遮罩为什么不允许交叉区间

```rust
// baked.rs:212-218
// Crossing mask intervals need an explicit stencil-stack representation.
ensure!(objects[index + 1..end].iter().all(|o| o.clip_depth <= object.clip_depth),
        "crossing mask ranges are unsupported");
```

格式选择了 `Mask { mask, children }` 这种**扁平的、一层**的表示。这换来的是运行时的**无栈 stencil**（[06 篇 §6](06-runtime.md#6-mask-的渲染语义)）。

如果要支持交叉区间，运行时就得维护一个真正的 stencil 深度栈，整个渲染循环的复杂度会上一个台阶。格式在这里明确地"砍掉一个特性换简单"。

### 2.7 写端为什么**不再**"序列化后立刻反序列化"

早期 `write_vatf` 会把动画数据序列化成 `ANIM` 的字节，再立刻 `deserialize` 回来喂给烘焙器。当时的理由是"让 ANIM 与 BAKD 由构造保证同源，并顺带回归测试 bincode 编解码路径"。

`ANIM` 删除后这两个理由都消失了，而每次转换还要为 0.5–2.4 MB 的动画数据白付一次完整序列化 + 解析。现在直接调 `AnimContainer::from_parts`（[02 篇 §6](02-pipeline.md#6-write_vatf-的组装)）。

> 顺带一个观察：那个往返其实**并不能**失败——它解析的字节正是同一个进程刚刚用同一个类型序列化出来的。所以它作为"同源保证"的价值本来就有限，真正有价值的部分（`from_parts` 的排序归一化）被保留了下来。

### 2.8 为什么 `Group` 没有自己的变换

`BakedNode::Group { children, filters, blend_mode }` 不带 `transform`。因为它的子节点**已经各自带好了世界变换**——烘焙期的 `compose` 是沿着整棵树累积的（[04 篇 §3.3](04-animation.md#33-变换在烘焙期就乘完)）。再加一层变换只会重复。

同理 `Mask` 也不带。

### 2.9 为什么 `num_passes` 和 `flags` 冗余存储

`AnimFilter` 里 `num_passes` 是**解码好的趟数**，`flags` 是**原始位域**，两者都有。因为运行时两边都要用：

- `num_passes` 用于离屏尺寸计算（`filter_dest_rect` 里的 `PASS_SCALES` 索引）；
- `flags` 用于 `knockout` / `inner` / `on_top` 这些渲染开关。

代价是消费者必须知道各滤镜的 PASSES 位域位置**不一样**（Blur 在 bit 3–7，其余在 bit 0 起）——见 [05 篇 §7](05-assets.md#7-滤镜的两套类型)。

### 2.10 为什么 `filter_dest_rect` 要重写一遍

为了让下游渲染器**在不链接 `swf` crate** 的前提下算离屏纹理尺寸。代价是复制了上游的数学，所以配了 `filter_dest_rect_matches_swf_crate` 差分测试来防止走样。

这是"运行时独立于转换器"这个目标的具体体现——和 `AnimFilter` 全基元化（[04 篇 §1](04-animation.md#1-两代数据模型)）是同一个动机。

### 2.11 为什么纹理统一成 WebP

一条解码路径 vs 四条（JPEG / PNG / GIF / zlib+调色板）。代价是**有损**——位图经历二次损失、渐变斜坡可能出色带。

### 2.12 为什么渐变斜坡做成纹理而不是色标数组

256 像素的小纹理，换着色器里**零循环的查表**。对 GPU 划算。而且斜坡去重（不含矩阵）让跨 draw 共享变得自然——见 [03 篇 §8.1](03-geometry.md#81-去重的粒度斜坡全局共享矩阵每-draw-一份)。

---

## 3. 与 Ruffle / swf_player 的关系

相当一部分代码是 vendored 改写的。想深挖或对照上游，可以从这些线索入手：

| 文件 | 来源 | 证据 |
|---|---|---|
| `decoder.rs` | Ruffle 的 `decoder.rs` + `bitmap/ruffle_decoder.rs` | 注释引用 `ruffle-rs/ruffle#8775`、`#1191`、`#6893` |
| `decoder/bt601.rs` | Ruffle 的 YUV 转换 | 注释致谢 Lokathor，测试注释风格一致 |
| `shape_utils.rs` | Ruffle 的 `shape_utils.rs` | `DistilledShape` / `DrawPath` / `ActivePath` 是上游的类型名；`RuffleVertexCtor` 直接以 Ruffle 命名 |
| `morph.rs` | **swf_player** 的 `morph_shape.rs` | 模块头注释明写 |
| `matrix.rs` / `transform.rs` | Ruffle | `round_to_i32` 的 Flash 语义注释是上游原文 |
| `tessellator.rs` | Ruffle 的 tessellator（改成单 crate 版） | `ruffle_path_to_lyon_path`、`RuffleVertexCtor` |
| `filter.rs` | Ruffle 的滤镜封装 | `calculate_dest_rect` / `impotent` 同名 |

判断"哪些是上游遗留、哪些是本项目设计"的一个实用线索：**带 `#[allow(unused)]` / `#[expect(dead_code)]` / `pub` 但没有调用者的东西，多半是 vendored 时带过来的**。

---

## 4. 未接线 / 死代码清单

读代码时这些**很容易被误认为是主路径**。逐个核实过：

| 项 | 位置 | 状态 |
|---|---|---|
| `Draw::mask_index_count` | `tessellator.rs:258-259` | **`#[expect(dead_code)]`**，字段被仔细计算但无人读取 |
| 连带的 `assert!(self.mask_index_count.is_none())` | `tessellator.rs:155` | 只服务于上面这条废弃路径 |
| `TransformStack` | `transform.rs:13-45` | **全仓无引用**（只在自己文件内定义 + `Default`） |
| `BitmapFormat::Yuv420p` / `Yuva420p` | `decoder.rs:131,135` | 都标了 `#[allow(unused)]`；**没有任何解码器会产生它们** |
| `Bitmap::into_rgba` 的 YUV 分支 | `decoder.rs:55-82` | 因上一条而**不可达** |
| **`decoder/bt601.rs` 整个模块** | 483 行 | **只被自己的测试触达** |
| `Gradient::compute_gradient_color` 的 `convert` 闭包 | `tessellator.rs:286-290` | 两个分支**完全相同**（恒等函数），`LinearRgb` 实际交给着色器 |
| `Filter::scale` | `filter.rs:18` | 无调用者 |
| `Filter::calculate_dest_rect` | `filter.rs:30` | 无调用者（实际用的是 `filter_dest_rect`） |
| `Filter::impotent` | `filter.rs:42` | 无调用者，且注释留着 `// TODO: There's more cases here, find them!` |
| `AnimFilter::scale` | `animation.rs:160` | 无调用者 |
| `AnimFilter::impotent` | `animation.rs:172` | 无调用者 |
| `DistilledShape::shape_bounds` / `edge_bounds` / `id` | `shape_utils.rs:96-98` | 只在 `From<&swf::Shape>` 里赋值，`tessellate_shape` 只读 `.paths` |
| `VabReader::into_parts` / `into_animations` | `reader.rs:165,246` | 无调用者（对外 API） |
| **`src/decoder/utils.rs`** | —— | **空文件（2 字节 `\r\n`），未被声明为模块，但已被 git 跟踪** |
| `BakedMovie::Default` | `baked.rs:9` | 用作"无根时间轴"的返回值（**在用**） |
| `bake()`（非 skin 版） | `baked.rs:72` | 只被 `baked.rs` 的测试使用，对外 API |

大部分（滤镜的 `scale` / `impotent`、`into_parts`）是**有意的运行时 API 面**——它们的存在本身在提示"渲染器应该由外部实现"。但 `bt601.rs` 整块、YUV 分支、`decoder/utils.rs` 属于**没有收益的负担**，删掉不会影响任何现有功能。

---

## 5. 已核实的问题清单

按"值得关注的顺序"排列。所有条目都已回源码核对。

### 5.1 `quadratic_curve_bounds` 的起点传参错误

**位置**：`shape_utils.rs:51-65`

```rust
cursor += *control_delta;
let control = cursor;
cursor += *anchor_delta;
let anchor = cursor;
bounds = bounds.union(&quadratic_curve_bounds(
    cursor,          // ← 此时 cursor 已经是 anchor，不是曲线起点
    Twips::ZERO,
    control,
    anchor,
));
```

**后果**：求的是 `anchor → control → anchor` 这条退化曲线的包围盒，真实曲线的极值点可能落在盒子外 → **包围盒可能被低估**。

**当前影响有限**：该包围盒只被 `morph.rs:123` 用作插值后的 `shape_bounds`，而量化落盘走 `edge_bounds`。但如果将来用它做视锥剔除，会变成真的渲染 bug。

**附带**：`stroke_width` 参数在唯一调用点恒传 `Twips::ZERO`，形同虚设（Ruffle 原版会传真实笔宽）。

### 5.2 `Matrix::MulAssign` 与 `Mul` 的加法不一致

**位置**：`matrix.rs:179-180`（`Mul`，用 `wrapping_add`）vs `matrix.rs:248-249`（`MulAssign`，用普通 `+`）

两份数学完全相同，但 `MulAssign` 在 twips 溢出时会 **debug 构建 panic**，而 `Mul` 静默回绕。触发需要坐标累积到 ±2³¹ twips（约 ±10⁸ 像素），现实中不太可能——但这是模块里唯一的算术 panic 隐患，且纯粹由不一致引入。

### 5.3 `swf_to_gl_matrix` 无奇异矩阵防护

**位置**：`tessellator.rs:400-421`（`swf_bitmap_to_gl_matrix` 同样）

`det` 为 0 时产生 `inf` / `NaN` 矩阵。对比：`matrix.rs` 的 `Matrix::inverse` 是**有** `|det| > f32::EPSILON` 检查并返回 `Option` 的——这里没有复用它。

零缩放的填充矩阵在真实 SWF 里少见，但并非不可能。

### 5.4 `flatten_matrix_3x3_to_6` 的注释与实现不一致

**位置**：`lib.rs:564-568`

注释说结果是 `[a, c, tx, b, d, ty]`，但实现是**行主序展平**：

```rust
[m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1]]
```

按 §[03 篇 §8.3](03-geometry.md#83-渐变矩阵--f3233) 的推导，实际得到的 6 个值是 `[a', b', c', d', tx', ty']`（逆矩阵的线性部分按 SWF 的 `a,b,c,d` 字段序，加上逆平移）。

**本仓库没有着色器代码，无法在这里判定哪一种才是消费者期望的**。写渲染器时以实测为准。这是格式文档里最需要小心的一处。

### 5.5 `round_to_i32` 的文档注释与实现不符

**位置**：`matrix.rs:288-304`

注释说"把越界值和 NaN 都钳到 `i32::MIN`"，实现里 NaN/±∞ 走 `else` 分支返回 **0**。只有"有限且 ≥ 2³¹"才返回 `i32::MIN`。见 [04 篇 §6](04-animation.md#round_to_i32--flash-的取整约定)。

### 5.6 JPEG 嗅探不认 `GIF87a`

**位置**：`decoder.rs:311`

只匹配 `"GIF89a"`（`47 49 46 38 39 61`）。`GIF87a` 会落到 `Unknown` → `Err(Error::UnknownType)` 导致解码失败。老 SWF 里确实存在 GIF87a。

对比之下 JPEG 那条甚至专门认了一个畸形头（`decoder.rs:309`）——三个格式的宽容度不一致。

### 5.7 `validate_size` 的常量是像素数不是字节数

**位置**：`decoder.rs:457-465`

```rust
const INVALID_SIZE: usize = 0x8000000; // 128MB
let size = (width as usize).saturating_mul(height as usize);
if size >= INVALID_SIZE { return Err(Error::TooLarge); }
```

注释说 128MB，但比较的是 `width × height`——**像素数**。实际字节预算是 384–512 MB。常量名和错误信息（"larger than the rendering device supports"，从 Ruffle 继承）对无头转换器也不准确。

另外该方法在 `Rgb15` 和 `ColorMap8` 分支里有调用，**`Rgb32` 分支没有**。

### 5.8 bincode 的反炸弹限额粒度偏松

**位置**：`reader.rs:137,148`

```rust
.with_limit(bytes.len() as u64)      // ← 整个文件长度，不是 chunk 长度
```

恶意长度前缀最多能要求分配**整个文件大小**的内存。有界，但比"该 chunk 的大小"松得多。

### 5.9 镶嵌失败时缺少回滚

**位置**：`tessellator.rs:205-216`

```rust
match result {
    Ok(_)   => { if needs_flush { self.flush_draw(draw); } }
    Err(e)  => { error!("Tessellation failure: {:?}", e); }
}
```

渐变/位图路径镶嵌失败时，`flush_draw` 被跳过，但 `lyon_mesh` 里可能已有（部分）几何。这些几何会在后续某次 `flush_draw(DrawType::Color)` 时**被当作实色画出去**，同时 `gradients` 里对应的 uniform 成为孤儿。

鉴于 lyon 大体上"先算完再写"，实际风险不高，但失败分支里加一句 `self.lyon_mesh = VertexBuffers::new();` 是廉价的防御。

### 5.10 `is_stroke` / `mask_index_count` 未在 `tessellate_shape` 开头重置

**位置**：`tessellator.rs:49-51` 只重置了 `mesh` / `gradients` / `lyon_mesh`。

目前无害（`lib.rs:329` 每个 shape 都新建一个 `ShapeTessellator`），但如果改成复用实例，`is_stroke` 会从上个 shape 串味。

### 5.11 `BakedMovie::validate()` 的校验盲区

**位置**：`baked.rs:342-379`

不检查：`Shape.id` 是否在 SHAP/MORP 里、`Skin.symbol` 是否有对应 skin、`Group.blend_mode ≤ 14`、遮罩区间一致性、`start_frame` 与根时间轴长度的关系。

（`frame_rate` 曾经也在这张表里——它是 `ANIM` 的字段。现在已移入 `BakedMovie` 并由 `validate()` 检查。）

### 5.12 空文件被 git 跟踪

`src/decoder/utils.rs` 内容是 2 字节（`\r\n`），既没被声明为模块也没被引用。多半是从 Ruffle 拆分 `bitmap/utils.rs` 时的残留（那里面是 BT.601 的 LUT 表，后来改用 `wide` SIMD 后就废弃了）。

### 5.13 morph 记录流错位时不会重新同步

**位置**：`morph.rs:73-101`

两个"记录类型错位"分支只推进对应那一侧的光标，**没有重新同步机制**。畸形输入下两个流可能持续错位——好在 `lerp_edges` 里有 `unreachable!` 兜底（`morph.rs:307`）会 panic 而不是产出垃圾，所以不会静默出错。

---

## 6. 约定陷阱小结

写工具或读格式时最容易踩的：

| 陷阱 | 说明 |
|---|---|
| **命名三兄弟** | magic 是 `VATF`，crate 是 `vatf`，扩展名是 `.vab` |
| **POD chunk 无对齐填充** | 文件里紧凑排布；解析时必须自己保证对齐（`AlignedChunk`） |
| **POD chunk 是原生字节序** | 大端主机直接拒绝 |
| **bincode 必须 fixint** | 写端 `bincode::serialize` 默认 fixint，读端显式指定；改一端必须改另一端 |
| **`ShapeMesh` 是 48 字节** | 不是 64；`MorphEntry` 24；`GradientUniforms` 40 |
| **`MORP` 一对多** | 一个 `(morph_id, ratio)` 可能有多行，必须两级 map |
| **morph 与 shape 的判别** | 先查 MORP，没有才查 SHAP |
| **`(morph_id, ratio)` 只覆盖出现过的值** | 不能自由插值 |
| **顶点反量化参数在 `ShapeMesh` 里** | 不需要额外查表 |
| **`material::*` 从 0 开始，但着色器枚举从 1 开始** | `shape` / `repeat` 是 1/2/3 |
| **`sampler_flags`** | bit0 = smoothed，bit1 = repeating |
| **`place_frame` 是父时间轴的帧号** | 子时间轴位置 = `(parent - place) mod child_len` |
| ~~BAKD 不带帧率~~ | 已修复：`BakedMovie::frame_rate` |
| **clip 帧号是相对的，事件也是** | 根帧 = `start_frame + clip 索引` |
| **`Skin` 变体内部是局部空间** | 要乘 `Skin.transform`；`Shape` / `Group` / `Mask` 子节点已是世界空间 |
| **`Group` / `Mask` 没有自己的 transform** | 别多乘一次 |
| **`blend_mode > 1` 才包装 Group** | 因为 `swf::BlendMode` 没有判别值 1（`Normal=0`, `Layer=2`） |
| **`skin_` / `anim_` / `event_` 前缀是转换期约定** | 落盘时都被剥离了 |
| **有帧标签 ≠ 是 skin** | 还需要实例名有 `skin_` 前缀 |
| **滤镜 PASSES 位域位置不一** | Blur 在 bit 3–7，其余在 bit 0 起 |
| **`num_passes` 与 `flags` 冗余** | 两个都要会读 |
| **`Rgb15` 行填充 ×2，`ColorMap8` 不乘** | 每像素字节数不同 |
| **`DefineBitsJPEG3` 的 alpha 要 `min(a)` 钳位，`DefineBitsLossless` 不钳** | 不对称是 Flash 的实际行为 |
| **纹理全是 WebP（有损）** | 不是无损格式 |
| **顶点没有 UV** | 纹理坐标由着色器用 per-draw 仿射算 |

---

## 7. 可以继续做的方向

按"收益/风险"粗略排序，仅供后续参考：

**体积**
- 索引宽度自适应（顶点数 < 65536 时用 `u16`）——`INDX` 常常是最大的 chunk 之一。
- 帧间差分（重复帧只存一次引用）——大量 SWF 有静止帧段。
- 顶点/索引跨 shape 去重（同一形状被多次 `Place` 时）。

**性能**
- `process_morphs` 的 `pairs.contains` 是线性查找，换 `HashSet` 是 O(1)（`lib.rs:866-871`）。
- `ShapeTessellator` 可以在多个 shape 间复用（同时修掉 5.10）。
- WebP 编码可以并行（每个纹理独立）。

**正确性**
- 修复 §5.1 / §5.2 / §5.5 / §5.6。
- 给 §5.4 补一个实测测试确定 6 元素排布。
- 在 `validate()` 里补上 `Shape.id` / `Skin.symbol` 的存在性检查。
- 失败分支加回滚（§5.9）。

**保真度**
- `allow_scale_x/y`（非缩放描边）——影响带缩放的动画。
- 几何抗锯齿（顶点法线）——目前完全依赖 MSAA。
- morph ratio 的运行时插值。

**工程**
- 删掉 §4 里没有收益的死代码（尤其 `bt601.rs` 和空的 `decoder/utils.rs`）。
- 补一个 `size_of` 断言测试，把 [01 篇](01-format.md#3-pod-结构体字段表) 的结构体大小表变成可执行的契约。
