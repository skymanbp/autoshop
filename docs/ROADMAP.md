# ROADMAP — “一定程度直接取代 Photoshop” 路线（v0.2.0 之后）

> 交接文档：每项都附实现要点与 `file:line` 锚点，供新会话不重读全库即可
> 开工。更新于 2026-07-06，HEAD = `997de65`（已推送，local==origin）。

## 当前状态（已完成，勿重做）

- **v0.2.0 已发布**（tag `v0.2.0` → `1bc57ff`，GitHub Release 带双 exe）。
  其后 `150c3f3`（缩放/mask/裁剪）与 **①-⑤ 整批 + 差距调查**
  （`006b7d4`/`c0ac3e7`/`35a39e6`/`124f994`/`4154534`/`997de65`）均已推送，
  **尚未发新 release**——内容已够 v0.3.0，等用户说"发布"。
- **有序批次 ①-⑤ 全部完成**（详见各节 ✅ 小节，含实现锚点与已知近似）：
  ①曲线编辑器 ②批量复制/粘贴 ③WB 吸管（含 WB 预览前置重构）
  ④拉直（引擎真旋转+自动内接裁剪）⑤仿制图章（clone_raw 像素通路）。
- 更早已上线：反推配方（`fit.rs` + CLI `match`）、gpt-image-2 弹性高分辨率
  （≤8.3MP + 400 回退）、风格提示词提取、GUI 生产化（直方图/toast/快捷键/
  拖拽/持久化/折叠分组/双击归零）。
- **下一批按 §与 Photoshop 的核心差距 的顺序：A 范围蒙版 → B 双轨打通 →
  F 导出管线 → E 高分预览 → C 镜头校正 → D 色管 → G 版本。**
- 待用户真机验收：曲线拖拽/吸管/图章/拉直手感；持久化"正常关闭→重启恢复"；
  高分辨率生成与风格提示词的真实 API 行为（付费调用，有 400 回退兜底）。

## 关键架构事实（新会话必读）

- 所有图上交互经 `ViewXform`（屏幕↔全幅归一化，gui.rs）；工具互斥分发在
  `after_view`（crop > placing > wb_pick > clone > paint > box-select）。
- `develop_preview`（render.rs）跑 `apply_recipe_wb` + `apply_develop`；
  **不应用裁剪**（GUI 用 uv 窗显示、导出端真裁）。**拉直**由 GUI `redevelop`
  在 develop_preview 之后调引擎 `rotate_straighten` 完成（导出路径同函数）。
- **坐标空间约定（④起）**：`recipe.crop` 存拉直后空间；masks/画笔/吸管/
  region 存原始空间——gui.rs `view_norm_to_orig / orig_norm_to_view /
  geom_to_view` 在数据边界换算，共用引擎 `inscribed_dims`，0° 恒等。
- tone 模型单一事实来源：`render::TONE_KNOTS_X / tone_slider_basis /
  tone_exposure_curve`（pub(crate)，fit.rs 逆着它解）；曲线采样单一事实来源
  `render::curve_lut`（pub，GUI 曲线编辑器直接画它）。
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

## ② 批量：配方复制 / 粘贴 / 同步（✅ 已完成）

- GUI：gallery 支持 Ctrl+点击多选（现 `selected: Option<usize>` 单选，加
  `HashSet<usize>`）；按钮「复制配方」/「粘贴到选中(N)」。
- 粘贴 = 对每张写 `write_recipe` + `write_xmp`（./out，RAW 才有 XMP），
  可选跳过 crop/straighten（LR 同步对话框的简化版：一个 checkbox）。
  worker 线程跑批 + 状态/toast 汇报；沿用 `Msg` 通道模式。
- 不渲染成品（可选 flag 后续加）；库只读不变。

## ③ WB 吸管（✅ 已完成，含前置）

- **前置已做**：新共享阶段 `apply_recipe_wb`（render.rs，apply_wb 旁）接入
  develop_preview / render_to_image / render_baked_to_image 三条路径；
  `temperature_k.is_some() || tint != 0` 即生效（修复了 tint 单独无效的旧坑）。
- 吸管已做：`render::solve_wb_from_neutral`（对数网格扫 K 使 r≈b，绿残差
  解析出 tint，与 `wb_gains` 同一正向模型）；GUI 色调区「💧 吸管」按钮 +
  图上点击取 5×5 均值（取 base_preview 的 pre-develop 像素）。
  单测：合成偏色像素 → 反解中和（<2% 残差）+ 预览 WB 生效性。

## ④ 拉直（✅ 已完成）

- 引擎：`render::rotate_straighten`（顺时针、双线性、16-bit）+ 公开的
  `render::inscribed_dims`（闭式最大内接矩形），在两条导出路径的用户裁剪
  **之前**、orientation 之后应用；GUI `redevelop` 用同一函数旋转预览。
- 坐标空间约定（重要）：`recipe.crop` 存**拉直后**空间（导出旋转后裁剪，
  裁剪工具无需映射）；masks/画笔/吸管/region 存**原始**空间——gui.rs 的
  `view_norm_to_orig / orig_norm_to_view / geom_to_view` 在数据边界换算
  （共用引擎 inscribed_dims，0° 恒等，roundtrip 有单测）。
