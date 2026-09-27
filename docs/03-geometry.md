# 03 · 形状解析 / 镶嵌 / 渐变 / morph 插值

本篇讲几何数据从 SWF 记录变成 `.vab` 里顶点索引的完整路径。涉及文件：`src/shape_utils.rs`、`src/tessellator.rs`、`src/morph.rs`，以及 `src/lib.rs` 的 `process_shape_geometry`。

---

## 1. 三层管线

```
swf::Shape
   │
   │  ① ShapeConverter                     shape_utils.rs:318
   │     SWF 记录流 → 抽象路径（twips 空间）
   │     处理双边填充、边汤焊接
   ▼
DistilledShape { paths: Vec<DrawPath> }
   │
   │  ② ShapeTessellator                   tessellator.rs:44
   │     Shape → lyon Path → 三角化
   │     同时按样式切分 draw call、去重渐变
   ▼
Mesh { draws: Vec<Draw>, gradients: Vec<Gradient> }
   │
   │  ③ VatfBuilder::process_shape_geometry   lib.rs:306
   │     顶点量化成 i16、纹理编码 WebP 并 intern、
   │     填 ShapeMesh / GradientUniforms / BMAP
   ▼
SHAP / SHME / GRAD / BMAP / TEXT / VERT / INDX
```

三层职责清晰：① 只关心 SWF 语义，② 只关心三角化与材质切分，③ 只关心落盘。

`DistilledShape` 里还有 `shape_bounds` / `edge_bounds` / `id` 三个字段，但**下游一个都没用**——`lib.rs:314-319` 直接读 `swf::Shape::edge_bounds`。它们是从 Ruffle vendored 时带过来的（见 [07 篇](07-design-notes.md)）。

---

## 2. SWF 双边填充模型

这是理解 SWF 形状的**第一道门槛**，也是 `shape_utils.rs` 存在的主要理由。

### 问题

SWF 里的一条边不是"属于某个填充"，而是**两侧各标一个填充样式**：

```
        fill_style_0   ←  边方向  ←   fill_style_1
```

每条 `StraightEdge` / `CurvedEdge` 都携带 `fill_style_0` 和 `fill_style_1` 两个样式 id。同一条边被相邻的两个填充区域**共享**，只编码一次。样式 id `0` 表示"这一侧没有填充"。

于是"属于样式 1 的所有边"并不是连续出现的——它们散落在整个记录流里，还混着方向相反的边。

### 解法（`shape_utils.rs:453-478`）

`ShapeConverter` 维护**三套并行累加器**：

```rust
fill_style0: ActivePath,   // 样式 0 侧的边
fill_style1: ActivePath,   // 样式 1 侧的边
line_style:  ActivePath,   // 描边（独立第三套）
```

每收到一个顶点，同时投喂给这三套（各自按自己的 `style_id` 判断是否启用）：

```rust
// shape_utils.rs:353-368（visit_point 的等价逻辑）
if self.fill_style1.style_id > 0 { self.fill_style1.add_point(point); }
if self.fill_style0.style_id > 0 { self.fill_style0.add_point(point); }
if self.line_style.style_id  > 0 { self.line_style.add_point(point);  }
```

### `flip` —— 关键的一步

一侧的线段结束时落桶，**样式 0 那条要翻转方向**：

```rust
// shape_utils.rs:300-308
fn flush_fill(&mut self, start: swf::Point<Twips>, pending: &mut [PendingPath], flip: bool) {
    if self.style_id > 0 && !self.segment.is_empty() {
        if flip { self.segment.flip(); }        // ← 只有 flip=true 才反转
        pending[self.style_id as usize - 1].add_segment(self.segment.clone());
    }
    self.segment.reset(start);
}
```

调用点（`shape_utils.rs:399-419`）：

```rust
// fill_style_1：不翻转
self.fill_style1.flush_fill(self.cursor, &mut self.fills, /*flip=*/false);
// fill_style_0：翻转
self.fill_style0.flush_fill(self.cursor, &mut self.fills, /*flip=*/true);
```

**为什么**（源码注释 `shape_utils.rs:166-172` 说得很直白）：

> *"Flash fill paths are dual-sided, with fill style 1 indicating the positive side and fill style 0 indicating the negative. We have to flip fill style 0 paths in order to link them to fill style 1 paths."*

即：原始边方向是"正侧在左"。对样式 1 来说，沿边方向走，样式 1 的区域在左侧——方向已经对了。但对样式 0 来说，它的区域在**右侧**，所以要把这条边反过来走，才能让"样式 0 的区域"落在左侧。翻转之后，两个桶里的轮廓**朝向约定一致**（都是"区域在左"），这才使得：

