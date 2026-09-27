# animx / `vatf` 设计与实现

> 把 Flash `.swf` **离线烘焙**成 `.vab` 运行时格式的转换器。

## 一句话定位

Flash 播放器的复杂度几乎全在**运行时求值**上：`PlaceObject` 逐帧累积成显示列表、嵌套 sprite 各跑各的时间轴、渐变/位图填充要靠矩阵实时算纹理坐标、morph 形状要按 ratio 实时插值、滤镜要实时估离屏纹理尺寸。

这个项目把这些**全部前移到转换阶段算完**，输出的 `.vab` 里只剩三类内容：

1. **静态几何表**——已经镶嵌好的顶点/索引，顶点量化成 `i16`；
2. **动画帧表**——每一帧的**完整**绘制列表，且所有变换**已经乘成世界空间**；
3. **纹理池**——渐变斜坡与位图统一压成 WebP。

于是运行时要做的事收缩成"查表 + 转发变换 + 按需解码纹理"，不需要任何 per-instance 时间轴状态。

---

## 模块地图

```
src/
├── main.rs          CLI 入口（单文件 / 目录批处理）                    [bin]
├── lib.rs           crate 根；POD 格式类型、顶点量化、VatfBuilder、
│                    convert_swf_to_vab、parse_tags、process_morphs      [pub]
├── reader.rs        VabReader：.vab 解析与类型化访问                    [pub]
├── baked.rs         BAKD 烘焙器（Compiler）+ BakedMovie 数据模型        [pub]
├── animation.rs     线格式（Anim* 类型）、滤镜线类型、
│                    filter_dest_rect 离屏尺寸计算                      [pub]
├── filter.rs        解析期滤镜类型（持有 swf::Filter）                  [pub]
├── transform.rs     Transform / TransformStack                         [pub]
├── matrix.rs        仿射矩阵（SWF a,b,c,d,tx,ty 约定）                  [私有]
├── shape_utils.rs   SWF 形状记录 → DrawPath（双边填充模型）             [私有]
├── tessellator.rs   lyon 镶嵌 → Draw 列表 + 渐变去重                    [私有]
├── morph.rs         morph 形状按 ratio 插值                            [私有]
├── bitmap.rs        CompressedBitmap（延迟解码的位图）                  [私有]
└── decoder/         位图解码
    ├── decoder.rs   JPEG/PNG/GIF/DefineBitsLossless 解码、JPEGTables 兼容
    ├── bt601.rs     YUV 4:2:0 → RGBA SIMD（`wide`）
    ├── error.rs     解码错误类型
    └── utils.rs     （空文件，见 07 篇）
```

> 注意哪些是 `pub`：`matrix` / `morph` / `shape_utils` / `tessellator` / `bitmap` / `decoder` 都是**私有模块**，它们只是转换器的内部实现。对外可用的只有 `reader` / `baked` / `animation` / `filter` / `transform` —— 也就是"**运行时需要知道的那些类型**"。这个可见性划分本身就在表达设计意图。

### 依赖分工

| 依赖 | 用途 |
|---|---|
| `swf 0.2.2` | SWF 标签解析（**只解析，不做像素解码**） |
| `lyon_tessellation` | 填充/描边的三角化 |
| `bytemuck` | `#[repr(C)]` POD 与字节切片互转（零拷贝） |
| `bincode` | `BAKD` chunk 的序列化 |
| `image` | 位图与渐变斜坡的 **WebP 编码** |
| `jpeg-decoder` / `png` / `gif` / `flate2` | 位图像素解码 |
| `wide` | `bt601` 的 YUV→RGB SIMD |
| `clap` / `anyhow` / `tracing` | CLI / 错误 / 日志 |

---

## 端到端数据流

### 转换侧（`.swf` → `.vab`）

```
 .swf
   │  decompress_swf → parse_swf                        (swf crate)
   ▼
 Vec<Tag>
   │
   │  parse_tags                                        lib.rs:635
   ├─ DefineShape ────────► ShapeTessellator ───────────► vertices / indices /
   │                        （见 03 篇）                    SHME / GRAD / BMAP / TEXT
   ├─ DefineMorphShape ───► morph_shapes（仅登记，延后处理）
   ├─ DefineBits* ────────► bitmap: CompressedBitmap（仅登记，延后解码）
   ├─ DefineSprite ───────► 递归 parse_tags
   └─ PlaceObject / RemoveObject / ShowFrame
              │
              ▼
      animations: HashMap<CharacterId, Vec<Vec<DisplayObject>>>
              │
              │  process_morphs                          lib.rs:853
              │  （只对时间轴里真实出现过的 (morph_id, ratio)
              │    插值 + 镶嵌，写 MORP 表）
              ▼
        write_vatf                                         lib.rs
              │
              ├─ AnimContainer::from_parts
              │  （排序归一化 → 确定的烘焙输出）
              │        ▼
              ├─ bake_with_skin_variants ─► BAKD  (bincode) ─► validate()
              ▼
           .vab
```

### 读取侧（`.vab` → 可渲染的绘制列表）

```
 .vab
   │  VabReader::from_bytes                             reader.rs
   ├─ 校验 magic / version / length
   ├─ 逐 chunk ──► AlignedChunk（Vec<u64> 承载，保证 8 字节对齐）
   └─ BAKD ──► bincode ──► BakedMovie ──► validate()   ← 读取时也校验
                                │
                                ▼
                  clip = movie.clips[i]              （按名字选）
                  nodes = clip.frames[playhead]      ← 完整绘制列表
                                │
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
   BakedNode::Shape        ::Skin                  ::Group / ::Mask
        │                       │                       │
   查 SHME/SHAP             查 movie.skins          离屏 + 滤镜/混合
   （或 SHME/MORP，          [symbol].variants[i]   / stencil
     按 (id, ratio)）       （变体内部仍是局部空间）
   反量化 → VERT/INDX
   按 material_type 取
   GRAD / BMAP / TEXT
```

