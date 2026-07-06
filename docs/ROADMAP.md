# ROADMAP — “一定程度直接取代 Photoshop” 路线（v0.2.0 之后）

> 交接文档：按顺序推进下列条目。每项都附实现要点与 `file:line` 锚点，
> 供新会话不重读全库即可开工。写于 2026-07-06，HEAD = `150c3f3`（已推送）。

## 当前状态（已完成，勿重做）

- **v0.2.0 已发布**（tag `v0.2.0` → `1bc57ff`，GitHub Release 带双 exe）。
- 其后又推送 `150c3f3`：缩放/平移（ViewXform 统一坐标）、手动局部 mask
  编辑器、交互式裁剪。**尚未发新 release**。
- 反推配方（`fit.rs` + CLI `match` + GUI 按钮）、gpt-image-2 弹性高分辨率
  （≤8.3MP + 400 回退）、风格提示词提取（`advisor::describe_style`）均已上线。
- GUI 生产化（直方图/toast/快捷键/拖拽/持久化/分组折叠/双击归零）已上线。
- **① 曲线编辑器已完成**：develop_panel「曲线 · Curves」，主/R/G/B 通道，
  直方图背景 + 点击加点/拖动移点/拖出删点；预览线直接采样公开的
  `render::curve_lut`（引擎同源）。下一项从 **②** 开始。
- 待用户真机验收：缩放/裁剪/mask 手感；持久化“正常关闭→重启恢复”回路；
  高分辨率生成与风格提示词的真实 API 行为（需付费调用，有 400 回退兜底）。

## 关键架构事实（新会话必读）

- 所有图上交互经 `ViewXform`（屏幕↔全幅归一化，gui.rs，struct 在
  `CROP_ASPECTS` 之后）；工具互斥分发在 `after_view`。
- `develop_preview`（render.rs）只跑 `apply_develop`，**不应用 WB/裁剪/旋转**；
  裁剪由 GUI 用 uv 窗显示、导出端 `render_to_image` 真裁。
- tone 模型单一事实来源：`render::TONE_KNOTS_X / tone_slider_basis /
  tone_exposure_curve`（pub(crate)，fit.rs 逆着它解）。
- `recipe.masks` 是 AI 与手动共用的同一列表；引擎 `apply_masks` 实时渲染
  tone+saturation+NR，clarity/dehaze/texture/temp/tint 仅进 XMP（GUI 已如实分组）。
- 照片库 `D:/Photography` 只读；输出一律 `./out`（`pipeline::guard_readonly`，
  项目自身 `./out` 永远可写）。

## ① 色调曲线交互编辑器（✅ 已完成）

- 数据已通：`recipe.tone_curve/red_curve/green_curve/blue_curve:
  Vec<CurvePoint{input,output: u8}>`（recipe.rs）；引擎组合方式——master 曲线
  在滑杆样条**之后**复合（`build_tone_lut` 末尾 `sample_lut(&curve, hermite_eval…)`），
  RGB 曲线在 master 之后（`apply_rgb_curves`）；分段线性 `interp`。
  XMP：`ToneCurvePV2012(+Red/Green/Blue)`（xmp.rs `curve_elem`）。
- GUI 设计：develop_panel 新 CollapsingHeader「曲线 · Curves」；通道选择
  （主/R/G/B）；自绘 widget：`allocate_exact_size(~边长 220)` + painter——
  网格、直方图背景（有 `self.histogram`）、曲线线条（按引擎同款 `interp`
  采样保真）、控制点拖拽（命中半径 ~8px）、空白处点击加点、拖出框外删点
  （LR 手势）、input 保持严格递增去重。改动 → `clamp()+dirty`。
- 无引擎改动。测试：曲线点排序/去重的纯函数可单测。

## ② 批量：配方复制 / 粘贴 / 同步

- GUI：gallery 支持 Ctrl+点击多选（现 `selected: Option<usize>` 单选，加
  `HashSet<usize>`）；按钮「复制配方」/「粘贴到选中(N)」。
- 粘贴 = 对每张写 `write_recipe` + `write_xmp`（./out，RAW 才有 XMP），
  可选跳过 crop/straighten（LR 同步对话框的简化版：一个 checkbox）。
  worker 线程跑批 + 状态/toast 汇报；沿用 `Msg` 通道模式。
- 不渲染成品（可选 flag 后续加）；库只读不变。

## ③ WB 吸管（有前置）

- **前置**：`develop_preview` 目前不应用 WB → GUI 里 Temp/Tint 是预览无感的。
  先在 develop_preview 接 `apply_wb`（as-shot 锚 5500K，与 render_to_image
  同款近似，render.rs `apply_wb/wb_gains/kelvin_to_rgb`）。
- 吸管：色调节加按钮进入取样模式 → 点击 `base_preview` 像素 → 数值反解：
  在 k∈[2000,40000]（对数网格）× tint 上搜索使 `wb_gains` 中和该像素
  r≈g≈b 的 (k, tint)，残差最小者写入 recipe。引擎一致（用我们自己的
  wb_gains 正向模型）。加单测：合成偏色像素 → 反解回中性。

## ④ 拉直（需引擎加旋转）

- 现状：`recipe.straighten_deg` 只进 XMP（`CropAngle`），引擎不旋转。
- 引擎：任意角双线性旋转 + 旋转后最大内接轴对齐矩形自动裁（闭式公式），
  在 `render_to_image` 裁剪前应用；GUI 预览在 `redevelop` 时旋转预览像素
  （1280px 每次配方变更一次，可接受）。
- GUI：裁剪节加 -10..10° 滑杆；后续可加“画地平线”手势。
- 测试：旋转 90°/0° 退化、自动裁边界、内接矩形公式。

## ⑤ 仿制图章（像素路径）

- 非 recipe：走 heal 的像素通路（retouch.rs，实现前先读）。
- GUI：Alt+点击取源点 → 画笔涂目标区 → 按偏移克隆 → 存 ./out 像素母版
  （同 heal，非 XMP）。与 ViewXform 兼容（画笔已走归一化坐标）。

## 完成每项后的例行动作

1. `cargo clippy --features gui --all-targets`（零警告）+ `cargo test --lib`
   + release build + GUI 启动烟雾。
2. 密钥扫描（`sk-[A-Za-z0-9]{20,}|OPENAI_API_KEY=|ANTHROPIC_API_KEY=`）后
   提交（结尾 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`），
   用户说 push 才推、说发布才发 release。
3. 攒够一批（如 ①②③）可提议发 v0.3.0。
