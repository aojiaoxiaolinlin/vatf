# 05 · 位图解码 / JPEG 兼容 / YUV SIMD / 滤镜

> 本篇保留源码分析摘录和历史行号；当前编译模式、UI、裁剪与预处理契约见 [08](08-ui-and-compilation.md)。源码行号可能随重构变化。

涉及文件：`src/bitmap.rs`、`src/decoder.rs`、`src/decoder/bt601.rs`、`src/decoder/error.rs`、`src/filter.rs`。

> **来源说明**：这个子系统大量 vendored 自 Ruffle（`ruffle-render/src/bitmap/`）。`decoder.rs` 的注释里直接引用了上游 issue（`#8775`、`#1191`），`bt601.rs` 的测试与注释也来自上游。想深挖可以对照 Ruffle 仓库。

---

## 1. `CompressedBitmap` —— 延迟解码

```rust
// bitmap.rs:5-14
#[derive(Clone)]
pub enum CompressedBitmap {
    Jpeg { data: Vec<u8>, alpha: Option<Vec<u8>>, width: u16, height: u16 },
    Lossless(DefineBitsLossless<'static>),
}
```

**只有两个变体**，但覆盖了 SWF 的四种位图标签：

| SWF 标签 | 存成 |
|---|---|
| `DefineBits`（JPEG1，需拼 JPEGTables） | `Jpeg` |
| `DefineBitsJpeg2`（JPEG/PNG/GIF） | `Jpeg` |
| `DefineBitsJpeg3`（JPEG/PNG/GIF + alpha） | `Jpeg { alpha: Some(..) }` |
| `DefineBitsLossless` / `Lossless2` | `Lossless` |

**JPEG / PNG / GIF 不区分**——`DefineBitsJpeg2` 里塞的可能是 PNG 或 GIF，但解析时一律存进 `Jpeg` 变体，等到 `decode()` 时才由魔数嗅探决定走哪条路（`decoder.rs:306`）。

这样做的理由：**格式嗅探必须推迟**，因为
- JPEG1 要先和 `JPEGTables` 拼接才能嗅探；
- `DefineBitsJpeg3` 的 alpha 数据布局依赖真实格式。

`decode()` 的两个分支（`bitmap.rs:30-39`）分别转调 `decode_define_bits_jpeg` 和 `decode_define_bits_lossless`。

### 什么时候才解码

转换流程里，位图**登记**在解析期、**解码**在镶嵌期：

```rust
// lib.rs:411-434（镶嵌到这个位图填充时）
DrawType::Bitmap(bm) => {
    let Some(compressed_bitmap) = bitmap.get(&bm.bitmap_id) else { continue };
    let decoded = match compressed_bitmap.decode() {
        Ok(decoded) => decoded,
        Err(e) => { error!("Failed to decode bitmap: {:?}", e); continue; }
    };
    let bitmap_rgba = decoded.into_rgba();
    …然后立刻编码成 WebP…
}
```

