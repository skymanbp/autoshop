# ROADMAP — “一定程度直接取代 Photoshop” 路线（v0.5.0 之后 · UX 阶段）

> 交接文档：每项都附实现要点与 `file:line` 锚点，供新会话不重读全库即可
> 开工。更新于 2026-07-07（**v0.5.0 已发布**；差距清单三大项 + UX 三批 +
> debug 清扫完成；**本地领先 origin 五个未推送提交**，见下）。

## 当前状态（已完成，勿重做）

- **本地未推送提交（v0.5.0 tag 之后，等用户说 push/发布）**：
  `763a2bc` UX批次1（蒙版覆盖叠加/削波警告/Esc）→ `eb6a098` 叠加参考缓存
  → `51c151d` UX批次2（hover 预览/直方图三角灯/批量进度条）→ `be60c52`
  UX批次3（蒙版⬆⬇排序/光标语言/缓存key收窄）→ `55e7e07` **debug 清扫**：
  ①方向统一——rawler ARW 内嵌预览不带 EXIF 转正（crate 源码实证），旧管线
  在 develop **之后**才 oriented() ⇒ 竖拍 RAW 的 crop/straighten 会错轴；
  现两侧都在最前端转正（引擎 `orient_f32` 复用同一 `oriented`，decode 端
  `preview_only`/`decode_raw` 同函数转正），Normal 方向逐位不变，回归
  测试+真 ARW 61MP 全流程实测；②hover_mask 改帧作用域（折叠面板/换图
  不再粘滞）。
- **v0.5.0 已发布**（tag `v0.5.0` → `3ab41b6`，双 exe 资产字节核对）：
  三大项整批——C2 手动畸变 / D2 P3+AdobeRGB 真 gamut 导出 / A② AI
  主体天空分割（位图 mask 通路 + python sidecar，用户真机实测）。
- **v0.4.0 已发布**（tag `v0.4.0` → `e175bf8`）：范围蒙版 / 双轨续接 /
  导出管线 / 高分预览 / 暗角补偿 / sRGB ICC / 版本快照——A-G 整批。
- **~~C2 手动畸变校正~~ ✅ 完成（2026-07-06 深夜，见 §C，提交 b623e5a）**
  ——坐标映射整体设计（original→corrected→view 三空间合约）+ 引擎径向
  重映射 + GUI 全调用点接入 + XMP，67 lib + 4 gui 测试。
- **~~D2 P3/AdobeRGB 输出~~ ✅ 完成（2026-07-06 深夜，见 §D）**——真
  gamut 变换（色度推矩阵 + 双 TRC）+ CC0 profile 双件 + GUI 色彩空间
  下拉 + Prefs，69 lib + 4 gui 测试。
- **~~A② AI 主体/天空分割~~ ✅ 完成（2026-07-07 凌晨，见 §A）**——
  引擎位图 mask 通路（MaskGeometry::Bitmap + 双线性采样 + XMP 跳过）+
  `python/segment.py` sidecar（subject=rembg U²-Net / sky=SegFormer
  ADE20K，实测均通）+ GUI 两键入口，72 lib + 4 gui 测试。
- **三大项至此全部触底。** 剩余工作 = 各节「未做/已知边界」小项（去紫边、
  Upright、lensfun、位图 overlay 半透明显示、tile 金字塔、水印等）+
  真机验收清单；无未开工的大工程。
- v0.3.0 → `fa9add8`，v0.2.0 → `1bc57ff`。
- **有序批次 ①-⑤ 全部完成**（详见各节 ✅ 小节，含实现锚点与已知近似）：
  ①曲线编辑器 ②批量复制/粘贴 ③WB 吸管（含 WB 预览前置重构）
  ④拉直（引擎真旋转+自动内接裁剪）⑤仿制图章（clone_raw 像素通路）。
- **差距批次 A① 亮度/颜色范围蒙版已完成**（见 §A ✅ 小节：recipe/render/
  xmp/gui/advisor 五层，60 lib + 4 gui 测试）。A②（主体/天空 AI 分割）
  未做——前置是引擎位图 mask 通路。
