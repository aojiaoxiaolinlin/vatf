# vatf 技术文档

第一次使用请从 [项目 README](../README.md) 开始，运行命令和测试素材均可在本项目内找到。配套 Bevy 渲染器的效果展示与使用方法在 [bevy_flash_remake README](../../bevy_flash_remake/README.md)。

## 文档导航

| 文档 | 内容 |
|---|---|
| [01 · VAB 格式](01-format.md) | 文件头、chunk、POD 几何、材质与版本策略 |
| [02 · 转换流水线](02-pipeline.md) | 标签解析、显示列表构建与中间表示 |
| [03 · 几何处理](03-geometry.md) | 路径拼接、三角化、渐变与 morph |
| [04 · 时间轴与烘焙](04-animation.md) | 动作、子动画时序、皮肤、事件与遮罩 |
| [05 · 图像与滤镜参数](05-assets.md) | 位图解码、纹理、滤镜参数与范围计算 |
| [06 · 读取端](06-runtime.md) | VabReader、对齐、烘焙帧访问与播放语义 |
| [07 · 设计分析记录](07-design-notes.md) | 早期源码分析与设计取舍，含历史记录 |
| [08 · UI 与编译接口](08-ui-and-compilation.md) | 当前三种模式、导出 UI、按钮、资源裁剪和资产预处理 |

01–07 保留了较详细的源码分析与历史测量。其中代码摘录和行号是分析时的快照，不是完整 API 定义；当前使用契约以项目 README、08 及源码为准。历史体积测量也不代表当前版本的性能结果。

## 当前数据流

```text
SWF bytes + SwfCompileSettings
  -> 解析定义、显示列表与标签
  -> Animation: 根动作 / 事件 / 皮肤
     StaticUi / AnimatedUi: ExportAssets 及其依赖
  -> 路径三角化、morph / 静态字形处理、位图解码
  -> 烘焙帧与分组 / 遮罩 / 滤镜参数
  -> 从最终引用压缩资源并重建偏移
  -> VAB bytes + ResourcePruningReport
  -> VabReader -> 播放器 / 渲染器
```

## 源码入口

| 文件 | 职责 |
|---|---|
| `src/compiler.rs` | 共用编译设置、字节编译与文件接口、COMPILER_REVISION |
| `src/main.rs` | CLI 参数及批量转换 |
| `src/lib.rs` | 解析、资源构建、网格量化及 VAB 写入 |
| `src/animation.rs` / `src/baked.rs` | 编译期时间轴、最终烘焙树与校验 |
| `src/graphics.rs` | UI 导出依赖、动画周期、按钮状态及 UI 数据模型 |
| `src/pruning.rs` | 最终资源可达性、压缩与报告 |
| `src/reader.rs` | VAB 校验、对齐存储与读取接口 |

`AnimContainer` 只属于编译期；VAB 不包含旧的 ANIM chunk。运行时普通动画载荷是 BAKD，UI 另有可选 UIGR/UIBT。滤镜参数离线保留，实际滤镜绘制仍由渲染器执行。
