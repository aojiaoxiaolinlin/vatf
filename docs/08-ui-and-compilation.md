# 08 · UI 与共用编译接口

本篇描述当前契约。基础命令见 [项目 README](../README.md)，二进制细节见 [01 · 格式](01-format.md)。

## 三种入口，共用一套编译器

```rust
pub enum SwfCompileMode { Animation, StaticUi, AnimatedUi }
pub enum RootTranslationPolicy { Preserve, NormalizeClipStart }
pub struct SwfCompileSettings {
    pub mode: SwfCompileMode,
    pub root_translation: RootTranslationPolicy,
}
pub struct CompiledVab {
    pub bytes: Vec<u8>,
    pub pruning: ResourcePruningReport,
}
```

`SwfCompileSettings::default()` 选择 Animation + Preserve。设置支持 serde，未知字段会报错。`compile_swf(&[u8], &settings)` 不读写文件；`convert_swf(&Path, &Path, &settings)` 创建输出父目录并写入文件，返回裁剪报告。CLI 和 Bevy 处理器使用相同入口。

普通动画保留根动作与事件、预展开普通 Sprite、共享命名皮肤变体。UI 模式使用 ExportAssets 导出库，源根舞台不是 UI 资产自身的显示内容。

## 导出名称与类型

ExportAssets 的名称是稳定标识，不以 Character ID 对外命名。可以在 JPEXS 添加或修改导出条目，将 Shape、Sprite 或原生 Button 的 Character ID 关联到名称。修改后保存 SWF，再转换即可。

导出名必须非空、没有 `#` 或控制字符、不能使用加载器保留的 `__vab/` 前缀，也不能重复。不要依赖内部 mesh/material 标签命名。Bevy 使用 `ui.vab#button_background`，预处理则使用 `ui.swf#button_background`，无需 `symbol/` 中间层。

| SWF 定义 | 导出结果 |
|---|---|
| DefineShape | 命名矢量图形 |
| DefineSprite | 静态或烘焙循环图形，由编译模式决定 |
| DefineButton / DefineButton2 | 命名按钮，关联状态图形 |

## 纯矢量约束

StaticUi 要求导出及其普通 Sprite 依赖为静态；AnimatedUi 才允许普通子动画。两者都拒绝引用位图填充（含描边）、morph、静态文本、脚本及不受支持的嵌套按钮等内容，不会隐式栅格化成 PNG。

可编辑文本 DefineEditText 被省略，不能导出为有效 UI 图形，也不会提供输入控件。包含名称框的资源应在 Bevy 用 Text/Input 控件补上文字。动画模式的静态 DefineText/DefineText2 字形展开是另一个功能，不改变 UI 的纯矢量限制。

## 动态 UI 周期与固定布局

根 Sprite 可以只有一帧，子 Sprite 做循环闪光。AnimatedUi 会沿显示列表分析可达时间轴：持续播放的兄弟子动画用最小公倍数合并周期，子实例的相位仍取决于放置帧；多帧父时间轴循环重置子实例时遵循父时间轴。动画不是简单地把所有定义的帧数相乘。

周期最多 4096 帧；超过限制会报错，建议在制作端统一循环长度或拆分资源。未引用的 Sprite 不参与周期计算。皮肤选择与根动作 API 不用于 UI 循环。

`Graphic` 保存名称、原始几何 `source_bounds = [xmin, ymin, xmax, ymax]`、完整帧树及帧率。所有帧共用一个几何范围中心，变换固定；不按当前帧范围重新居中。滤镜影响的绘制范围、离屏分辨率、UI 排版和 GPU 缓存由渲染器负责。Bevy 插件可以让多个 ImageNode 共享栅格化输出，并分别进行等比布局。

## 原生按钮状态

`Button` 保存名称和 `up / over / down / hit_test` 状态图形引用。