- **差距批次 B 双轨打通已完成**（见 §B ✅ 小节：母版路径入 GUI 态 +
  「⤴ 以此母版继续修图」保留配方续接）。
- **差距批次 F 导出管线已完成**（见 §F ✅ 小节：ExportOpts 长边/锐化/质量 +
  批量渲染 worker，61 lib 测试）。
- **差距批次 E 高分预览已完成**（见 §E ✅ 小节：1280/2560/4096 预览分辨率
  下拉，切换保配方重解码）。
- **差距批次 C 两片全部完成**（见 §C ✅ 小节）：暗角补偿（线性光域径向
  增益 + GUI 镜头校正区 + XMP VignetteAmount/Midpoint）+ C2 手动畸变校正
  （三空间坐标合约 + 引擎径向重映射 + GUI 映射链全接入 + XMP
  LensManualDistortionAmount，67 lib 测试）。
- **差距批次 D 第一步 导出嵌 sRGB ICC 已完成**（见 §D ◐ 小节：三格式
  显式编码器 + CC0 profile 入库，64 lib 测试）。
- **差距批次 G 版本快照已完成**（见 §G ✅ 小节：`<stem>.v<N>.recipe.json`
  编号快照 + 版本区存/载 UI）。
- 更早已上线：反推配方（`fit.rs` + CLI `match`）、gpt-image-2 弹性高分辨率
  （≤8.3MP + 400 回退）、风格提示词提取、GUI 生产化（直方图/toast/快捷键/
  拖拽/持久化/折叠分组/双击归零）。
- 待用户真机验收（v0.3.0 起累计）：曲线拖拽/吸管/图章/拉直/范围蒙版
  手感；「以此母版继续修图」链路（修补→动滑杆→再修补→导出）；导出长边/
  锐化/质量 + 批量渲染选中；预览 2560/4096 的滑杆延迟是否可接受；暗角
  补偿手感；版本快照存/载；导出 ICC 在广色域屏与真 LR 的显示；范围蒙版
  XMP 与 VignetteMidpoint 在真 Lightroom 打开的效果；持久化"正常关闭→
  重启恢复"；高分辨率生成与风格提示词的真实 API 行为（付费调用，有 400
  回退兜底）。**v0.5.0+UX 阶段新增待验**：AI 选主体/选天空按钮真机手感
  （GUI 内点击链路，sidecar 命令行已实测）；畸变滑杆与真 LR 同数值强度
  对比；P3/AdobeRGB 文件在广色域屏与印刷流程观感；蒙版覆盖叠加透明度
  （255,40,40 α≤140/255）与 hover 预览响应；削波三角灯灵敏度（任一像素
  即亮）；批量进度条；蒙版⬆⬇排序手感；**竖拍 ARW 全流程**（方向统一后
  显示/蒙版/裁剪/拉直应全部正确——修复靠单测+横拍实测，竖拍样张未过）。

## 关键架构事实（新会话必读）

- 所有图上交互经 `ViewXform`（屏幕↔全幅归一化，gui.rs）；工具互斥分发在
  `after_view`（crop > placing > wb_pick > range_pick > clone > paint >
  box-select）。
- **EXIF 方向在链条最前端**（55e7e07 起）：引擎 `orient_f32` 在 develop
  之前转正 f32 缓冲，decode 端 `preview_only`/`decode_raw` 用同一
  `render::oriented`（pub(crate)）转正内嵌预览——GUI 显示帧 == 引擎
  original 帧，任何 RAW 方向下蒙版/裁剪/拉直坐标一致。rawler 的 ARW
  内嵌预览本身**不带**转正（crate 源码实证）。
- `develop_preview`（render.rs）跑 `apply_recipe_wb` + `apply_develop`；
  **不应用裁剪**（GUI 用 uv 窗显示、导出端真裁）。**几何链**由 GUI `redevelop`
  在 develop_preview 之后依次调引擎 `apply_lens_distortion`（C2 畸变）→
  `rotate_straighten`（拉直）完成（导出路径同函数、同顺序）。