- 填充规则（nonzero / even-odd）在桶内全局成立；
- 相邻同类区域共享的边在桶里首尾相接，能**焊接成闭环**。

### 1-based 索引

```rust
pending[self.style_id as usize - 1]
```

样式 id 从 **1** 开始（`0` = 无填充），而 `self.fills` 是按样式数分配的 `Vec<PendingPath>`，所以下标要减一。同时还有一道范围校验（`shape_utils.rs:404`）：

```rust
let style_id = if new_style_id <= num_fill_styles { new_style_id } else { 0 };
```

越界的样式 id 被降级成 0（丢弃），而不是 panic。

### 什么情况下 `flip` 是空操作

最常见的"单一填充"形状：`fill_style_1 = 1`、`fill_style_0 = 0`。此时样式 0 那侧 `style_id == 0`，`flush_fill` 直接跳过，`flip` 从不生效——只有样式 1 的桶收到边。所以**最简单的形状根本走不到翻转逻辑**，这也让这个 bug 容易被忽略。

### 刷新点

| 时机 | 动作 | 位置 |
|---|---|---|
| `StyleChange` 带 `move_to` | 抬笔 → `flush_paths` | `shape_utils.rs:379-384` |
| `StyleChange` 带 `new_styles` | **先** `flush_layer`（用旧样式表），再换新样式表 | `shape_utils.rs:386-397` |
| 改变 `fill_style_1` | `flush_fill(flip=false)` | `shape_utils.rs:399-409` |
| 改变 `fill_style_0` | `flush_fill(flip=true)` | `shape_utils.rs:411-419` |
| 改变 `line_style` | `flush_stroke` | `shape_utils.rs:421-428` |

`flush_paths` 的顺序是 `fill_style1`（不翻）→ `fill_style0`（翻）→ `line_style`（`shape_utils.rs:471-478`）。

> **一处细节**：`new_styles` 分支必须**先**刷新再换样式表，因为刷新要用旧样式的引用。`push_layer`/`flush_layer` 的注释明确写了这一点。

---

## 3. 边汤焊接

SWF 给出的是**无序的边**（edge soup），源码注释（`shape_utils.rs:225-231`）用了这个词。所以每条线段落桶时要尝试与已有线段拼接：

```rust
// shape_utils.rs:246-271
fn add_segment(&mut self, mut new_segment: PathSegment) {
    if !new_segment.is_empty() {
        let mut start_open = true;
        let mut end_open = true;
        let mut i = 0;
        while (start_open || end_open) && i < self.segments.len() {
            let other = &mut self.segments[i];
            if start_open && other.end() == new_segment.start() {
                other.points.extend_from_slice(&new_segment.points[1..]);
                new_segment = self.segments.swap_remove(i);
                start_open = false;
            } else if end_open && new_segment.end() == other.start() {
                std::mem::swap(&mut other.points, &mut new_segment.points);
                other.points.extend_from_slice(&new_segment.points[1..]);
                new_segment = self.segments.swap_remove(i);
                end_open = false;
            } else {
                i += 1;
            }
        }
        self.segments.push(new_segment);
    }
}
```

三个非直觉的地方：

1. **`swap_remove` 被当作"取走累加器"用。** 合并成功后，`new_segment` 被赋值为从列表里摘出来的那条，循环继续——于是**一条新线段可以接连焊接多条已存线段**。这就是这个 `while` 循环存在的意义（如果只是"找一条接上"，一次匹配就够了）。
2. **`i` 在 `swap_remove` 之后故意不递增。** `swap_remove(i)` 会把**最后一个元素填到槽位 `i`**，而那个被换过来的元素还没被检查过。不递增才能保证它下一轮被访问。
3. **第二个分支先 `swap` 再 `extend`。** 合并后的顺序应当是 `other_old ++ new[1..]`，但代码把 `new` 当成了累加器，所以先把 `other` 的旧内容换进 `new`，再拼上 `other` 的新内容（此时 `other` 已经被截成 `new_segment.points[1..]`）。读的时候容易绕晕。

另外：**只有填充走焊接**。描边走 `push_path`（`shape_utils.rs:273-275`），每条线段独立成一条路径：

```rust
fn flush_stroke(&mut self, start, pending) {
    if self.style_id > 0 && !self.segment.is_empty() {
        pending[self.style_id as usize - 1].push_path(self.segment.clone());   // ← 不焊接
    }
    self.segment.reset(start);
}
```

这也解释了 `tessellator.rs:504-518` 的行为：每条描边线段各自成为**独立的 `DrawPath::Stroke`**。

> `swap_remove` 会打乱列表顺序。这里无害：子路径顺序不影响填充规则，而描边本来就是逐段独立处理的。

---