- 已知近似（待真 LR 验证）：angle≠0 且带 crop 时 XMP 的 CropLeft…/CropAngle
  组合语义与我们的"先转后裁"是否逐像素一致未对照过真实 ACR 边车。

## ⑤ 仿制图章（✅ 已完成）

- 引擎：`HealSpot.clone_raw`（跳过 heal 的边界色调匹配 = 原样搬运 + 羽化）
  + `retouch::clone_stamp(src, mask, source_norm, full_res, out)`——涂抹 blob
  → spots，每个 spot 的供体偏移 = 源点 − blob 中心（PS 非对齐取样）。
- GUI：Retouch「仿制图章」节——进入图章模式，Alt+点击取源（十字标记，
  存原始帧坐标），共用画笔涂目标，「⎘ 克隆已涂区域」worker → ./out
  像素母版（同 heal，非 XMP）。单测锁定 clone（原样）vs heal（色调匹配）
  的语义差异。
- 已知近似：拉直角≠0 时画笔 overlay 纹理按原始帧直贴（落点计算正确，
  显示未旋转）——heal/clone/fill 共同的显示级问题，engine 结果不受影响。

## 与 Photoshop 的核心差距（2026-07-06 调查 · ①-⑤ 完成后）

> 定位前提：目标是"日常出片替代"（LR/ACR + PS 修图子集），不是 PS 的
> 设计/合成全集。按对日常出片的影响排序；「现状」均为当日代码实测。

### A. 智能选区 / 范围蒙版（差距最大）
- PS/LR：Select Subject / Sky、亮度/颜色范围蒙版。
- 现状：mask 仅线性/径向两种几何（recipe.rs `MaskGeometry`；xmp.rs 只写
  `Mask/Gradient` + `Mask/CircularGradient`，全库 grep 无 RangeMask 实现）；
  画笔手涂只喂 fill/heal/clone，进不了 recipe。
- 路径：① 亮度/颜色范围蒙版——`apply_masks` 加逐像素权重项，XMP 有
  CorrectionRangeMask 约定，**低成本高价值，建议下一批第一项**；
  ② 主体/天空 AI 分割（python sidecar 循 SCUNet 模式或云 API），前置是
  引擎加位图 mask 通路。

### B. 像素母版 ↔ 参数配方双轨打通
- 现状：fill/heal/clone 输出 ./out 像素母版，仅在 After 显示一次；滑杆一动
  即 redevelop 回配方渲染，母版脱链（继续编辑需手动打开 ./out png）。
- 路径：「以此母版继续 develop」一键（母版设为新 base + src 重定向）。低成本。

### C. 镜头/几何校正
- 现状：零实现（全库无 distortion/vignetting/CA/perspective 代码；EXIF 只存
  镜头名 decode.rs:28）。
- 路径：手动滑杆先行（k1/k2 径向畸变、暗角补偿、去紫边），透视 Upright 后置；
  lensfun 数据库长期项。

### D. 色彩管理
- 现状：全程 sRGB gamma（render.rs 管线），导出不嵌 ICC，egui 显示端无色管
  （广色域屏会偏饱和）。
- 路径：先导出嵌 sRGB profile，再谈 P3/AdobeRGB 输出选项；显示端受 egui 限制。

### E. 1:1 真像素检查
- 现状：预览 1280px（gui.rs `PREVIEW_EDGE`），「1:1」= 预览像素，查合焦/噪点
  不够。
- 路径：高分预览开关（2560/4096）成本最低；全分辨率 tile 金字塔是大工程。

### F. 导出管线
- 现状：`render_to_file` 只出全分辨率 16-bit TIFF / q95 JPEG，无重采样/输出
  锐化/水印/预设；批量只同步配方不出图。
- 路径：导出对话框（长边像素+锐化+质量）+ 批量渲染 worker。

### G. 历史/版本
- 现状：undo/redo 100 步（内存态，关会话即失）；./out recipe json 单份。
- 路径：recipe 快照（`<stem>.v2.json`…）+ 版本切换 UI（≈LR 虚拟副本）。

### 明确不追（定位外）
- 图层/混合模式/智能对象、文字/矢量、设计合成——PS 的另一半；
  reimagine/fill + 反推配方已覆盖摄影侧的"创意改图"。

### 建议批次顺序（v0.3.x 起）
A（范围蒙版）→ B（双轨打通）→ F（导出管线）→ E（高分预览）→
C（镜头校正）→ D（色管）→ G（版本）。

## 完成每项后的例行动作

1. `cargo clippy --features gui --all-targets`（零警告）+ `cargo test --lib`
   + release build + GUI 启动烟雾。
2. 密钥扫描（`sk-[A-Za-z0-9]{20,}|OPENAI_API_KEY=|ANTHROPIC_API_KEY=`）后
   提交（结尾 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`），
   用户说 push 才推、说发布才发 release。
3. 攒够一批（如 ①②③）可提议发 v0.3.0。