后果是好的：**没被任何形状引用的位图永远不解码**。`size()`（`bitmap.rs:17-28`）提供廉价的尺寸查询，供 [03 篇 §8.4](03-geometry.md#84-位图矩阵) 的矩阵归一化使用——它只读结构体里已存的 `width` / `height`，不做任何解码。

⚠️ 解码失败是 `error!` + `continue`：**该 mesh 被静默丢弃**，形状少一块填充，但整个转换仍然"成功"。

---

## 2. `Bitmap` 与格式归一

```rust
pub struct Bitmap { width: u32, height: u32, format: BitmapFormat, data: Vec<u8> }

pub enum BitmapFormat { Rgb, Rgba, Yuv420p, Yuva420p }
```

### 尺寸安全网

```rust
// decoder.rs:24-42
pub fn new(width: u32, height: u32, format: BitmapFormat, mut data: Vec<u8>) -> Self {
    let expected_len = format.length_for_size(width as usize, height as usize);
    if data.len() != expected_len {
        warn!("Incorrect bitmap data size, expected {} bytes, got {}", expected_len, data.len());
        data.resize(expected_len, 0);      // 截断 或 补零
    }
    …
}
```

`Vec::resize` **同时具备截断和补零两种能力**，所以这一行把"长度不对"的所有情况都归一化了——构造出的 `Bitmap` 永远不可能让下游越界。这是很经济的防御。

`length_for_size`（`decoder.rs:140-149`）用 `div_ceil` 处理色度平面的奇数尺寸：

| 格式 | 长度 |
|---|---|
| `Rgb` | `w·h·3` |
| `Rgba` | `w·h·4` |
| `Yuv420p` | `w·h + ⌈w/2⌉·⌈h/2⌉·2` |
| `Yuva420p` | `w·h·2 + ⌈w/2⌉·⌈h/2⌉·2` |

### `into_rgba` —— 唯一的归一化出口

所有格式最终统一成**交错的预乘 RGBA**：

| 源 | 处理 |
|---|---|
| `Rgb` | `chunks_exact(3).flat_map(\|rgb\| [r, g, b, 255])` |
| `Rgba` | 无操作 |
| `Yuv420p` | 平面拆包 → `yuv420_to_rgba`（alpha 恒 255） |
| `Yuva420p` | 平面拆包 → `yuv420_to_rgba`，再**逐像素 `min(a)` 钳位** |

`Yuva420p` 那一路的钳位（`decoder.rs:78-82`）：

```rust
// RGB components need to be clamped to alpha to avoid invalid premultiplied colors
self.data = rgba.chunks_exact(4).zip(a)
    .flat_map(|(rgba, a)| [rgba[0].min(*a), rgba[1].min(*a), rgba[2].min(*a), *a])
    .collect();
```

> ⚠️ **`Yuv420p` / `Yuva420p` 两个变体没有任何解码器会产生**（都标了 `#[allow(unused)]`）。它们是从 Ruffle 带过来的残留（上游有视频/VP6 路径），`into_rgba` 里的 YUV 分支**目前不可达**。这也意味着整个 `bt601.rs`（483 行，位图子系统里最大的文件）**只被它自己的测试触达**。见 [07 篇](07-design-notes.md)。

---

## 3. JPEGTables：为什么需要字节手术

这是整个项目里最"SWF 特色"的一段知识，集中在 `decoder.rs:378-453` 的长注释里。

### 问题

SWF 为了压缩体积，把 JPEG 的**量化表（DQT）和 Huffman 表（DHT）**从一个图像里抽出来，单独放在一个 `JPEGTables` 标签里全局共享，`DefineBits` 里只留帧数据。于是：

```
JPEGTables 的内容:   SOI  DQT… DHT…  EOI
DefineBits 的内容:   SOI  SOF… SOS… 压缩数据  EOI
```

两部分**各自都不是合法的完整 JPEG**。Flash 在运行时把它们拼起来。

### 拼接产生的畸形

朴素的拼接（去掉表的 EOI、去掉图的 SOI）会得到：

```
SOI  DQT… DHT…  [FF D9 FF D8]  SOF… SOS… 数据  EOI
                └── 内部多出来的 EOI+SOI ──┘
```

这个 `FF D9 FF D8`（EOI 紧跟 SOI）出现在流中间。**标准 JPEG 解码器遇到 EOI 就停下**，于是什么都解不出来——而 Flash 的解码器认识这个组合并跳过它（注释里的推测：这是 `JPEGTables` 时代的遗留，Flash 解码器"期望"看到这一对）。

注释还指出：这与 SWF 规范说的"只在 v8 之前的文件里出现于开头"**不符**——它可能出现在 SOF 之前的任何位置，而且 v9 的文件里也有。

### `glue_tables_to_jpeg`

```rust
// decoder.rs:358-376
pub fn glue_tables_to_jpeg<'a>(jpeg_data: &'a [u8], jpeg_tables: Option<&'a [u8]>) -> Cow<'a, [u8]> {
    if let Some(jpeg_tables) = jpeg_tables && jpeg_tables.len() >= 2 {
        let mut full_jpeg = Vec::with_capacity(jpeg_tables.len() + jpeg_data.len());
        full_jpeg.extend_from_slice(&jpeg_tables[..jpeg_tables.len() - 2]);   // 去掉表的尾部 EOI
        if jpeg_data.len() >= 2 {
            full_jpeg.extend_from_slice(&jpeg_data[2..]);                     // 去掉图的头部 SOI
        }
        return full_jpeg.into();
    }
    jpeg_data.into()      // 没有表 → 借用，零拷贝
}
```

无条件砍掉表的**最后 2 字节**（假定是 EOI）、图的**前 2 字节**（假定是 SOI），然后拼接。返回 `Cow` 让"没有 JPEGTables"这个常见情况（JPEG2/JPEG3）不产生拷贝。

### `remove_invalid_jpeg_data`

```rust
// decoder.rs:380-453（注释已省略，见源码）
const SOF0: u8 = 0xC0;   // Start of frame
const RST0: u8 = 0xD0;   // Restart
const RST7: u8 = 0xD7;
const SOI:  u8 = 0xD8;   // Start of image
const EOI:  u8 = 0xD9;   // End of image

let mut data: Cow<[u8]> = if let Some(stripped) = data.strip_prefix(&[0xFF, EOI, 0xFF, SOI]) {
    // 最常见的情况：序列就在开头（正如规范所说），调整切片避免拷贝
    stripped.into()
} else {
    let mut jpeg_data = data;
    let mut pos = 0;
    loop {
        if jpeg_data.len() < 4 { break data.into(); }
        let payload_len: usize = match &jpeg_data[..4] {
            [0xFF, EOI, 0xFF, SOI] => {
                // 找到非法的 EOI+SOI，就地剪除
                let mut out_data = Vec::with_capacity(data.len() - 4);
                out_data.extend_from_slice(&data[..pos]);
                out_data.extend_from_slice(&data[pos + 4..]);
                break out_data.into();
            }
            // EOI / SOI / RST 这些标记后面【不】跟长度字段
            [0xFF, EOI | SOI | RST0..=RST7, _, _] => 0,
            [0xFF, SOF0, _, _] => { break data.into(); }   // 到达 SOF，停止搜索
            // 其它标记后面跟一个 big-endian u16 长度
            [0xFF, _, a, b] => u16::from_be_bytes([*a, *b]).into(),
            _ => { break data.into(); }   // 不是标记（JPEG 标记必以 0xFF 开头）→ 放弃
        };
        jpeg_data = jpeg_data.get(payload_len + 2..).unwrap_or_default();
        pos += payload_len + 2;
    }
};
```

算法本质是一个 **JPEG 标记遍历器**：从流开头按标记长度逐段跳（段长 + 2 字节的标记头），遇到 `FF D9 FF D8` 就剪掉，遇到 `SOF0` 就停止搜索（SOF 之后不可能再有这种情况）。

几处值得注意的写法：

- **零拷贝快路径**：序列在开头时用 `strip_prefix` 直接返回子切片。这在"v8 之前的文件"这个符合规范的情况下是常态。
- **`[0xFF, EOI | SOI | RST0..=RST7, _, _]`** 这个模式用的是 Rust 的**或模式（or-pattern）**：`EOI | SOI | RST0..=RST7` 展开成 `0xD9 | 0xD8 | 0xD0..=0xD7`。可读性一般，但很紧凑。
- **`payload_len + 2`** 是"标记(2) + 长度字段本身也算在长度里"，所以跳 `payload_len + 2` 正好落到下一个标记。这是 JPEG 格式的段长定义（段长包含自身的 2 字节）。
- 遇到 `_ =>`（首字节不是 `0xFF`）时**直接放弃并返回原数据**，不报错——因为可能根本不是 JPEG。

### 补 EOI

```rust
// decoder.rs:443-452
// Some JPEGs are missing the final EOI marker (JPEG optimizers truncate it?)
// Flash and most image decoders will still display these images, but jpeg-decoder errors.
// Glue on an EOI marker if its not already there and hope for the best.
if data.ends_with(&[0xFF, EOI]) { data }
else {
    warn!("JPEG is missing EOI marker and may not decode properly");
    data.to_mut().extend_from_slice(&[0xFF, EOI]);
    data
}
```

有些 JPEG 优化器会把结尾的 EOI 砍掉。Flash 和大多数解码器容忍，但 `jpeg-decoder` 会报错——所以补一个。`to_mut()` 是 `Cow` 真正发生克隆的地方。

### ⚠️ 只解尺寸的路径也必须先做手术

```rust
// decoder.rs:467-477
fn decode_jpeg_dimensions(jpeg_data: &[u8]) -> Result<(u16, u16), Error> {
    let jpeg_data = remove_invalid_jpeg_data(jpeg_data);    // ← 不能省
    let mut decoder = jpeg_decoder::Decoder::new(&jpeg_data[..]);
    decoder.read_info()?;
    …
}
```

`read_info()` 只读头部，但它**同样会踩到中间那个 EOI**并报告错误的尺寸。所以 [02 篇 §5](02-pipeline.md#5-位图只登记不解码) 说的"只解尺寸"并不是免费的——它也得付一次标记遍历的代价。

### `decode_define_bits_jpeg_dimensions` 的分发

```rust
// decoder.rs:346-354
match determine_jpeg_tag_format(data) {
    JpegTagFormat::Jpeg => decode_jpeg_dimensions(data),
    JpegTagFormat::Png  => decode_png_dimensions(data),
    JpegTagFormat::Gif  => decode_gif_dimensions(data),
    Unknown => Err(Error::UnknownType),
}
```

注意这里**没有**先做 `glue_tables_to_jpeg`——调用方（`lib.rs:781`）已经拼好了。

---

## 4. 格式嗅探

```rust
// decoder.rs:306-314
pub fn determine_jpeg_tag_format(data: &[u8]) -> JpegTagFormat {
    match data {
        [0xff, 0xd8, ..]                                     => JpegTagFormat::Jpeg,
        [0xff, 0xd9, 0xff, 0xd8, ..]                         => JpegTagFormat::Jpeg,  // 畸形头
        [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, ..] => JpegTagFormat::Png,   // \x89PNG\r\n\x1a\n
        [0x47, 0x49, 0x46, 0x38, 0x39, 0x61, ..]             => JpegTagFormat::Gif,   // "GIF89a"
        _ => JpegTagFormat::Unknown,
    }
}
```

纯字节前缀匹配。两点：

- **JPEG 那条多认一个 `FF D9 FF D8` 开头的畸形头**（对应 §3 说的那个组合直接出现在开头的情况）——比另外两个格式更宽容。
- ⚠️ **GIF 只认 `GIF89a`，不认 `GIF87a`**（`47 49 46 38 37 61`）。老 SWF 里确实有 GIF87a，会被判成 `Unknown` 从而解码失败。这是一个覆盖缺口。

---

## 5. `DefineBitsJPEG3` 的 alpha 合并

```rust
// decoder.rs:216-245
if let Some(alpha_data) = alpha_data {
    let alpha_data = decompress_zlib(alpha_data)?;

    if alpha_data.len() == decoded_data.len() / 3 {
        let rgba = decoded_data.chunks_exact(3).zip(alpha_data)
            .flat_map(|(rgb, a)| {
                // The JPEG data should be premultiplied alpha, but it isn't in some incorrect
                // SWFs (see #6893).
                // This means 0% alpha pixels may have color and incorrectly show as visible.
                // Flash Player clamps color to the alpha value to fix this case.
                // Only applies to DefineBitsJPEG3; DefineBitsLossless does not seem to clamp.
                let r = rgb[0].min(a);
                let g = rgb[1].min(a);
                let b = rgb[2].min(a);
                [r, g, b, a]
            })
            .collect();
        return Ok(Bitmap::new(…, BitmapFormat::Rgba, rgba));
    } else {
        error!("Size mismatch in DefineBitsJPEG3 alpha data");
    }
}
```

三个要点：

1. **alpha 是 zlib 压缩的**（SWF 规范里写作 "DEFLATE"，但实现上用的 `ZlibDecoder`）。
2. **`min(a)` 钳位**是 Flash 兼容性的兜底：规范说 JPEG3 的像素数据**已经预乘**了，但一些编码器给的是裸 RGB。此时 alpha 为 0 的像素会带着颜色"透出来"（预乘语义下应该被乘没）。`min(a)` 对**格式正确的数据是恒等操作**（预乘后的分量天然 ≤ alpha），对畸形数据是修复。
3. **尺寸校验是全量的**（`alpha.len() == decoded.len() / 3`），不是逐行的。所以"长度刚好对但内容错位"的 alpha 平面能通过检查。

**不对称性**：`DefineBitsLossless` **不做**这个钳位（注释明确说了）。同一份代码里两种位图走不同的 alpha 策略，只看行为很难猜出来——需要知道 Flash 自身的差异。

### 预乘的两套实现

```rust
// decoder.rs:337-344 —— f32 路径（PNG 的 Rgba 分支、GIF 用）
fn premultiply_alpha_rgba(rgba: &mut [u8]) {
    rgba.chunks_exact_mut(4).for_each(|rgba| {
        let a = f32::from(rgba[3]) / 255.0;
        rgba[0] = (f32::from(rgba[0]) * a) as u8;    // ← 截断，非四舍五入
        …
    })
}
```

而 PNG 的 `GrayscaleAlpha` 分支用的是**整数**运算（`decoder.rs:283-295`）：

```rust
let a = pixel[1];
let v = (u16::from(pixel[0]) * u16::from(a) / 255) as u8;
```

两条路径**不保证逐位一致**（浮点截断 vs 整数除法），虽然差最多 1。属于可以接受但不整洁的重复。

---

## 6. `DefineBitsLossless` 的 5 种格式

```rust
// decoder.rs:503-597
let mut decoded_data = decompress_zlib(&swf_tag.data)?;
let has_alpha = swf_tag.version == 2;
let out_data = match (swf_tag.version, swf_tag.format) { … };
Ok(Bitmap::new(…, BitmapFormat::Rgba, out_data))
```

**全部输出 RGBA**（没有 RGB 捷径），且**不做预乘、不做钳位**。

### `(1, Rgb15)` —— 15 位色，行填充 **2 字节**

```rust
let padded_width = (swf_tag.width + 0b1) & !0b1;
validate_size(swf_tag.width, swf_tag.height)?;
for _ in 0..swf_tag.height {
    for _ in 0..swf_tag.width {
        let compressed = u16::from_be_bytes([decoded_data[i], decoded_data[i + 1]]);
        let rgb5_component = |shift: u16| {
            let component = (compressed >> shift) & 0x1F;
            ((component * 255 + 15) / 31) as u8        // 5 位 → 8 位，四舍五入
        };
        out_data.extend([rgb5_component(10), rgb5_component(5), rgb5_component(0), u8::MAX]);
        i += 2;
    }
    i += (padded_width - swf_tag.width) as usize * 2;    // ← 注意 ×2
}
```

- 每行按**偶数个像素**对齐（`(w + 1) & !1`），跳过的字节数是 `(padded - w) * 2`——因为每像素 2 字节。
- `(c * 255 + 15) / 31` 是 5→8 位的**精确四舍五入**（31 个值均匀铺到 256 个值上，`+15` ≈ 半格）。

### `(1|2, Rgb32)` —— SWF 存的是 ARGB

```rust
for rgba in decoded_data.chunks_exact_mut(4) {
    rgba.rotate_left(1);            // A R G B → R G B A
    if !has_alpha { rgba[3] = u8::MAX; }
}
decoded_data                        // ← 原地复用
```

SWF 的 `Rgb32` 是 **ARGB** 排列，而输出要 RGBA，所以整体左旋 1 字节。v1 没有 alpha 通道，补 255。

这是三种格式里**唯一的原地操作**（复用 `decoded_data`，不分配新 `Vec`），也是唯一**没有调用 `validate_size`** 的分支。

### `(1|2, ColorMap8)` —— 调色板，行填充 **4 字节**

```rust
let padded_width = (swf_tag.width + 0b11) & !0b11;
let mut palette = Vec::with_capacity(num_colors as usize + 1);
for _ in 0..=num_colors {                        // ← 注意是 <=，读 num_colors+1 项
    let a = if has_alpha { decoded_data[i + 3] } else { u8::MAX };
    palette.push(Color { r: decoded_data[i], g: decoded_data[i+1], b: decoded_data[i+2], a });
    i += if has_alpha { 4 } else { 3 };
}
…
    let color = palette.get(entry).unwrap_or(if has_alpha { &Color::TRANSPARENT } else { &Color::BLACK });
    i += (padded_width - swf_tag.width) as usize;   // ← 注意不 ×2
```

- 每行按**4 的倍数**对齐（`(w + 3) & !3`），跳过的字节数是 `(padded - w)`——因为每像素 1 字节索引。
- **调色板是 `num_colors + 1` 项**（SWF 的约定），所以循环用 `0..=num_colors`。
- 调色板项 v2 是 4 字节（RGBA），v1 是 3 字节（RGB）。
- 越界索引**不 panic**，回退到透明（v2）或黑（v1）。

> ⚠️ **`Rgb15` 的行填充要 `×2`，`ColorMap8` 的不要**——这是两个分支间最经典的陷阱（每像素字节数不同）。这里的实现是对的，但如果要自己重写这段，务必注意。

### 其余组合

```rust
_ => return Err(Error::UnsupportedLosslessFormat(swf_tag.version, swf_tag.format)),
```

### `decompress_zlib`

```rust
// decoder.rs:600-609
let mut out_data = Vec::new();
let mut decoder = flate2::bufread::ZlibDecoder::new(data);
decoder.read_to_end(&mut out_data).map_err(|_| Error::InvalidZlibCompression)?;
out_data.shrink_to_fit();
Ok(out_data)
```

一个函数同时服务 `DefineBitsLossless` 的像素数据和 `DefineBitsJPEG3` 的 alpha 数据（两者都是 zlib）。`shrink_to_fit()` 是有意义的：SWF 位图动辄几十 MB，`Read` 的实现可能过度分配，解压后回收一次能省下可观内存。

---

## 7. 滤镜的两套类型

滤镜在两个阶段有两套表示：

```
swf::Filter（解析期，Box + Cow）  ──From──▶  Filter（filter.rs，owned）
                                                 │
                                          ──From──▶  AnimFilter（animation.rs，纯基元）
```

| 类型 | 位置 | 作用 |
|---|---|---|
| `swf::Filter` | 外部 crate | 标签解析产物 |
| `Filter` | `filter.rs` | **持有权包装**，让数据脱离被解析的 SWF 生命周期 |
| `AnimFilter` | `animation.rs` | **线格式**，bincode 可序列化的纯基元 |

`Filter` 本身**没有任何数学**——`scale` / `calculate_dest_rect` / `impotent` 全是转调 `swf` crate 的同名方法。它存在的唯一理由是 `swf::Filter` 内部是 `Box<...>` + `Cow`，需要一个 owned 版本才能存进 `DisplayObject.filters: Box<[Filter]>`。

`From<&swf::Filter> for Filter`（`filter.rs:52-75`）是唯一的桥梁，逐个 `filter.as_ref().to_owned()`。

> `GradientGlowFilter` 和 `GradientBevelFilter` 都包着 `swf::GradientFilter`——**两者数据完全一样**，区别只在枚举变体本身，靠它承载 glow / bevel 的语义。

### 三个方法是死代码

`Filter::scale`（`filter.rs:18`）、`Filter::calculate_dest_rect`（`filter.rs:30`）、`Filter::impotent`（`filter.rs:42`），以及 `AnimFilter::impotent`（`animation.rs:172`）**在本仓库里没有任何调用者**。

- `Filter::calculate_dest_rect` 是 `swf` crate 在 twips 空间的版本；实际在用的是 `animation.rs` 的 `filter_dest_rect`（像素空间、无 swf 依赖，[04 篇 §7](04-animation.md#7-filter_dest_rect--离屏纹理尺寸)）。
- `Filter::impotent` 里还留着一句 `// TODO: There's more cases here, find them!`，只处理了 `BlurFilter` 和 `ColorMatrixFilter`。

这些是**给下游渲染器预留的 API**——它们的存在本身就是一条关于"渲染器应该在哪里"的线索。

### `AnimFilter` 的线格式

8 个变体对应 8 种 SWF 滤镜。转换时的两个编码决定：

**① `num_passes` 被预先从 bitflags 里解出来**，存成独立的 `u8`：

```rust
// animation.rs:185-187
/// Pre-computed passes (1..15), NOT raw bitflags.
/// Set from `inner.num_passes()` during conversion.
pub num_passes: u8,
```

**② 但原始 `flags` 字节仍然保留**，因为运行时还需要 `knockout` / `inner` / `on_top` 这些位（`glow_modes_survive_encoding` 测试固定了 `flags == 0xe3` 能通过 bincode 往返）。

于是**两个字段有冗余**：趟数信息在 `flags` 里已经有一份。消费者如果要用 `flags`，必须知道各滤镜的 PASSES 位域位置**不一样**：

| 滤镜 | PASSES 所在位 | 其它位 |
|---|---|---|
| `BlurFilterFlags` | **bit 3–7**（`0b11111 << 3`） | 无 |
| `GlowFilterFlags` | bit 0–4 | `INNER_GLOW = 1<<7`，`KNOCKOUT = 1<<6`，`COMPOSITE_SOURCE = 1<<5` |
| `DropShadowFilterFlags` | bit 0–4 | `INNER_SHADOW = 1<<7`，`KNOCKOUT = 1<<6`，`COMPOSITE_SOURCE = 1<<5` |
| `BevelFilterFlags` | bit 0–3 | 上述 + `ON_TOP = 1<<4` |
| `GradientFilterFlags` | bit 0–3 | 同 Bevel |

**Blur 的趟数在 bit 3 起，其它在 bit 0 起**——这是最容易读错的一处。

### 数值编码

| 量 | 线格式 | 单位 |
|---|---|---|
| `blur_x` / `blur_y` | `i32` | **16.16 定点**（像素 × 65536） |
| `angle` / `distance` | `i32` | 16.16 |
| `strength` | `i16` | Fixed8 |
| 颜色 | 4 个 `u8` | RGBA |
| `ColorMatrixFilter` | `[f32; 20]` | 原样 |
| `ConvolutionFilter` | `Vec<f32>` + divisor / bias / default_color / flags | 原样 |

全部 `#[derive(Serialize, Deserialize)]`——**没有 `Option`、没有带载荷的枚举**，`Vec` 只用在渐变色标和卷积矩阵上，bincode 友好。

`AnimBlurFilter::impotent`（`animation.rs:191-193`）复刻了 `swf` crate 的判断：

```rust
self.num_passes == 0 || (self.blur_x <= 65536 && self.blur_y <= 65536)
```

即"趟数为 0，或两个方向的半径都 ≤ 1.0 像素"。

---

## 8. `bt601.rs` —— YUV SIMD（当前不可达）

`decoder.rs:1` 声明了 `pub(crate) mod bt601`，`bt601.rs` 提供：

```rust
pub fn yuv420_to_rgba(y: &[u8], chroma_b: &[u8], chroma_r: &[u8], y_width: usize) -> Vec<u8>
```

把**平面 YUV 4:2:0**（BT.601 有限范围）转成交错 RGBA8888，**每次 4 个像素**。

> ⚠️ 如前所述，**这个模块目前只被自己的测试触达**——`Bitmap::into_rgba` 的 YUV 分支不可达。以下内容对理解"如果要做视频/VP6 支持该怎么做"有价值，但不是当前的数据路径。

### 4 像素一条 SIMD 指令

```rust
let y  = i32x4::from([y[0] as i32, y[1] as i32, y[2] as i32, y[3] as i32]) - i32x4::splat(16);
let cb = i32x4::from([cb[0] as i32, cb[0] as i32, cb[1] as i32, cb[1] as i32]) - i32x4::splat(128);
let cr = i32x4::from([cr[0] as i32, cr[0] as i32, cr[1] as i32, cr[1] as i32]) - i32x4::splat(128);
```

- **一条 lane 一个像素**，用 32 位中间精度。注释说明了这么选的原因：*"so as to fill the 128-bit SIMD registers on WASM. And i32x4 also allows the neat transpose trick at the end."* —— 目标平台是 WASM，不是 x86。
- `-16` / `-128` 是**去掉有限范围的偏置**（Y 的范围是 16–235，Cb/Cr 是 16–240）。
- **色度的水平复制** `[cb0, cb0, cb1, cb1]` 就是 4:2:0 的上采样，而且是**最近邻、无插值**。源码注释直说了这是故意的：
  > *"The chroma_b and chroma_r samples are simply reused without any interpolation for all four corresponding pixels. This is not the most correct, or nicest, but it's what Flash Player does."*

### 16.16 定点系数

| 系数 | 值 | 来源 |
|---|---|---|
| `gray` | `76309` | `round((255.0/219.0) * 65536)` |
| `cr2r` | `104597` | `round((255.0/224.0) * 1.402 * 65536)` |
| `cr2g` | `-53279` | `round(-(255.0/224.0) * 1.402 * (0.299/0.587) * 65536)` |
| `cb2g` | `-25675` | `round(-(255.0/224.0) * 1.772 * (0.114/0.587) * 65536)` |
| `cb2b` | `132201` | `round((255.0/224.0) * 1.772 * 65536)` |
| `half` | `32768` | 0.5 in 16.16，用于右移时的舍入 |

`255/219` 和 `255/224` 是**有限范围 → 全范围的拉伸因子**，被融合进了 BT.601 系数里（省掉一次独立的缩放步骤）。`1.402` / `1.772` / `0.299` / `0.587` / `0.114` 是标准 BT.601 系数。

```rust
let r: i32x4 = (gray + cr2r + half) >> 16;
let g: i32x4 = (gray + cr2g + cb2g + half) >> 16;
let b: i32x4 = (gray + cb2b + half) >> 16;
```

`+ half` 之后算术右移 16 位 = **四舍五入（朝 +∞）**。最坏情况 `239*76309 + 112*104597 + 32768 ≈ 3.0e7`，**远在 i32 范围内**——所以不会溢出。

### clamp 的 `wide` API 陷阱

```rust
// A simple clamp(x, 0, 255) doesn't work, because it seems to operate on
// entire tuples, instead of each element separately.
let max = i32x4::splat(255);
let r = r.max(i32x4::ZERO).min(max);
```

源码注释记录了一个实际的 `wide` crate 行为坑：`clamp()` 在向量上的语义与预期不符，得自己用 `max` / `min` 组合。

### transpose 技巧

```rust
#[cfg(target_endian = "little")]
let rgba_4x = ((r) | (g << 8)) | ((b << 16) | (a << 24));
#[cfg(target_endian = "big")]     // 注释：I haven't tested this, but should work
let rgba_4x = ((r << 24) | (g << 16)) | ((b << 8) | (a));

rgba.copy_from_slice(bytemuck::cast::<i32x4, u8x16>(rgba_4x).as_array())
```

把四个通道**打包进一个 32 位字**（每个 lane 一个像素的 RGBA），再用 `bytemuck::cast` 把 128 位寄存器**重新解释成 16 个字节**。在小端序下，`[R0,G0,B0,A0, R1,G1,B1,A1, …]` 正好就是想要的交错布局——**一次 cast 完成了转置**。注释里感谢了 Lokathor（`bytemuck` 作者）提供这个技巧。

代价是字节序相关，所以有 `cfg` 分支，而且**大端那条分支明确标注未经测试**。

### 主循环与余数处理

```rust
for luma_rowindex in 0..y_height {
    let chroma_rowindex = luma_rowindex / 2;      // 4:2:0 垂直方向每 2 行亮度共 1 行色度
    let y_remainder = y_width % 4;
    let br_remainder = br_width % 2;              // 其中 br_width = y_width.div_ceil(2)
    …
    // 主循环只处理整块（4 像素 / 2 色度对）
    for (((y, cb), cr), rgba) in y_iter.zip(cb_iter).zip(cr_iter).zip(rgba_iter) { … }
    // 余数：把最后不足 4 个像素补齐成 4 lane 算一遍，只拷回有效字节
    if y_remainder != 0 { … }
}
```

- 用 `bytemuck::cast_slice` 而不是 `slice::array_chunks`（注释里有 `TODO` 等它稳定）。
- **色度迭代是"每 4 像素一个 `[u8;2]`"**，所以 `[cb0, cb0, cb1, cb1]` 的复制是免费的。
- 余数处理把**最后不足 4 个像素**当作一个完整 4-lane 运算跑一遍（未使用的 lane 读的是缓冲区尾部的数据），再只把有效输出字节拷回去。这要求 `y_width - y_remainder ≡ 0 (mod 4)` 才能保证绝对列号与组内下标一致——测试里用一张"最后一列倒置"的图来验证余数分支读的是**正确的色度行**。

### 测试与反函数

测试块（`bt601.rs:198-483`）里有一个 `#[cfg(test)]` 的**反函数** `rgb_to_yuv`，用的是**浮点 BT.601 全精度系数**，与正向路径的定点实现**刻意不同源**——这样往返测试才有意义。

测试用注释显式记录了**已知的 ±1 舍入误差**：

> ```
> // !!! there is a rounding error here
> ```

然后对 matplotlib "tab10" 调色板断言容差 ≤ 1。这种"承认误差并把它固定下来"的做法比追求完美对齐更务实。

> 测试注释里还有一句很坦诚的话，关于色度最近邻上采样在 1 像素宽的色度跳变上的表现：转换回 YUV 是 `(112, 97, 218)` 而不是期望的 `(125, 90, 240)`，但再转回 RGB 是 `(255, 51, 50)` vs `(255, 51, 49)`——*"So, close enough."*

---

## 9. 最终统一出口：WebP

不管来源是 SWF 位图还是渐变斜坡，**最终都编码成 WebP 存进 `TEXT`**：

```rust
// lib.rs:427-434（位图）
let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(w, h, bitmap_rgba.data().to_vec())
    .expect("Bitmap dimensions must match RGBA data");
let mut webp_buf = Cursor::new(Vec::new());
img.write_to(&mut webp_buf, ImageFormat::WebP).unwrap();
let webp_bytes = webp_buf.into_inner();
let (texture_offset, texture_length) = self.intern_texture(&webp_bytes);
```

```rust
// lib.rs:556-562（渐变斜坡）
fn encode_gradient_as_webp(gradient: &Gradient) -> Vec<u8> {
    let color = gradient.compute_gradient_color(256);
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(256, 1, color).unwrap();
    …write_to(…, ImageFormat::WebP)…
}
```

**收益**：运行时只需要一条纹理解码路径，不用同时带 JPEG / PNG / GIF / zlib / 调色板解码器。

**代价**：
- **有损**。`image` crate 的 WebP 编码默认是有损的，所以位图会经历一次 JPEG→RGBA→有损 WebP 的二次损失，渐变斜坡的 256 级色阶也可能产生色带。
- 转换耗时（每个唯一纹理一次 WebP 编码）。
- 渐变斜坡虽然只有 256×1，但走同一个编码器，开销不小。

把渐变斜坡也做成纹理（而不是传一组色标 uniform）是一个明确的取舍：**用一张 256 像素的小纹理换取着色器里零循环的查表**。对 GPU 来说这个交换通常是划算的。