- **坐标空间约定（④起，C2 扩展）**：original →（畸变校正）→ corrected →
  （旋转+内接裁剪）→ view；`recipe.crop` 存 view 空间；masks/画笔/吸管/
  region 存 original 空间——gui.rs `view_norm_to_orig / orig_norm_to_view /
  geom_to_view`（三者带 `dist` 参数，来源 `geom_ctx`）在数据边界换算，共用
  引擎 `inscribed_dims / distort_norm / undistort_norm`，全零恒等。完整
  合约见 render.rs "Manual lens distortion" 注释块。
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

### A. 智能选区 / 范围蒙版（① ✅ 2026-07-06 · ② ✅ 2026-07-07）
- PS/LR：Select Subject / Sky、亮度/颜色范围蒙版。
- **① 亮度/颜色范围蒙版 ✅**：五层打通，权重 = 几何 × 范围（相交）。
  - recipe.rs：`RangeMask` 枚举（Luminance 4 数梯形 = ACR LumRange 原样；
    Color = 参考色 rgb + amount 容差 + px/py 取样点）+
    `LocalAdjustment.range: Option<RangeMask>`（serde default，旧 JSON 兼容）；
    clamp 强制梯形非降序。
  - render.rs：`range_weight`（亮度=梯形 ramp，退化边=阶跃；颜色=亮度不变
    色度距离，除以各自 luma 后欧氏距离，d_max = 0.15+0.9·amount）；
    apply_masks tone + NR 双 pass 相乘接入。
  - xmp.rs：`range_mask_xml` 第二组件 `Mask/RangeMask`，相交编码
    `BlendMode=1 + Inverted=true + Value=0`（从用户自己的 LR 边车
    `_DSC9245.xmp`/`_DSC9303.xmp` 解码验证的代数）。
  - gui.rs：选中 mask 面板「范围蒙版」下拉（无/亮度/颜色）；亮度=下限/上限/
    羽化三滑杆（GUI 对称羽化 ↔ recipe 4 数梯形）；颜色=色块 + 🎯 取样
    （`handle_range_pick`：pre-mask develop 的 5×5 均值，与引擎评估像素
    一致）+ 容差滑杆；`range_picking` 入工具互斥。
  - advisor：openai.rs 结构化 schema 加 `range`（anyOf 双变体 + null）+
    prompt 用法指引。
  - 已知近似：(a) 范围权重按"全局显影后、蒙版逐个叠加时"的像素评估——
    多 mask 叠加时后面的 range 看到前面 mask 的输出（LR 是固定参考图；
    全分辨率快照内存不可行，已注释）；(b) 颜色 PointModels 第 4-6 数
    按"取样点坐标+保留位"假设写出，未与真 ACR 对照语义；(c) 真 LR 打开
    效果待用户验收。
