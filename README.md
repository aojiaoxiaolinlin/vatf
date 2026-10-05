# vatf

将 SWF 转换成离线烘焙的 **VAB** 动画或矢量 UI 资源。Rust 库名为 `vatf`，命令行工具与库共用同一套编译器。

转换器提前展开显示列表、普通子 Sprite 时间轴、变换和 morph 网格。播放器按帧读取绘制树，不再运行 SWF 时间轴或 ActionScript。滤镜仍保留为参数，由渲染器在运行时执行。

## 快速开始

需要 Rust 工具链；当前开发环境为 Rust 1.95、edition 2024。以下命令在本项目根目录执行，使用已包含的 [测试素材](fixtures/README.md)，无需安装相邻游戏项目。

```sh
# 普通动画
cargo run --release -- fixtures/spirit2159src.swf -o target/examples/spirit2159src.vab

# 静态矢量 UI：导出名 button_background
cargo run --release -- fixtures/ui_demo.swf --ui -o target/examples/ui_demo.vab

# 有子动画的矢量 UI：导出名 sparkles
cargo run --release -- fixtures/background551284.swf --ui-animated -o target/examples/background551284.vab

# 原生 DefineButton 按钮：导出名 login_button
cargo run --release -- fixtures/login.swf --ui -o target/examples/login.vab
```

`-o` 在单文件模式下是完整输出文件名；省略时输出到源文件旁的同名 `.vab`。输入目录时，转换其第一层 `.swf` 文件，默认输出到该目录下的 `output/`。不递归，也不会自动判断每个文件应使用哪种模式；不同类型的 UI 与动画应分开转换。

## 选择编译模式

| 模式 | CLI | 用途 |
|---|---|---|
| `Animation`（默认） | 不加选项 | 根时间轴动作、帧事件、命名皮肤 |
| `StaticUi` | `--ui` | ExportAssets 指定的纯矢量 Shape、静态 Sprite 和原生按钮 |
| `AnimatedUi` | `--ui-animated` | 同样的导出机制，允许普通子 Sprite 循环动画 |

`--ui` 与 `--ui-animated` 互斥。UI 以 ExportAssets 名称引用，例如配套 Bevy 插件加载 `ui.vab#button_background`；类型从 SWF 定义推断，无需给名字添加 `symbol_` 或类型前缀。

## 动画制作约定

- 根 MC 就是动画根。转换器不会寻找主动画、补根帧或自动对齐子动画时长；源文件应先完成帧提取和时长对齐。
- 根级每个非 `event_` 标签划分一个动作。`anim_idle` 暴露为 `idle`，`ATTACK` 保留为 `ATTACK`；无动作标签时生成覆盖根时间轴的 `default`。
- 第一个动作从第 0 帧开始，动作名称和起始帧不得重复。动作覆盖到下一个标签之前，最后一个到根时间轴末尾。
- 有动作标签的资源，每帧最多一个根控制对象。编译器抵消每个动作首个非空帧的根对象放置平移，保留缩放、旋转和之后的相对运动。这不是脚底锚点推断；无标签的一般场景保留原坐标。
- `event_hit` 成为动作局部帧上的 `hit` 事件。事件响应、播放队列、fallback、死亡动画终态等属于播放器 API。
- 皮肤通过 `skin_<slot>` 实例名标记 Sprite；该 Sprite 内唯一、非空的帧标签直接作为变体名，无需 `variant_` 前缀。没有标签的帧不参与选择，变体中的普通子动画冻结为静态快照。
- SWF 原点和 Y 向下的坐标仍是资源坐标；Bevy 实体位置、镜像、舞台适配由播放器处理。

## 矢量 UI

静态模式要求导出的 Sprite 及其依赖没有动画；动画模式允许子时间轴，离线计算循环周期并展开全部帧。多个持续循环子时间轴用最小公倍数合并，父时间轴重置子实例时仍遵循实际放置时序，最大周期为 **4096 帧**，超出会报错。没有手填播放长度参数。

UI 帧共用固定的几何范围和中心，避免逐帧重新居中造成抖动；滤镜扩展的最终绘制范围由渲染器计算。原生按钮的 `up / over / down` 保持共同的布局范围，`hit` 是命中测试几何，既不是按下状态也不是 focus。

UI 是严格的纯矢量导出：位图填充（包括描边）、静态文本、morph、脚本等不受支持；可编辑文字被省略，不会变成可编辑输入框。普通动画模式另有静态 DefineText/DefineText2 字形展开功能。具体限制和按钮状态标签见 [UI 与编译接口](docs/08-ui-and-compilation.md)。