## 4. 控制点交错编码

`PathSegment` 用一个**扁平的点序列**同时表达直线和二次曲线（`shape_utils.rs:126-136`）：

```rust
struct Point { x: Twips, y: Twips, is_bezier_control: bool }
```

`CurvedEdge` 在 `shape_utils.rs:434-443` 处**原子地压入两个点**——先是控制点（`is_bezier_control = true`），再是锚点（`false`）：

```rust
ShapeRecord::CurvedEdge { control_delta, anchor_delta } => {
    self.cursor += *control_delta; self.visit_point(true);    // 控制点
    self.cursor += *anchor_delta;  self.visit_point(false);   // 锚点
}
```

于是"从控制点后面紧跟的那个点就是它的锚点"这条不变量恒成立，`to_draw_commands`（`shape_utils.rs:195-222`）就能用一个小状态机无歧义地还原：

```rust
std::iter::once(DrawCommand::MoveTo((*first).into())).chain(std::iter::from_fn(move || {
    match i.next() {
        Some(point @ Point { is_bezier_control: false, .. }) => Some(DrawCommand::LineTo((*point).into())),
        Some(point @ Point { is_bezier_control: true, .. }) => {
            let end = i.next().expect("Bezier without endpoint");   // ← 不变量在此
            Some(DrawCommand::QuadraticCurveTo { control: (*point).into(), anchor: (*end).into() })
        }
        None => None,
    }
}))
```

那个 `expect` 在正常路径下不可能触发，理由就是上面说的原子压入。

### `flush_layer` 的两个顺序决定

```rust
// shape_utils.rs:481-519
// 1) 填充按【样式 id 升序】输出，不是按绘制顺序
for (i, path) in self.fills.iter_mut().enumerate() { … }
// 2) 描边逐段输出
for … in self.strokes { … }
```

填充按样式 id 排序而非绘制顺序——因为同层的填充区域在 SWF 语义上是**互不重叠**的（它们共同划分整个层），顺序不影响结果。而描边必须在填充**之后**绘制，这里靠"先填充后描边"两次循环保证。

---

## 5. 曲线包围盒 —— 以及一个已核实的缺陷

`quadratic_curve_bounds`（`shape_utils.rs:522-569`）求二次贝塞尔曲线的**精确**轴对齐包围盒：先用两个端点初始化盒子，然后逐轴判断控制点是否超出端点区间；超出则解导数零点

```
t = (from - control) / (from - 2*control + anchor)
```

钳到 `[0,1]`，再用 Bernstein 形式求值 `s²·from + 2st·control + t²·anchor`。最后按 `stroke_width / 2` 外扩。

### ⚠️ 已知缺陷：起点传错

唯一调用点在 `calculate_shape_bounds`（`shape_utils.rs:51-65`）：

```rust
ShapeRecord::CurvedEdge { control_delta, anchor_delta } => {
    cursor += *control_delta;
    let control = cursor;
    cursor += *anchor_delta;
    let anchor = cursor;
    bounds = bounds.union(&quadratic_curve_bounds(
        cursor,            // ← 这里传的是 anchor，不是曲线起点
        Twips::ZERO,       // ← stroke_width 恒为 0（参数形同虚设）
        control,
        anchor,
    ));
}
```

`cursor` 在构造参数之前**已经被推进到锚点**了，所以实际求的是 `anchor → control → anchor` 这条退化曲线的包围盒。真实曲线的极值点可能落在这个盒子之外，于是**包围盒可能被低估**。