- **② 主体/天空 AI 分割 ✅（2026-07-07 凌晨）**：位图 mask 通路 + python
  sidecar 两层全通。
  - **位图通路**：recipe.rs `MaskGeometry::Bitmap { path }`（`kind`-tag 序列化，
    JSON 往返测试）；render.rs `load_mask_bitmap`（每 mask 每次 develop 解码
    一次，绝不进像素循环；缺文件=惰性 + stderr 警告）+ `sample_gray_norm`
    （归一坐标双线性 → 1280 mask 驱动 61MP 导出）+ `mask_weight` 第三臂，
    tone/NR 双 pass 共享；xmp.rs 位图 mask 跳过（经典 ACR XMP 无法表达；
    全位图时不发空壳块——参数 mask 照常写出，§B 式定位取舍）；GUI 列表
    「位图」标签、overlay 徽标（不假装形状）、重画按钮对位图隐藏。
  - **sidecar**（`python/segment.py` + `src/segment.rs` 桥，循 denoise.py
    模式；config `segment_script` / `AUTOSHOP_SEGMENT_SCRIPT`）：
    `--target subject` = rembg U²-Net 显著主体软 alpha（`pip install rembg`，
    模型首跑自动下载 ~/.u2net，176MB）；`--target sky` = SegFormer-B0
    ADE20K 天空类概率（transformers，~14MB 自动下载；sky 类号从模型
    id2label 解析、不硬编码）。缺依赖时 exit 2 + 打印确切 pip 命令。
  - **GUI**：局部调整区「🤖 AI 选主体」「☁ AI 选天空」→ worker 喂
    ORIGINAL 帧预览 → `./out/<stem>.mask-<target>.png`（同 target 重跑
    覆盖同文件）→ 推入 Bitmap mask 并选中，undo 一步回退；软 alpha 即
    天然羽化。
  - **实测（2026-07-07，用户环境 Python313）**：天空 = Lundy 真照片
    天侧均值 254/地侧 0；主体 = 合成主体中心 255/背景 0/覆盖 18.7%
    （与真实面积一致）；纯风景无主体时主体 mask 近空属模型正常行为。
    rembg 需装进 `python` 对应环境（用户机上 `pip`≠`python -m pip`，
    后者才对）。
  - 已知边界：mask 位图不进 XMP（LR 侧丢 AI 选区）；位图 overlay 暂为
    徽标而非半透明叠加显示；分割跑在预览分辨率（对羽化选区足够）。

### B. 像素母版 ↔ 参数配方双轨打通（✅ 已完成 2026-07-06）
- 现状（旧）：fill/heal/clone 输出 ./out 像素母版，仅在 After 显示一次；
  滑杆一动即 redevelop 回配方渲染，母版脱链。
- **已实现**（gui.rs）：`Msg::Retouched`（`RetouchDone` 别名）四条像素路径
  （fill/heal/clone/reimagine）都带回母版路径 → `self.master`；Retouch 面板
  顶部「⤴ 以此母版继续修图」→ `continue_from_master`：一次性 `keep_recipe`
  标志让下一个 `Msg::Opened` **保留当前配方**（母版是同帧中性显影+修补像素，
  滑杆/蒙版/裁剪/拉直 1:1 适用），src_path 重定向到母版 → 后续修图/导出
  都基于它。undo 历史在新 base 上重开；master 随换图清空；打开失败也会
  消费掉 keep_recipe（不泄漏到无关的下一次打开）。
- 边界：XMP 仍只随 RAW 源写出（母版是 PNG，只写 recipe json）——参数轨
  的 Lightroom 出口停在原 RAW 一侧，属定位内取舍。GUI 态逻辑无单测
  （egui app 态），入真机验收列表。

### C. 镜头/几何校正（✅ 暗角 + C2 手动畸变均完成 2026-07-06）
- **暗角补偿 ✅**：`recipe.lens_vignette / lens_vignette_mid`（-100..100 /
  0..100，clamp 齐全）；引擎 `apply_vignette`（render.rs）——**线性光域**
  径向增益 `1 + k·rⁿ`，midpoint 经指数 0.6..3.0 控制作用范围，apply_develop
  第 0 步（tone 前），预览/导出/母版三路径共享；GUI「镜头校正 · Lens」区
  两滑杆；XMP `VignetteAmount`（键名从用户 140 份真边车实证）+
  `VignetteMidpoint`（ACR 文档配对键，用户边车中无非零实例，语义待真 LR
  验证），amount=0 时零键写出（与旧 writer 字节兼容）。单测：中心不动/
  径向单调/负值压暗/高中点收缩作用域；XMP 条件写出。
