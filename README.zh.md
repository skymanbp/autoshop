# skymanbp 的 Autoshop

AI 辅助、确定性 RAW 处理：GPT 视觉顾问产生 Lightroom 风格的 EditRecipe，Rust 引擎从原始 RAW 确定性渲染出可复现的 16 位主图并写入 Lightroom 兼容的 XMP 副文件。

快速开始

```bash
# 构建
cargo build --release

# 端到端示例（构建后运行）
target/release/autoshop auto "photo.ARW" --guidance "warm, lift shadows"
```

核心功能

- 从原始 RAW 确定性渲染 → 16 位 TIFF
- GPT 视觉顾问输出有界的 EditRecipe（JSON），主流程 AI 不触碰像素
- 生成 Lightroom 兼容的 XMP，编辑可在 Lightroom/ACR 中调整
- 可选 AI 去噪、像素修复与生成式修图（均为显式启用）
- 桌面原生 GUI 与本地 Web UI
- 批量处理、风格检索、反向拟合（Look match）、评估工具

文档与支持

- 架构与设计：docs/ARCHITECTURE.md
- 去噪脚本：python/denoise.py
- 开发与测试：运行 `cargo test`

报告问题或请求功能

- 请新建 issue 并选择合适的模板（bug / feature）。
- 报告 bug 请附上重现步骤、最小输入图片或 EXIF、使用的命令与错误输出。

许可

本仓库使用 Apache-2.0 许可证。详见 LICENSE。
