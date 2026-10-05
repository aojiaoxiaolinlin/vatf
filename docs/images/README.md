# 渲染效果图

这些 PNG 从 bevy_flash_remake 的 docs/images 原样复制，来自该项目真实 GPU 测试输出，未裁剪、调色或合成。透明背景保留；动态资源只展示一帧，不构成像素一致性或性能证明。

| 文件 | GPU 测试 / 原始输出 |
|---|---|
| `spirit2159src.png` | `spirit_frames_to_png` / `spirit2159src_0009.png` |
| `nameplate.png` | `nameplate_ui_renders_without_editable_text` / `vab_nameplate_ui.png` |
| `animated-background.png` | `background551284_export_animates_and_renders` / `background551284_408.png` |
| `button-up.png`、`button-over.png`、`button-down.png` | `native_login_button_states_render_different_pixels` / 三种可见状态 |

vatf 不包含 GPU 渲染器。需要更新截图时，在接口匹配的配套插件项目运行对应 `cargo test --test render_gpu <测试名> -- --ignored --nocapture --test-threads=1`，核对输出后复制到本目录。普通 vatf 测试与转换命令不需要这些 GPU 测试或相邻项目。

相关源 SWF 已包含在本项目 fixtures：角色、名称框、循环背景、登录按钮。素材仅用于开发验证，不能据此推定拥有原作品授权。