| SWF 状态 | 含义 | 生成的图形名 |
|---|---|---|
| up | 普通状态 | `<name>/up` |
| over | 指针悬停 | `<name>/over` |
| down | 按下 | `<name>/down` |
| hit | 命中区域 | `<name>/hit`（可选） |

over/down 缺失时回退到 up。可见状态使用共同的几何范围和注册中心，切换不会因各状态尺寸不同重新定位；hit 使用同一个注册坐标，但保留自己的范围。

hit 不绘制为按钮状态，也不表示 focus。配套 Bevy 插件使用原生交互状态；精确矢量 hit 命中、键盘 focus、按钮声音和 ActionScript 行为不是转换器功能。

用 Sprite 的不同帧表达按钮状态与原生 Button 是不同结构；当前不会仅凭帧标签自动推断出 Button。不要把根动作标签切分规则套到导出 UI 上。

## 二进制扩展与读取

VAB 基础 chunk 包含 BAKD 和几何/材质/纹理资源；UI 增加可选的 bincode 编码 UIGR（图形帧）与 UIBT（按钮状态引用）。普通动画不需要这些 UI chunk。

```rust,no_run
# fn main() -> anyhow::Result<()> {
let reader = vatf::reader::VabReader::open("ui.vab")?;
let graphics = reader.graphics()?;
let buttons = reader.buttons()?;
# Ok(())
# }
```

不存在的可选 chunk 返回空 Vec；存在但损坏或不符合契约会返回错误。bincode 依赖双方一致的数据 schema，并非自描述、跨任意版本兼容的格式。

## 两阶段资源裁剪

1. UI 解析 ExportAssets 的依赖闭包，只为需要的源定义构建图形，减少无关定义的三角化开销。
2. 写 VAB 前，从最终 BAKD、所有 UI 帧与按钮状态的引用出发压缩资源。

普通动画的所有动作、所有皮肤变体、所有帧，UI 的所有导出、按钮 hit、遮罩和分组内部节点都属于保留根。不会依据当前皮肤、alpha=0 或相机可见性删资源。

压缩重新排列 mesh、VERT、INDX、GRAD、BMAP、TEXT 和 morph 几何偏移，同时更新相关查找区间；Character ID 与烘焙节点引用不变。一个依赖被多个导出共享时不会重复复制。报告的 before/after 包含资源计数与逻辑资源载荷字节，不包含整个文件的 chunk 头和全部烘焙树体积；源侧提前剔除的定义也不等于报告里最终压缩的差值。

## Bevy 预处理与发布

配套插件的可选 `asset_processor` feature 开启 Bevy 后台处理器；应用使用 AssetPlugin 的 Processed 模式并注册 VabAssetProcessorPlugin。元数据选择同一套 SwfCompileSettings，缺省为 Animation。

处理器把源 SWF 转成 VAB bytes，同时生成 VabLoader 元数据。它不修改逻辑资产路径，所以 UI 子资产仍通过 `.swf#export` 加载。正式应用可以不编译处理器 feature，只分发已处理字节和元数据。

Bevy 的源内容/元数据哈希不能感知编译器代码变化。`COMPILER_REVISION` 在编译行为变化时递增，配套 `vab_processed_asset_path` 为缓存路径加修订命名空间。VAB_VERSION 则是文件格式版本，当前未发布工作版本为 1；开发阶段原地改 schema 后仍须重编译全部产物。两者职责不同。

## 根平移策略更新（2026-10-07）

默认编译保留原始根平移（`RootTranslationPolicy::Preserve`）。上述自动归零说明
仅适用于显式选择 `NormalizeClipStart` 的动作素材表；CLI 使用
`--normalize-clip-start`，共享 `SwfCompileSettings` 使用 `root_translation` 字段。
设置缺省字段兼容旧元数据，但缺省语义现在为保留。UI 导出和无标签场景不受影响。
编译器修订号更新为 2，VAB 格式版本仍为 1。