**当前影响有限**：`calculate_shape_bounds` 的产物只被 `morph.rs:123` 用作插值后形状的 `shape_bounds`，而落盘量化走的是 `edge_bounds`（见 [01 篇 §4](01-format.md#4-顶点量化)）。但如果将来有人拿 `shape_bounds` 做视锥剔除，这条就会变成真的渲染 bug。

顺带：`stroke_width` 参数在唯一调用点恒为 `Twips::ZERO`——Ruffle 原版会给描边传真实笔宽，这里的调用没有传。

---

## 6. lyon 镶嵌配置

`tessellate_shape`（`tessellator.rs:44-227`）对每条 `DrawPath` 做一次镶嵌，全部结果累加到**同一个** `lyon_mesh: VertexBuffers<Vertex, u32>` 上，靠 `flush_draw` 切分。

### 填充

```rust
self.fill_tess.tessellate_path(
    &lyon_path,
    &FillOptions::default().with_fill_rule(winding_rule.into()),
    &mut buffers_builder,
)
```

**只覆盖了 fill rule**，其余全用 lyon 默认值。特别地：**没有设置 tolerance**，用的是 lyon 默认的 `0.1`。由于这里的路径坐标已经是**像素**（`Twips::to_pixels()` = /20），0.1 像素的容差是合理的。

`FillRule` 来自 shape 的 `NON_ZERO_WINDING_RULE` 标志位（`shape_utils.rs:364-368`），映射到 `lyon_tessellation::path::FillRule`（`shape_utils.rs:9-16`）。

### 描边

```rust
let width = (style.width().to_pixels() as f32).max(1.0);
```

**笔宽下限 1 像素**。SWF 允许宽度为 0 的"发丝线"（hairline），但零宽在 lyon 里会镶嵌出空几何——整个描边消失。钳到 1 像素是个务实的近似。

```rust
swf::LineJoinStyle::Miter(limit) => {
    let limit = limit.to_f32();
    if limit >= StrokeOptions::MINIMUM_MITER_LIMIT {
        stroke_options = stroke_options.with_miter_limit(limit);
        lyon_tessellation::LineJoin::MiterClip
    } else {
        lyon_tessellation::LineJoin::Bevel     // 降级
    }
}
```

两处非平凡：

- 尖角映射到 lyon 的 **`MiterClip`** 而不是 `Miter`。`MiterClip` 会在超过 miter limit 时自动截断尖角，而 `Miter` 会直接放弃并回退——`MiterClip` 更接近 Flash 的表现。
- miter limit 低于 lyon 的 `MINIMUM_MITER_LIMIT` 时**降级为 bevel**，注释写的是 *"Avoid lyon assert with small miter limits"*——纯粹为了绕开 lyon 的断言 panic。

### 被忽略的描边属性

`LineStyle` 上的 `allow_scale_x` / `allow_scale_y`（非缩放描边）、`pixel_hinting`、`allow_close` **全部没有处理**。这意味着"不随缩放变粗"的描边在这个格式里会跟着缩放变粗。这是格式的一个**已知保真度缺口**。

### 失败处理

```rust
Err(e) => { error!("Tessellation failure: {:?}", e); }   // 注释：可能只是退化路径
```

镶嵌失败只记日志，不中断。见下面 §7 的一个相关隐患。

### `ruffle_path_to_lyon_path` 的状态机

`tessellator.rs:359-398` 用 `cursor: Option<Point>` 表示"有一个待处理的 MoveTo"：

| 状态 | 含义 |
|---|---|
| `Some(p)` | 收到过 `MoveTo`，子路径**尚未**开始 |
| `None` | 正在子路径内 |

- `MoveTo`：若已在子路径内则 `builder.end(false)` 收尾，然后记下新的起点（**不**立刻 `begin`）。
- `LineTo` / `QuadraticCurveTo`：若 `cursor` 是 `Some` 才 `begin(cursor)`，然后画。
- 收尾：`cursor.is_none()`（即处于子路径内）时按 `is_closed` 决定 `close()` 还是 `end(false)`。

这套惰性 `begin` 处理了两件事：连续的 `MoveTo` 不会产生空子路径，也不会触发 lyon 的"begin 时已经 begin"断言。填充恒传 `is_closed = true`（`tessellator.rs:59`），描边用 `segment.is_closed()`。

---

## 7. Draw call 切分

`Mesh.draws` 是运行时实际的绘制单元，切分策略在 `tessellator.rs:146-158`：

```rust
if needs_flush || (self.is_stroke && !next_is_stroke) {
    self.flush_draw(DrawType::Color);
} else if !self.is_stroke && next_is_stroke {
    assert!(self.mask_index_count.is_none());
    self.mask_index_count = Some(self.lyon_mesh.indices.len() as u32);
}
self.is_stroke = next_is_stroke;
```

| 情形 | 行为 |
|---|---|
| `needs_flush`（渐变/位图填充） | 先 flush 掉已积累的实色几何，自身镶嵌完**立刻**单独 flush |
| 描边 → 填充切换 | 强制 flush（因为遮罩渲染时要丢掉描边） |
| 填充 → 描边切换 | **不** flush，只记录 `mask_index_count` 分界点 |
| 实色填充连续 | 累积，不 flush |

净效果：

- **一组连续的实色填充 + 其后的描边**合并成**一个** `Color` draw call（省 draw call）；
- **每个渐变/位图填充**独占一个 draw call（它们要各自的 uniform 和纹理）。

### 顶点构造：没有 UV，没有法线

```rust
// tessellator.rs:463-485
struct RuffleVertexCtor { color: swf::Color }

impl FillVertexConstructor<Vertex> for RuffleVertexCtor {
    fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
        Vertex { x: vertex.position().x, y: vertex.position().y, color: self.color }
    }
}
```

**只取位置**。lyon 提供的 `normals()`（抗锯齿用）、`StrokeVertex::normal()` / `advancement()` **全部丢弃**，所以这个格式**放弃了几何抗锯齿**（需要 MSAA 或后处理）。

**也完全没有 UV**：`Vertex` 只有 `x, y, color`。渐变/位图的纹理坐标由着色器在运行时用**每个 draw 一个的 6 元素仿射**（`GRAD.texture_transform` / `BMAP`）作用于顶点位置算出来。

这解释了那个看起来很奇怪的设计：为什么要把纹理矩阵做成 uniform 而不是烘进顶点——**每个顶点因此省下 8 字节**（一个 `vec2` UV），而 draw call 数量本来就少，uniform 成本可以忽略。

> 对渐变/位图，`color` 被填成 `Color::WHITE`（`tessellator.rs:83/96/114/137`）——因为着色器会用纹理去乘顶点色，白色即"不做调制"。

### `mask_index_count` 是死代码

```rust
// tessellator.rs:254-260
pub struct Draw {
    pub draw_type: DrawType,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Reserved for the stencil-only mesh path.
    #[expect(dead_code)]
    pub mask_index_count: u32,
}
```

字段被计算得很仔细（还连带着那个 `assert!`），但**没有任何消费者**。它原本的用途是"渲染遮罩时只画填充、跳过描边"——需要把 draw call 内描边段的索引起点记下来。BakedNode::Mask 走的是另一条路（整个子树当遮罩），所以这条路径没被启用。

### 一处潜在隐患

`needs_flush` 的 draw **只在镶嵌成功时**才 flush（`tessellator.rs:205-211`）：

```rust
match result {
    Ok(_) => { if needs_flush { self.flush_draw(draw); } }
    Err(e) => { error!("Tessellation failure: {:?}", e); }
}
```

如果某个渐变路径镶嵌失败，`flush_draw(Gradient)` 被跳过，但此时 `lyon_mesh` 里可能已经积累了（部分）几何。这些几何会在后续某次 `flush_draw(DrawType::Color)` 时被当作**实色**画出去，且 `gradients` 里那条 uniform 变成孤儿。

实际影响取决于 lyon 失败时是否已经写入顶点缓冲——lyon 的 tessellator 大体上是"先算完再写"的，所以通常安全。但这里缺少显式的回滚（比如失败时 `lyon_mesh = VertexBuffers::new()`），是一个**应当补上的防御**。

另：`is_stroke` 和 `mask_index_count` 在 `tessellate_shape` 开头**没有**重置（`tessellator.rs:49-51` 只重置了 `mesh` / `gradients` / `lyon_mesh`）。目前无害，因为 `lib.rs:329` **每个 shape 都新建一个 `ShapeTessellator`**；但如果哪天改成复用实例，`is_stroke` 会从上个 shape 串味。

---

## 8. 渐变

### 8.1 去重的粒度：斜坡全局共享，矩阵每 draw 一份

```rust
// tessellator.rs:73-86
let uniform = swf_gradient_to_uniforms(GradientType::Linear, gradient, swf::Fixed8::ZERO);
let (gradient_index, _) = self.gradients.insert_full(uniform);

(DrawType::Gradient {
    matrix: swf_to_gl_matrix(gradient.matrix.into()),   // ← 矩阵在这里
    gradient: gradient_index,                            // ← 斜坡索引
}, swf::Color::WHITE, true)
```

`Gradient` 的 `Hash + Eq` 只比较 **类型 / 扩散方式 / 焦点 / 插值 / 色标列表**——**矩阵不在其中**。所以：

- **相同的色带**（哪怕被不同的矩阵摆在不同位置、不同大小）→ 只存**一张** 256×1 WebP 纹理；
- **每个 draw 各自**带一个 6 元素的仿射矩阵。

这是很划算的粒度：色带是重复度最高的东西（一个角色几十个部件可能共用同一条描边渐变），而矩阵本来就必须逐个 draw 存。

> 注意 `tessellator.rs:281-290` 里那个 `convert` 闭包两个分支**完全一样**（都是恒等）：
>
> ```rust
> let convert = if self.interpolation == swf::GradientInterpolation::LinearRgb {
>     |color| color
> } else {
>     |color| color
> };
> ```
>
> 也就是说 `LinearRgb`（线性色彩空间插值）**没有在烘焙时处理**，而是把标志位传给着色器（`GradientUniforms::interpolation = 1`），由着色器做 sRGB↔linear 转换。斜坡纹理永远按源格式的原始色彩空间烘焙。两边必须保持一致——改斜坡纹理格式时要留意。

### 8.2 斜坡采样 `compute_gradient_color`

```rust
for t in 0..gradient_size {
    let mut last = 0;
    let mut next = 0;
    for (i, record) in self.records.iter().enumerate().rev() {
        if (record.ratio as usize) < t {
            last = i;
            next = (i + 1).min(self.records.len() - 1);
            break;
        }
    }
    assert!(last == next || last + 1 == next);
    let factor = if next == last { 0.0 } else {
        (t as f32 - last_record.ratio as f32)
            / (next_record.ratio as f32 - last_record.ratio as f32)
    };
    colors[t * 4] = lerp(last_record.color.r as f32, next_record.color.r as f32, factor) as u8;
    // … g / b / a 同理
}
```

解析：

- **反向扫描**找的是"最后一个 ratio < t 的色标"。找到就取 `last = i`、`next = i + 1` 并跳出。
- **找不到**（`t = 0`，或 `t` 小于所有 ratio）时 `last = next = 0`，`factor = 0` → 取第一个色标的颜色。这就是"Pad"行为的实现。
- `ratio` 是 `u8`，被**直接当作梯度参数**用（`t ∈ 0..gradient_size`）。所以 `gradient_size = 256` 时，纹理的第 `t` 个像素精确对应 ratio `t`——**无需任何重标定**。写端固定传 256（`lib.rs:557`）。
- `lerp(a, b, f) = a + (b - a) * f`，最后 `as u8` **截断**取整（不是四舍五入）。
- `assert!(last == next || last + 1 == next)` 保证取到的是相邻两个色标。
- `records` 为空时返回全零（透明黑），`tessellator.rs:282-284`。

### 8.3 渐变矩阵 → `[[f32;3];3]`

```rust
// tessellator.rs:400-421
fn swf_to_gl_matrix(m: Matrix) -> [[f32; 3]; 3] {
    let tx = m.tx.get() as f32;
    let ty = m.ty.get() as f32;
    let det = m.a * m.d - m.c * m.b;
    let mut a = m.d / det;
    let mut b = -m.c / det;
    let mut c = -(tx * m.d - m.c * ty) / det;
    let mut d = -m.b / det;
    let mut e = m.a / det;
    let mut f = (tx * m.b - m.a * ty) / det;

    a *= 20.0 / 32768.0;  b *= 20.0 / 32768.0;
    d *= 20.0 / 32768.0;  e *= 20.0 / 32768.0;

    c /= 32768.0;  f /= 32768.0;  c += 0.5;  f += 0.5;

    [[a, d, 0.0], [b, e, 0.0], [c, f, 1.0]]
}
```

逐步推导：

**第一步：求逆。** SWF 的填充矩阵把**渐变空间映射到形状空间**，而着色器需要反过来（给定顶点位置求纹理坐标）。SWF 矩阵的标准形式是 `x' = a·x + c·y + tx`，其逆为：

```
a' =  d/det      c' = -c/det     tx' = (c·ty - d·tx)/det
b' = -b/det      d' =  a/det     ty' = (b·tx - a·ty)/det
```

对照代码里的局部变量：`a = a'`、`b = c'`、`c = tx'`、`d = b'`、`e = d'`、`f = ty'`。**注意局部变量名和 SWF 的字段名是错位的**——`b` 对应的是逆矩阵的 `c'`，读代码时极易搞混。

**第二步：单位换算。** 渐变空间以 `±16384` twips 为半宽（对应纹理 UV 的 `[0, 1]`，全长 `32768`）。设顶点位置为 `p` 像素、形状空间为 `s` twips，则 `s = 20p`：

```
g = A'·(20p) + t'          （形状 twips → 渐变 twips）
UV = g / 32768 + 0.5
   = (A' · 20/32768)·p + (t'/32768 + 0.5)
```

于是：线性部分乘 `20/32768`（`20` = twips/像素），平移部分只除 `32768` 再 `+0.5` 重定心（把中心的 `0` 映到 UV 的 `0.5`）。**代码与推导完全一致**。

**第三步：排布。**

```rust
[[a, d, 0.0],   // 局部变量 a, d
 [b, e, 0.0],   // 局部变量 b, e
 [c, f, 1.0]]   // 局部变量 c, f  = tx', ty'
```

按行主序展平后得到 6 个 float：

```
[a', b', c', d', tx', ty']
```

即"逆矩阵的线性部分（按 SWF 的 `a,b,c,d` 字段序）+ 逆平移"。

> ⚠️ **注释与实现对不上。** `lib.rs:564-568` 把这个 6 元素结果描述成 `[a, c, tx, b, d, ty]`，但 `flatten_matrix_3x3_to_6` 只是**行主序展平**：
>
> ```rust
> fn flatten_matrix_3x3_to_6(m: [[f32; 3]; 3]) -> [f32; 6] {
>     [m[0][0], m[0][1], m[1][0], m[1][1], m[2][0], m[2][1]]
> }
> ```
>
> 本仓库没有着色器代码，**无法在这里验证哪种顺序才是消费者期望的**。文档给出上面的推导，读者可拿一条已知渐变实测确认。这是格式文档里最需要小心的一处不一致。

**没有 det = 0 防护**——退化矩阵（零缩放）会产生 `inf`/`NaN`。注意 `matrix.rs` 里的 `Matrix::inverse` 是**有**检查的（返回 `Option`），但这里没有复用它。

### 8.4 位图矩阵

```rust
// tessellator.rs:423-446
fn swf_bitmap_to_gl_matrix(m: Matrix, bitmap_width: u32, bitmap_height: u32) -> [[f32; 3]; 3] {
    // 同样的求逆…
    a *= 20.0 / bitmap_width;   b *= 20.0 / bitmap_width;
    d *= 20.0 / bitmap_height;  e *= 20.0 / bitmap_height;
    c /= bitmap_width;          f /= bitmap_height;
    [[a, d, 0.0], [b, e, 0.0], [c, f, 1.0]]
}
```

与渐变的区别：

| | 渐变 | 位图 |
|---|---|---|
| 归一化除数 | `32768`（固定全宽） | `bitmap_width` / `bitmap_height`（**各向异性**） |
| 平移偏移 | `+= 0.5`（中心原点） | **不加**（左上角原点） |
| 尺寸来源 | 固定常量 | `CompressedBitmap::size()`（**只解头部**取尺寸） |

位图矩阵缺省找不到位图时整条路径被 `continue` 丢弃（`tessellator.rs:140-142`）。

`is_smoothed` / `is_repeating` 随 draw 带到 `ShapeMesh::sampler_flags`（[01 篇 §5](01-format.md#sampler_flags-位域)）。

---

## 9. morph 插值

`src/morph.rs` 把 `DefineMorphShape` 的 start/end 两个形状，按 `ratio`（0–65535）插值成一个完整的 `swf::Shape`，然后**走和普通形状完全一样的镶嵌流程**。

模块头注释写明来源：*"taken from swf_player's morph_shape.rs"*。

### 9.1 权重

```rust
let b = f32::from(ratio) / 65535.0;
let a = 1.0 - b;
```

所有插值都是 `start * a + end * b`。

调用方只在时间轴里**真实出现过的** `(morph_id, ratio)` 上插值（[02 篇 §7](02-pipeline.md#7-process_morphs--惰性插值)）。

### 9.2 样式的双游标合并

```rust
// morph.rs:51-117
while let (Some(s), Some(e)) = (start_rec, end_rec) {
    match (s, e) {
        (ShapeRecord::StyleChange(start_change), ShapeRecord::StyleChange(end_change)) => {
            let mut style_change = start_change.clone();     // ★ 只克隆 start
            …
            start_rec = start_iter.next();
            end_rec = end_iter.next();
        }
        (ShapeRecord::StyleChange(start_change), _) => { … 只推进 start … }
        (_, ShapeRecord::StyleChange(end_change)) => { … 只推进 end … }
        _ => { /* 两个都是边 → lerp_edges */ }
    }
}
```

★ **为什么要克隆 start 侧而不是合并两侧的样式 id**：SWF 的 morph **end shape 是以 `num_fill_bits = 0` / `num_line_bits = 0` 编码的**（`swf` crate 的读取器明确这么做）。这意味着 end 侧的每个样式索引都读出来是 **0**，完全无意义。只有 start 侧的样式 id 是真实有效的，所以必须保留。

样式数组本身则是**按位置逐一配对插值**的（`morph.rs:22-38`）：`fill_styles` 和 `line_styles` 各自 `zip`。两侧长度不一致时会静默按短的截断。

三个 `match` 分支处理"记录类型错位"的情况：当两侧在同一位置给出不同类型的记录时，把 `StyleChange` 单独发出去，只推进对应那一侧的光标，让另一侧等一等。**这套机制不会 panic，但也不保证重新同步**——畸形输入下两个流可能持续错位（见 [07 篇](07-design-notes.md)）。

### 9.3 `lerp_edges` —— 类型统一

插值后的单条记录必须**语义唯一**：不能一半是直线一半是曲线。四种组合（`morph.rs:241-309`）：

| start | end | 处理 |
|---|---|---|
| 直线 | 直线 | 直接插值锚点 |
| 曲线 | 曲线 | 分别插值控制点与锚点 |
| **直线** | **曲线** | 直线**升格为二次曲线**：在直线中点合成控制点 |
| **曲线** | **直线** | 对称地升格 end 侧的直线 |

升格的实现（`morph.rs:276-290`）：

```rust
let start_control = start_pen + *sd / 2;                    // 直线中点 = 合成控制点
let control = lerp_point_twips(start_control, end_pen + *ec, a, b);
let anchor  = lerp_point_twips(start_pen + *sd, end_pen + *ec + *ea, a, b);
ShapeRecord::CurvedEdge { control_delta: control - pen, anchor_delta: anchor - control }
```

把直线的控制点放在中点，得到的是一条**几何上与原直线完全重合**的二次曲线（在 `t=0` 和 `t=1` 处退化）——镶嵌器处理它和处理直线没有任何区别，于是类型统一了。

另外注意：插值一律在**绝对坐标**下做（`start_pen + delta`），最后再减去插值后的钢笔位置转回**增量**。这样做避免了"对增量本身插值"带来的误差累积。

两个流都不同步时 `unreachable!` 会触发（`morph.rs:307`）——这是全模块唯一的硬 panic 点。

**钢笔位置的推进**（`update_pos`，`morph.rs:151-171`）：直线按 `delta`，曲线按 `control_delta + anchor_delta`（即走到锚点），`StyleChange` 按 `move_to` 绝对定位。

### 9.4 `edge_bounds` 必须插值源包围盒

```rust
// morph.rs:123-126
let shape_bounds = calculate_shape_bounds(&shape);
// `edge_bounds` must include stroke widths. The interpolated edge records
// carry no stroke half-widths, so interpolate the source bounds instead.
let edge_bounds = lerp_rect(&start.edge_bounds, &end.edge_bounds, a, b);
```

**这是一处关键的闭环**：`lib.rs:311-319` 用量化顶点时用的是 `edge_bounds`，而插值后的边记录**不携带描边半宽**——无法从它们重算出正确的 `edge_bounds`。如果这里图省事用 `calculate_shape_bounds` 的产物，粗描边的 morph 形状会在 [01 篇 §4](01-format.md#4-顶点量化) 描述的那个环节被钳死压平。

`shape_bounds` 则确实是从插值后的记录重算的——它不含描边，所以重算是对的。不过它继承了 §5 里那个 `quadratic_curve_bounds` 起点传错的缺陷。

### 9.5 其余插值细节

| 对象 | 方式 | 位置 |
|---|---|---|
| `Twips` | `(s*a + e*b).round() as i32` | `morph.rs:184-186` |
| `Color` | 逐通道 `f32` 加权，`as u8` **截断** | `morph.rs:175-182` |
| `Matrix` 线性部分 | 权重先量化成 `Fixed16`，再用 `Fixed16` 相乘 | `morph.rs:311-322` |
| `Matrix` 平移 | 走 `lerp_twips` | 同上 |
| `Gradient` | 色标 `ratio` 与颜色逐项插值；**`spread` 和 `interpolation` 取 start 侧** | `morph.rs:324-342` |
| `FocalGradient` | 焦点也用 `Fixed8` 加权 | `morph.rs:229-232` |
| `Bitmap` | 矩阵插值；**id 取 start 侧**，`is_smoothed` / `is_repeating` 也取 start | `morph.rs:199-212` |
| 类型不匹配的填充 | `warn!` + **保留 start** | `morph.rs:234-238` |

### 9.6 硬编码的字段

```rust
swf::Shape {
    version: 4,                                  // ← 硬编码
    id: 0,                                       // ← 硬编码（这也是为什么要避开 ShapeRecord）
    flags: swf::ShapeFlag::HAS_SCALING_STROKES,  // ← 硬编码
    …
}
```

源 `DefineMorphShape` 的 `flags`（`HAS_SCALING_STROKES` / `HAS_NON_SCALING_STROKES`）和 `version` 都被忽略。`id: 0` 是刻意的——配合 [02 篇 §7](02-pipeline.md#7-process_morphs--惰性插值) 说的"morph 网格不写 `ShapeRecord`"。

---

## 10. 本模块的保真度缺口汇总

理解这个格式的边界时，这些是有用的：

| 缺口 | 原因 |
|---|---|
| 无几何抗锯齿 | lyon 的 `normals()` 被丢弃 |
| 零宽描边被钳到 1 像素 | 避免镶嵌出空几何 |
| miter limit 过小时降级为 bevel | 绕开 lyon 断言 |
| `allow_scale_x/y` / `pixel_hinting` / `allow_close` 未处理 | 未实现 |
| morph 的 `flags` / `version` 被忽略 | 硬编码 |
| `LinearRgb` 插值交给着色器 | 斜坡纹理不预线性化 |
| WebP 有损 | 统一纹理格式的代价 |
| morph ratio 只能吸附不能插值 | 只在出现过的 ratio 上烘焙 |