- **手动畸变校正 ✅（C2，2026-07-06 深夜）**：`recipe.lens_distortion`
  （-100..100，ACR 语义：正修桶形、负修枕形）；引擎（render.rs）
  `distort_norm / undistort_norm / apply_lens_distortion`——半对角线归一的
  单系数径向模型 `r_src = s·r·(1+k(sr)²)`，`k = −amount/100·0.25`（|k|<1/3
  保单调可逆；方向经两条独立推导交叉验证），负 amount 走 Newton 填满缩放
  （无黑角，同拉直的 auto-fill 策略）、正 amount 角部内容自然裁出；逆映射
  Newton 求三次根、被裁内容钳到单调极限落在视野外。管线插入点：三条路径
  （RAW 导出/baked/GUI redevelop）统一 develop 之后、拉直之前。GUI 映射链
  `view_norm_to_orig/orig_norm_to_view/geom_to_view` 全部带 `dist` 项
  （wb 吸管/范围取样/画笔/mask 放置/region/克隆 全调用点接入），镜头面板
  第三滑杆；XMP `LensManualDistortionAmount`（键名从用户 148 份真边车实证，
  仅非零写出）。已知近似：amount→k 增益是我方标定（Adobe 未公开），同数值
  下 LR 的校正强度可能不同——入真机验收单。单测：映射双向 roundtrip
  （4 幅度）/方向性/双符号无黑角/中心不动点/内容径向外移。
- **未做**：per-lens profile 校正（lensfun / 厂商 k1+k2 多项式——手动滑杆
  已覆盖目测校正，按镜头自动化留长期项）；去紫边（需边缘邻近门控，防误伤
  紫色主体）；透视 Upright。
- AI advisor 暂不暴露镜头字段（校正是测量性操作，非审美建议；schema 未加）。

### D. 色彩管理（✅ sRGB ICC + D2 广色域输出均完成 2026-07-06）
- **导出嵌 ICC ✅**：`render_to_file` 三种格式全部显式编码器 + `tag_icc`
  （render.rs，原 tag_srgb 泛化）——JPEG=APP2 ICC_PROFILE 段、PNG=iCCP 块、
  TIFF=tag 34675；profile 用 saucecontrol/Compact-ICC-Profiles
  （**CC0-1.0 公有领域**，assets/ 下入库；下载时验证 acsp 签名 +
  repo license API 实证）。单测逐格式验证 marker 字节存在。image 0.25.10
  三个编码器的 `set_icc_profile` 实现已核对（真存储非 Unsupported）。
- **D2 P3/AdobeRGB 输出 ✅（2026-07-06 深夜）**：`ExportColorSpace`
  {Srgb, DisplayP3, AdobeRgb} 入 `ExportOpts`（默认 Srgb，旧调用零变化）；
  **真 gamut 变换**（render.rs `convert_export_color_space`）——解 sRGB
  TRC → 线性光 3×3 原色变换 → 目标 TRC（P3 同 sRGB 曲线；AdobeRGB 纯
  563/256 gamma）；矩阵**运行时从原色色度推导**（`rgb_to_xyz`/`inv3`，
  不手抄七位小数表），三空间共 D65 白点、无色适应项；白点保持单测端到端
  锁定推导。profile：`DisplayP3-v2-magic.icc`（736 B）+
  `AdobeCompat-v2.icc`（374 B），下载时同样验 acsp+尺寸。GUI 导出面板
  「色彩空间」下拉（sRGB/Display P3/Adobe RGB），入 Prefs（越界回落
  sRGB）。未知扩展名（无法带 tag 的格式）刻意留 sRGB——P3/AdobeRGB 数值
  不带 profile 到处都显示错。单测：白/灰/中性保持、逆矩阵 roundtrip、
  sRGB 红在 P3 内（正 g/b）/在 AdobeRGB 是重缩放纯红（共享红原色）、
  JPEG/TIFF 文件字节含完整目标 profile。
- **未做**：egui 显示端色管（上游限制）；retouch 母版 PNG 的 ICC（工作
  文件，导出时会再过 render_to_file 补 tag）；工作空间本身仍是 sRGB
  （引擎在更宽空间显影是另一级大工程，超出导出选项范畴）。