---

## 单一产物：BAKD

`.vab` 只存**一套**动画表示：

| chunk | 类型 | 语义 |
|---|---|---|
| **BAKD** | `BakedMovie` | 少量**命名 clip** 的、**已完全展开**的绘制树（世界变换）+ 帧率 |

`baked.rs:71` 的注释给了态度：*"普通 sprite 时间轴在这里求值，绝不由运行时渲染器求值"*。

> **沿革**：早期版本还有第 10 个 chunk `ANIM`（`AnimContainer`，**未展开**的 per-sprite 逐帧显示列表）。两者并存，ANIM 是唯一的帧率来源，也是 `tests/swf_oracle.rs` 差分测试的基准。
>
> 实测它占动画密集型产物 **17%–54%** 的体积（`yue_se.vab` 54.2%、`spirit3021src.vab` 23.1%），而**唯一只有它才有的数据是一个 f32 的帧率**——SWF 的帧率在 movie header 上，`DefineSprite` 根本没有自己的帧率字段。于是帧率移入 `BakedMovie::frame_rate`，chunk 删除，版本号重置为 `1`。
>
> oracle 测试失去了文件级的基准来源，改用 `vatf::parse_animation_container` 在**进程内**拿同一份容器——测试能力不变，格式不必为测试背一个 chunk（[04 篇 §5](04-animation.md#5-差分测试oracle)）。

---

## 快速上手

### 转换

```bash
# 单文件（输出路径不给 .vab 时按输入文件名推导）
cargo run -- input.swf -o out.vab

# 目录批处理（默认输出到 <输入目录>/output）
cargo run -- assets/
```

日志由 `tracing` 输出，能看到 sprite 数量、解析耗时、写出耗时。

### 读取

```rust
use vatf::reader::VabReader;

let reader = VabReader::open("out.vab")?;

// 静态几何
let meshes  = reader.shape_meshes().unwrap();   // &[ShapeMesh]
let records = reader.shape_records().unwrap();  // &[ShapeRecord]  shape id → mesh 区间
let verts   = reader.vertices().unwrap();       // &[Vertex]       量化 i16
let idx     = reader.indices().unwrap();        // &[u32]
let tex     = reader.texture_data().unwrap();   // &[u8]  交错 WebP 池

// 动画（BAKD，唯一的动画数据源）
let movie = reader.baked();
let clip  = &movie.clips[0];
let nodes = &clip.frames[0];                    // 该帧完整绘制列表
let fps   = movie.frame_rate;                   // f32，来自 movie header
```

完整 API 见 [06-runtime.md](06-runtime.md)。

### 测试与基准

```bash
cargo test          # 单元测试 + tests/swf_oracle.rs 差分测试
cargo bench         # benches/bench_read.rs
```

⚠️ 两个外部依赖：
- `tests/swf_oracle.rs` 的 `oracle_matches_baked_sample` 依赖 `../bevy_flash/assets/spirit2159src.swf`（即 `D:\Code\Rust\bevy_flash\assets\`）。**缺失时它打印 "skipping" 并直接返回**——CI 上不会失败，但会静默失去这段覆盖。
- `benches/bench_read.rs` 硬编码了 `D:\Code\Rust\bevy_flash\assets\spirit2159src.vab`。文件缺失**或版本不符**时它会打印原因并**跳过**（不 panic），重新转换即可。

⚠️ **改了格式就要重新生成产物。** 版本号是精确匹配的陈旧文件探测器（[01 篇 §8](01-format.md#8-版本与兼容性)），旧 `.vab` 会以 `Unsupported VAB version N` 被拒绝。`bevy_flash_remake` 依赖本 crate：
```bash
# 在 bevy_flash_remake/ 下
cargo run --manifest-path ../animx/Cargo.toml -- ../bevy_flash/assets/wu_kong.swf -o assets/wu_kong.vab
```

---

## 阅读指南

| 你想搞清楚…… | 看这篇 |
|---|---|
| `.vab` 每个字节是什么、字段怎么排 | [01-format.md](01-format.md) |
| `.swf` 怎么一步步变成 `.vab`；显示列表模型 | [02-pipeline.md](02-pipeline.md) |
| SWF 双边填充是什么；lyon 怎么用；渐变矩阵怎么推 | [03-geometry.md](03-geometry.md) |
| 嵌套 sprite 时间轴怎么展开；skin 变体是什么 | [04-animation.md](04-animation.md) |
| JPEGTables 的坑；位图各种格式；YUV SIMD | [05-assets.md](05-assets.md) |
| 播放器该怎么写；每帧要做什么 | [06-runtime.md](06-runtime.md) |
| 为什么这样设计；有哪些坑和死代码 | [07-design-notes.md](07-design-notes.md) |

---

## 阅读约定

- 正文中文，标识符 / 类型名 / 常量保留英文原文（如 `ShapeMesh`、`place_frame`）。
- 代码引用一律写成 `文件:行号`，可直接跳转。
- 涉及公式的地方会给出**完整推导**，而不是只贴代码——这些公式是理解格式的关键。
- 标注为 **⚠️ 已知问题** 的地方是本次通读代码时核实过的缺陷或文档不符之处，**只记录不修复**，汇总在 [07-design-notes.md](07-design-notes.md)。