## 作为 Rust 库调用

```rust,no_run
use vatf::{SwfCompileMode, SwfCompileSettings, compile_swf, reader::VabReader};

fn main() -> anyhow::Result<()> {
    let source = std::fs::read("fixtures/ui_demo.swf")?;
    let settings = SwfCompileSettings { mode: SwfCompileMode::StaticUi };
    let compiled = compile_swf(&source, &settings)?;
    std::fs::write("ui.vab", &compiled.bytes)?;

    let reader = VabReader::from_bytes(&compiled.bytes)?;
    for graphic in reader.graphics()? {
        println!("{}: {} frames", graphic.name, graphic.frames.len());
    }
    println!("meshes: {} -> {}", compiled.pruning.before.meshes,
        compiled.pruning.after.meshes);
    Ok(())
}
```

`convert_swf(input, output, settings)` 是文件接口，返回裁剪报告。已有的 `convert_swf_to_vab`、`convert_swf_ui_to_vab`、`convert_swf_animated_ui_to_vab` 保留 `Result<()>` 返回值；对应的 `*_with_report` 返回 `ResourcePruningReport`。

## 资源裁剪与资产预处理

UI 在三角化前只选择 ExportAssets 及其依赖。写 VAB 前，所有模式再次从最终烘焙引用裁剪资源，重新排列网格、顶点、索引、材质和纹理偏移。所有动作帧、所有皮肤变体、遮罩和 UI 状态都是保留根，不根据当前显示状态裁剪。报告中的资源字节数不等于整个 VAB 文件大小。

Bevy 的 `VabAssetProcessorPlugin` 直接调用 `compile_swf`，不启动 CLI，也不依赖临时文件。元数据使用同一个 `SwfCompileSettings` 选择编译模式；处理后的 `.swf` 路径保存 VAB 字节及加载器元数据，`.swf#export` 仍可加载。开发时启用处理器，发布时分发处理产物并关闭处理器。完整接入代码见 [配套插件 README](../bevy_flash_remake/README.md)。

**当前 VAB 尚未发布，工作版本为 1。** 开发期间可以原地修改格式，但必须一起重新生成已有 VAB；相同版本号不能识别所有旧布局。正式发布后，不兼容改动需要提高格式版本。`COMPILER_REVISION` 是独立的编译器行为修订号，编译结果语义变化时递增，配套插件用它隔离预处理缓存。详见 [版本策略](docs/01-format.md#8-版本与兼容性)。

## 验证与性能测试

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench --bench bench_read
```

真实 SWF oracle 测试使用项目内 `fixtures/spirit2159src.swf`，每次生成临时 VAB；素材缺失会使测试失败，不会静默跳过。UI 测试还在代码中构造小型 SWF，覆盖导出验证、子动画周期、按钮和裁剪等行为。

读取基准在计时前从同一份 SWF 编译当前格式的 VAB，测量 `VabReader::from_bytes`，不把 SWF 编译耗时计入读取耗时。它不测 Bevy 加载、GPU 渲染或滤镜性能。

## 渲染效果

以下图片来自配套 Bevy 渲染器的真实 GPU 验证输出；vatf 自身只做转换，不负责绘制。图片与素材均保存在本项目内。

| 动画 | 静态名称框 |
|---|---|
| ![Flash 动画与滤镜](docs/images/spirit2159src.png) | ![矢量名称框](docs/images/nameplate.png) |

![循环矢量背景的一帧](docs/images/animated-background.png)

| 按钮普通状态 | 悬停状态 | 按下状态 |
|---|---|---|
| ![up](docs/images/button-up.png) | ![over](docs/images/button-over.png) | ![down](docs/images/button-down.png) |

图片来源与更新方式见 [效果图说明](docs/images/README.md)。

## 文档与效果展示

- [技术文档目录](docs/README.md)：格式、解析、三角化、烘焙、读取端及 UI 编译接口。
- [配套插件用法与真实渲染效果](../bevy_flash_remake/README.md)：动画、静态 UI、动态背景与按钮展示。
- [素材说明](fixtures/README.md)：用途、来源与导出名。

这是视觉动画转换器，不是完整 Flash 虚拟机。ActionScript、交互行为、声音及动态文本运行时不在此项目范围内；混合和滤镜的最终支持程度取决于使用 VAB 的渲染器。测试素材仅用于开发验证，包含素材不代表提供原作品的授权。