### E. 1:1 真像素检查（✅ 已完成 2026-07-06）
- 现状（旧）：预览固定 1280px（gui.rs `PREVIEW_EDGE`），「1:1」= 预览像素。
- **已实现**（gui.rs）：标题行 Fit/1:1 旁新增预览分辨率下拉
  （1280 流畅 / 2560 / 4096 检查），入 `Prefs` 持久化（恢复时白名单校验，
  防坏存档造出 1px/100MP 预览）；`open_path` 按选中值缩放工作预览；
  **切换即重解码当前照片且配方保留**（复用批次 B 的 `keep_recipe` 通路），
  busy 时下拉禁用。代价如实标注：2560/4096 下每次滑杆调整变慢
  （develop_preview 逐像素成本 ×4/×10）。
- 未做（大工程，暂缓）：全分辨率 tile 金字塔（真 61MP 1:1 平滑漫游）。

### F. 导出管线（✅ 已完成 2026-07-06）
- 现状（旧）：`render_to_file` 只出全分辨率 16-bit TIFF / q95 JPEG；
  批量只同步配方不出图。
- **已实现**：
  - 引擎（render.rs）：`ExportOpts { long_edge, sharpen, jpeg_quality }` 作
    `render_to_file` 第 5 参（`Option`，None=旧行为，main.rs/serve.rs 7 个
    调用点传 None）；顺序=重采样（Lanczos3，永不放大）→ 输出锐化
    （luma unsharp r=1，在**缩放后**像素上）→ 按质量编码；返回保存后尺寸。
    单测锁定：50 长边出 50×25 且文件实测一致、超源尺寸不放大、q30<q95。
  - GUI（gui.rs）：导出区新增 长边下拉（原尺寸/1600/2048/2560/3840/5120）+
    输出锐化滑杆 + JPEG 质量滑杆（选 JPEG 时显示）；三项进 `Prefs` 持久化
    （Prefs 补 `serde(default)`+手写 Default 对齐 app 默认，旧存档不失效）；
    单张 Export/Download 与批量共用 `export_opts()`。
  - **批量渲染**（gui.rs `start_batch_render`）：gallery 多选 →「🖼 渲染
    选中(N)」——每张读它自己的 `./out/<stem>.recipe.json`（无则中性显影）
    按当前格式+导出选项出 `./out/<stem>.developed.*`；单 worker 顺序跑
    （61MP 全幅并行只会抖内存）；汇总成功/失败走 toast。AI Denoise 明确
    不参与批量（GPU sidecar 每张数分钟）。
- 未做（定位内暂缓）：水印、导出预设、色彩空间选项（后者归 §D）。

### G. 历史/版本（✅ 已完成 2026-07-06）
- 现状（旧）：undo/redo 100 步（内存态，关会话即失）；./out recipe json 单份。
- **已实现**（gui.rs）：版本快照 = `./out/<stem>.v<N>.recipe.json`（编号
  递增，不碰工作用 `<stem>.recipe.json`，库只读不变）；develop 面板
  「版本 · Versions」区——「＋ 存为版本」写下一号快照，列表每行「载入」
  替换当前参数（走 dirty→redevelop，撤销一步回到载入前）；列表缓存于
  `self.versions`，照片打开/存版时 `refresh_versions` 重扫（不逐帧扫
  ./out）；载入时 clamp() 防手改 JSON 越界。
- 未做（内存 undo 持久化到磁盘的完整历史——快照已覆盖"多套参数并存"
  的主需求，全量历史留给需要时再做）。

