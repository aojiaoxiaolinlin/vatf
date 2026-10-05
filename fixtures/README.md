# 测试与文档素材

这些 SWF 随项目保存，测试、读取基准和 README 命令不依赖外部盘符或相邻项目的 assets 目录。生成的 VAB 放到 `target/examples/`，不维护一套容易过时的二进制测试产物。

| 文件 | 用途 / 编译模式 | 已知导出名 |
|---|---|---|
| `spirit2159src.swf` | Animation；真实 SWF oracle、读取基准 | 不需要 UI 导出 |
| `wu_kong.swf` | Animation；命名皮肤手动验证素材 | 不需要 UI 导出 |
| `ui_demo.swf` | StaticUi；最小矢量图形 | `button_background` |
| `animated_ui.swf` | AnimatedUi；最小子动画 | `sparkles` |
| `nameplate3.swf` | StaticUi；名称框装饰、忽略可编辑文字 | `name_kuang` |
| `background551284.swf` | AnimatedUi；真实循环背景 | `sparkles` |
| `login.swf` | StaticUi；原生按钮状态 | `login_button` |

来源：前两份复制自开发环境原 bevy_flash 项目的 assets；其余复制自配套 bevy_flash_remake 的 assets，使用该项目已经整理的测试版本。不要用另一个未经对齐或导出标记的同名 SWF 替换它们。真实游戏素材仅用于开发回归与示例验证，使用者需自行确认原作品授权；没有因复制而取得额外授权。

`tests/ui_graphics.rs` 的核心行为测试还使用代码构造的独立小型 SWF，不依赖这些真实游戏 UI 素材。