### UX 批次（用户指定方向 2026-07-07 起：UI 与操作细节）
- **第一批 ✅（2026-07-07）**：
  1. **蒙版覆盖叠加显示**（LR 的 O 叠加）——引擎新增 `render::mask_coverage`
     （与 apply_masks 完全同源：geometry×inversion×amount×range，range 在
     masks-cleared develop 参考上求值，单测锁定）；GUI 选中蒙版即显示红色
     半透明覆盖层，经畸变+拉直同一几何链落到 view，随滑杆/选中/O 键实时
     刷新。**同时关闭了"位图蒙版只有徽标"的 A② 已知边界**——位图/参数/
     范围蒙版统一走真实权重显示。开销如实：叠加开启+选中蒙版时每次滑杆
     调整多跑一次 masks-cleared develop（1280 下无感，4096 下可 O 关掉）。
  2. **削波警告**（LR 的 J）——红=任一通道 ≥254 溢出、蓝=全通道 ≤1 死黑，
     按**显影后导出像素**判定；标题行 ▲ 按钮 + J 键，入 Prefs 持久化。
  3. **Esc 统一退出工具**——裁剪/放置蒙版/WB 吸管/范围取样/图章/画笔/
     框选一键全退（画布与取样点保留可续）。
- **第二批 ✅（2026-07-07）**：
  4. **蒙版行 hover 即预览覆盖**——鼠标悬停蒙版列表任意行，图上即显示
     该蒙版的覆盖（不必点选；移开回落到选中项）。靠第一批的参考缓存，
     hover 切换只重算轻量覆盖图。
  5. **直方图削波三角灯**（LR 同款）——直方图左上/右上三角：暗部/亮部
     极值 bin 有像素时点亮（蓝/红），干净时灰；点击 = 切换 J 叠加，
     与 ▲ 按钮/J 键三入口同一状态。
  6. **批量渲染进度条**——worker 逐张上报 `Msg::BatchProgress`，顶栏
     实时进度条 + 状态行计数（此前只有结束 toast，跑长批像卡死）。
  - 滑杆微调核实为 egui 原生已有（点数值可键入、拖数值即精调），不另做。
- **第三批 ✅（2026-07-07）**：
  7. **叠加参考缓存失效收窄**——cache key 里中和 straighten/distortion/
     crop（develop_preview 不读它们，由调用方在其后应用；lens_vignette
     保留因为它是 develop 阶段）——拖拉直/畸变滑杆不再无谓重建参考。
  8. **工具光标语言补全**——平移=抓手（按住空格=Grab/拖动=Grabbing）、
     画笔/放置蒙版/裁剪=十字线；WB/范围/图章取源原有十字线保留。
  9. **蒙版 ⬆⬇ 排序**——蒙版顺序是渲染语义（顺序叠加，后面的范围蒙版
     看到前面的输出），选中行两键上移/下移，动了即 redevelop。
- 待做候选（未开工）：直方图通道分开的削波三角（现为任一通道点亮）；
  蒙版真拖拽排序（现为按钮，egui dnd 待评估）；裁剪角柄的方向光标。
- 快捷键现状：Ctrl+Z/Y/O/E/S、←/→ 走图、B 对比、O 叠加、J 削波、Esc 退出、
  空格/中键平移、滚轮缩放、双击 Fit↔1:1、滑杆双击归零/点值键入。

### 明确不追（定位外）
- 图层/混合模式/智能对象、文字/矢量、设计合成——PS 的另一半；
  reimagine/fill + 反推配方已覆盖摄影侧的"创意改图"。

### 建议批次顺序（v0.3.x 起 · 2026-07-06 收官状态）
~~A①（范围蒙版）~~ ✅ → ~~B（双轨打通）~~ ✅ → ~~F（导出管线）~~ ✅ →
~~E（高分预览）~~ ✅ → ~~C（暗角 + C2 手动畸变）~~ ✅ →
~~D（sRGB ICC + D2 广色域输出）~~ ✅ → ~~G（版本）~~ ✅；
剩 A②（AI 分割）——待引擎位图 mask 通路，是差距清单最后一个大项。

## 完成每项后的例行动作

1. `cargo clippy --features gui --all-targets`（零警告）+ `cargo test --lib`
   + release build + GUI 启动烟雾。
2. 密钥扫描（`sk-[A-Za-z0-9]{20,}|OPENAI_API_KEY=|ANTHROPIC_API_KEY=`）后
   提交（结尾 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`），
   用户说 push 才推、说发布才发 release。
3. 攒够一批（如 ①②③）可提议发 v0.3.0。
