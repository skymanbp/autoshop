//! Zero-dependency i18n for the native GUI (English skeleton · Chinese overlay).
//!
//! English is the SKELETON: every user-facing string literal in `gui.rs` is
//! passed to [`tr`] AS its English text, which doubles as the lookup key.
//! [`Lang::En`] returns that key verbatim (no table walk); [`Lang::Zh`] looks it
//! up in the single [`ZH_ENTRIES`] catalogue and FALLS BACK to the English key
//! when a translation is missing — so an un-translated string renders in English
//! rather than blank. That is the whole mechanism: no external crate, no
//! codegen, one catalogue to maintain (the project's "language version control").
//!
//! Runtime interpolation ([`trf`]): Rust's `format!` requires a compile-time
//! literal format string, so a *translated* (runtime) string can't be handed to
//! it. Instead callers pass named placeholders (`{name}`) plus their
//! substitutions and `trf` does a plain string replace — identical behaviour in
//! English and Chinese, and the placeholder order is free to differ per language.
//!
//! This file is a PRIVATE submodule of `gui.rs` (`mod i18n;`), not a binary —
//! see `autobins = false` in Cargo.toml for why that distinction matters.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The UI language. `En` is both the default and the skeleton (see module docs).
/// Persisted in `Prefs` (eframe storage); a save from an older build that
/// predates this field decodes to `En` via `#[serde(default)]`.
#[derive(Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    /// Native display name for the language picker (never translated).
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }
}

/// Translate a skeleton (English) string. `En` returns it verbatim; `Zh` looks
/// it up in [`ZH_ENTRIES`], falling back to the English key when untranslated.
pub fn tr(lang: Lang, en: &'static str) -> &'static str {
    match lang {
        Lang::En => en,
        Lang::Zh => zh_map().get(en).copied().unwrap_or(en),
    }
}

/// Translate + interpolate. `args` are `(name, value)` pairs; each `{name}`
/// placeholder in the (possibly translated) string is replaced by `value`.
/// Used for every string that was a `format!(…)` before i18n: `format!` needs a
/// compile-time literal, so a translated string is filled by plain replacement.
/// A placeholder a translation happens to drop is simply left as-is (visible),
/// never a panic.
pub fn trf(lang: Lang, en: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = tr(lang, en).to_string();
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

/// Lazily materialise the English→Chinese lookup from the flat [`ZH_ENTRIES`]
/// slice (built once, on the first `Zh` translation).
fn zh_map() -> &'static HashMap<&'static str, &'static str> {
    static ZH: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    ZH.get_or_init(|| ZH_ENTRIES.iter().copied().collect())
}

/// THE single translation catalogue — "language version control" lives here.
/// English skeleton key → Chinese value, grouped by UI region. Add a pair when a
/// new English string is introduced; a key with no pair falls back to English.
/// Keys MUST match the English literal at the `tr`/`trf` call site byte-for-byte
/// (placeholders included), or the lookup silently misses.
#[rustfmt::skip]
static ZH_ENTRIES: &[(&str, &str)] = &[
    // ── Settings ────────────────────────────────────────────────────────────
    ("Language", "语言"),
    ("Saved to autoshop.local.json (gitignored, stays on this machine). Applies to the next Analyze.",
        "已保存到 autoshop.local.json（已 gitignore，仅存于本机）。下次分析时生效。"),
    ("Reverse-fit", "反推 / Reverse-fit"),
    ("Zoned fit (sky)", "分区反推：天空 / Zoned fit (sky)"),
    ("On reverse-fit, auto-split the sky on both sides and colour-correct sky↔sky separately (exposure / recolour gains / saturation, bitmap mask). Masks are rendered by the local engine; the LR sidecar carries only the global part. Needs the python segmentation deps (transformers + torch); falls back to pure global reverse-fit when unavailable, noting it in the rationale.",
        "反推时自动分割两侧天空，天空↔天空单独校色（曝光/重着色增益/饱和，位图蒙版）。蒙版由本机引擎渲染；LR sidecar 只携带全局部分。需要 python 分割依赖（transformers + torch）；不可用时自动退回纯全局反推并在理由里说明。"),
    ("Analysis — the verifier", "分析 · 校验器"),
    ("Provider", "提供方"),
    ("Model", "模型"),
    ("Base URL", "基础 URL"),
    ("API Key", "API 密钥"),
    ("key set — blank keeps it", "已设密钥 — 留空则保留"),
    ("no key set", "未设密钥"),
    ("Image — the vision proposer + generative edits", "图像 · 视觉提案 + 生成式编辑"),
    ("OAuth (Codex bridge / ChatGPT sub)", "OAuth (Codex 桥 / ChatGPT 订阅)"),
    ("fetching…", "拉取中… / fetching…"),
    ("🔄 Fetch models", "🔄 拉取可用模型 / Fetch models"),
    ("List the models this endpoint serves (GET /models) so you can pick instead of guess — and a live reachability check for the bridge/API. Uses the key/token typed below, or the saved one if blank.",
        "列出该端点提供的模型（GET /models），可挑选而非猜测 —— 并对桥接/API 做一次连通性检查。使用下方输入的密钥/令牌；留空则用已保存的。"),
    ("{chat} chat · {image} image", "{chat} 对话 · {image} 图像"),
    ("Bridge URL", "桥接 URL"),
    ("Vision model", "视觉模型"),
    ("Image-gen model", "生图模型"),
    ("Gate token", "网关令牌"),
    ("set — blank keeps it", "已设 — 留空则保留"),
    ("the bridge's own api-keys token (loopback, not a cloud key)", "桥接自身的 api-keys 令牌（回环地址，不是云端密钥）"),
    ("OAuth rides your ChatGPT subscription via the local Codex bridge — no OpenAI key. Start the bridge first (else edits fail to connect). Generative output is capped at ~1.5 MP by the subscription image tier; for full-resolution edits switch to API mode with a real key.",
        "OAuth 通过本地 Codex 桥使用你的 ChatGPT 订阅 —— 无需 OpenAI 密钥。请先启动桥（否则编辑无法连接）。生成输出受订阅图像档位限制，上限约 1.5 MP；需全分辨率编辑请切换到 API 模式并用真实密钥。"),
    ("Tip: gpt-image-1.5 keeps the photo most faithful (input_fidelity); newer models like gpt-image-2 ignore that lock and edit more freely.",
        "提示：gpt-image-1.5 对照片最忠实（input_fidelity）；gpt-image-2 等较新模型会忽略该锁定、编辑更自由。"),
    ("Save settings", "保存设置"),
    ("saved → {path}", "已保存 → {path}"),
    ("save failed: {err}", "保存失败: {err}"),

    // ── Local adjustments (masks) ────────────────────────────────────────────
    ("Linear", "线性"),
    ("Radial", "径向"),
    ("Bitmap", "位图"),
    ("mask", "蒙版"),
    ("Sky (reverse-fit)", "天空（反推）"),
    ("Land (reverse-fit)", "地景（反推）"),

    // ── Gallery / Library ─────────────────────────────────────────────────────
    ("Library", "图库 · Library"),
    ("Open folder…", "打开文件夹…"),
    ("{dir} · {count} photos", "{dir} · {count} 张照片"),
    ("⎘ Copy recipe", "⎘ 复制配方"),
    ("Copy every develop setting from the current photo", "复制当前照片的全部 develop 参数"),
    ("Recipe copied — Ctrl+click to pick several, then “Paste to selected”", "配方已复制 — Ctrl+点击选多张，再「粘贴到选中」"),
    ("⇩ Paste to selected ({n})", "⇩ 粘贴到选中({n})"),
    ("Writes a ./out recipe JSON for each; RAW also gets an XMP sidecar. Leaves library files untouched, renders nothing.",
        "对每张写 ./out 配方 JSON；RAW 同时写 XMP 边车。不动库文件、不渲染成品"),
    ("🖼 Render selected ({n})", "🖼 渲染选中({n})"),
    ("Each renders by its own ./out recipe (neutral develop if none) → ./out/<name>.developed.*, using the current format / long-edge / sharpening / quality; AI Denoise sits out the batch.",
        "每张按它自己的 ./out 配方出图（没有配方则中性显影）→ ./out/<名>.developed.*，用当前格式/长边/锐化/质量；AI Denoise 不参与批量"),
    ("Clear selection", "清除多选"),
    ("Include crop / straighten when pasting", "粘贴时含裁剪/拉直"),
    ("Off by default — composition rarely transfers between photos", "默认不带几何 — 构图在照片间通常不可复用"),
    ("Open a folder to browse your photos here.", "打开一个文件夹，在此浏览照片。"),
    ("✓ selected", "✓ 选中"),
    ("● edited", "● 已编辑"),

    // ── Retouch (reimagine / fill / heal / clone) ─────────────────────────────
    ("Retouch", "修饰 · Retouch"),
    ("Reimagine (whole image)", "整图 AI 生成 · Reimagine"),
    ("✨ Generate image", "✨ AI 生成出片"),
    ("Repaint the whole image with gpt-image (uses the Direction text above as the style). Repainted pixels = not faithful; the result is added as an 「AI generated」 variant at the bottom and switched to, so you can keep tweaking without reverting. Models that accept any size (gpt-image-2) reach ~8MP, others ~1.5K. Needs OPENAI_API_KEY.",
        "用 gpt-image 直接重绘整张图（拿上方 Direction 文本当风格描述）。重绘像素=非保真；生成后自动加入底部「AI 生成」变体并切过去，可继续微调不会变回去。支持任意尺寸的模型（gpt-image-2）可达 ~8MP，其余 ~1.5K。需 OPENAI_API_KEY。"),
    ("Generate an image first and stay on that variant to reverse-fit its recipe.",
        "先「AI 生成出片」并停在该变体上，才能反推它的配方。"),
    ("🎛 Reverse-fit recipe → sliders/XMP", "🎛 反推配方 → 滑杆/XMP"),
    ("Statistical fit: reverse the freshly generated look into editable develop params (local, no API cost). Sliders update (undoable), and for RAW an XMP is written to ./out; hit Save to render the full-resolution result.",
        "统计拟合：把刚生成的观感反解成可编辑的 develop 参数（本地运算，无 API 费）。滑杆会更新（可 undo），RAW 同时写 ./out XMP；再点 Save 可出全分辨率成品。"),
    ("📝 Extract style prompt", "📝 提取风格提示词"),
    ("Compare the original / generated images and have the vision model write a reusable style prompt: auto-fills Direction (ready to Reimagine other photos) and saves ./out/<stem>.style.txt.",
        "对比 原图/生成图，让 vision 模型写一段可复用的风格 prompt：自动填入 Direction（可直接给别的照片 Reimagine 用）并存 ./out/<stem>.style.txt。"),
    ("Uses the Direction above as the style. After generating, use 「Reverse-fit recipe」 to turn the look into sliders + XMP (the full-resolution way).",
        "拿上方 Direction 当风格描述。生成后可「反推配方」把观感变成滑杆+XMP（全分辨率的正道）。"),
    ("Paint mask", "涂抹蒙版"),
    ("Brush over the area; box-select is paused while on. Shared by Fill and Heal.",
        "在区域上涂抹；开启时框选暂停。Fill 与 Heal 共用。"),
    ("Clear", "清除"),
    ("brush", "画笔"),
    ("Generative Fill", "生成填充 · Generative Fill"),
    ("what belongs there, e.g. remove the trash can, extend the sky",
        "那里该有什么，例如：移除垃圾桶、延展天空"),
    ("Full-res", "全分辨率"),
    ("Composite onto the full-sensor develop (slow, RAW only)", "合成到全画幅显影上（慢，仅 RAW）"),
    ("Remove / Fill", "移除 / 填充"),
    ("Paint the area, write what belongs there, then Remove/Fill. Needs OPENAI_API_KEY.",
        "涂抹区域，写下那里该有什么，再点 Remove/Fill。需 OPENAI_API_KEY。"),
    ("Heal (pixel)", "去瑕疵 · Heal（像素）"),
    ("✦ AI heal (auto)", "✦ AI 去瑕疵 (auto)"),
    ("Heal painted area", "修复涂抹区域"),
    ("AI auto-detects dust / blemishes, or paint a mask and Heal it. Pixel retouch from surrounding pixels; saved to ./out.",
        "AI 自动识别灰尘/瑕疵，或涂抹蒙版后修复。按周围像素做像素级修饰；存 ./out。"),
    ("Clone Stamp", "仿制图章 · Clone Stamp"),
    ("✅ Done", "✅ 完成"),
    ("🖊 Enter stamp", "🖊 进入图章"),
    ("Stamp: Alt+click to set the source → brush the target area → 「⎘ Clone painted area」",
        "图章：Alt+点击取源点 → 画笔涂目标区 → 「⎘ 克隆已涂区域」"),
    ("⎘ Clone painted area", "⎘ 克隆已涂区域"),
    ("Clone on the full-resolution develop (slow, RAW only)", "在全分辨率显影上克隆（慢，仅 RAW）"),
    ("Photoshop-style clone stamp: Alt+click to sample a source (cross marker), brush the area to cover, and pixels are carried over as-is from the source (feathered edges, no tone matching). Local compute, saves a ./out pixel master.",
        "Photoshop 的仿制图章：Alt+点击取源（十字标记），画笔涂要覆盖的区域，按源点原样搬运像素（羽化边缘、不做色调匹配）。本地运算，存 ./out 像素母版。"),

    // ── Develop · shared slider helper ───────────────────────────────────────
    ("double-click resets", "双击归零 / double-click reset"),

    // ── Develop · panel + Tone & WB ──────────────────────────────────────────
    ("Develop", "显影 · Develop"),
    ("Tone & WB", "色调 · Tone & WB"),
    ("Custom white balance (off = as-shot)", "自定义白平衡（关=按拍摄值）"),
    ("💧 Click in image…", "💧 点击图中…"),
    ("💧 Eyedropper", "💧 吸管"),
    ("Click a spot in the image that should be neutral grey/white to auto-solve Temp/Tint (same forward model as the engine). Click again to cancel.",
        "点击图中应为中性灰/白的位置，自动解算色温/色调（与引擎同一正向模型）。再次点击取消。"),
    ("WB eyedropper: click a spot that should be neutral grey/white", "白平衡吸管：点击应为中性灰/白的区域"),
    ("Temp (K)", "色温 (K)"),
    ("Tint", "色调"),
    ("Exposure", "曝光"),
    ("Contrast", "对比度"),
    ("Highlights", "高光"),
    ("Shadows", "阴影"),
    ("Whites", "白色"),
    ("Blacks", "黑色"),

    // ── Develop · Presence / Detail ──────────────────────────────────────────
    ("Curves", "曲线 · Curves"),
    ("Presence", "质感 · Presence"),
    ("Clarity", "清晰度"),
    ("Dehaze", "去朦胧"),
    ("Vibrance", "自然饱和度"),
    ("Saturation", "饱和度"),
    ("Detail", "细节 · Detail"),
    ("Sharpening", "锐化"),
    ("Noise Reduction", "降噪"),

    // ── Develop · Color Mixer (HSL) + Grading ────────────────────────────────
    ("Color Mixer (HSL)", "颜色混合器 · HSL"),
    ("↺ reset all", "↺ 全部重置"),
    ("Hue", "色相"),
    ("Luminance", "明度"),
    ("Color Grading", "颜色分级 · Grading"),
    ("Blending", "混合"),
    ("Balance", "平衡"),
    // HSL_BANDS labels (Color Mixer band picker).
    ("Red", "红"),
    ("Orange", "橙"),
    ("Yellow", "黄"),
    ("Green", "绿"),
    ("Aqua", "青"),
    ("Blue", "蓝"),
    ("Purple", "紫"),
    ("Magenta", "洋红"),
    // GRADE_REGIONS labels (Color Grading region picker).
    ("shadow", "阴影"),
    ("midtone", "中间调"),
    ("highlight", "高光"),
    ("global", "全局"),

    // ── Develop · Crop + Lens ────────────────────────────────────────────────
    ("Crop", "裁剪 · Crop"),
    ("⛶ Enter crop", "⛶ 进入裁剪"),
    ("Straighten (°)", "拉直 (°)"),
    ("Once in, drag the corner handles / move the crop box on the image; preview, export and XMP all match. Straighten auto-crops the black corners.",
        "进入后，在图上拖角柄 / 移动裁剪框；预览、导出与 XMP 三者一致。拉直会自动裁掉黑边。"),
    ("Lens", "镜头 · Lens"),
    ("Vignette", "暗角"),
    ("Midpoint", "中点"),
    ("Distortion", "畸变"),
    ("Vignette: positive brightens the corners (compensates falloff), negative darkens; a radial gain in linear light. Distortion: positive fixes barrel (wide-angle bulge), negative fixes pincushion (tele pinch); auto-scales to fill the frame, and masks / brush still position on the corrected image. Preview / export / XMP match. De-fringe in a later batch.",
        "暗角：正值提亮四角（补偿衰减），负值压暗；在线性光下的径向增益。畸变：正值修桶形（广角外凸），负值修枕形（长焦内缩）；自动缩放填满画幅，蒙版/画笔仍按校正后的图像定位。预览/导出/XMP 一致。去紫边留待后续批次。"),
    // CROP_ASPECTS display names (ratio values are not localized).
    ("Free", "自由"),
    ("Original", "原始"),

    // ── Develop · Local Masks (add + AI segmentation) ────────────────────────
    ("Local Masks ({n})", "局部蒙版 ({n})"),
    ("＋ Linear gradient", "＋ 线性渐变"),
    ("Drag on the image: start = unaffected side, end = fully-applied side",
        "在图上拖拽：起点=不受影响侧，终点=完全应用侧"),
    ("Drag on the image to draw a linear gradient (start unaffected → end fully applied)",
        "在图上拖拽画线性渐变（起点不受影响 → 终点完全应用）"),
    ("＋ Radial", "＋ 径向"),
    ("Drag on the image to draw an elliptical area", "在图上拖拽画一个椭圆区域"),
    ("Drag on the image to draw a radial (elliptical) area", "在图上拖拽画径向（椭圆）区域"),
    ("🤖 AI select subject", "🤖 AI 选主体"),
    ("U²-Net salient-subject segmentation → bitmap mask (python sidecar: pip install rembg; first run auto-downloads the model to ~/.u2net)",
        "U²-Net 显著主体分割 → 位图蒙版（python sidecar：pip install rembg；首次运行自动下载模型到 ~/.u2net）"),
    ("☁ AI select sky", "☁ AI 选天空"),
    ("SegFormer-ADE20K sky segmentation → bitmap mask (python sidecar: pip install transformers; first run auto-downloads a ~14MB model)",
        "SegFormer-ADE20K 天空分割 → 位图蒙版（python sidecar：pip install transformers；首次运行自动下载约 14MB 模型）"),

    // ── Develop · selected-mask controls ─────────────────────────────────────
    ("Name", "名称"),
    ("↻ Redraw", "↻ 重画"),
    ("Re-drag this mask's area on the image", "在图上重新拖拽这个蒙版的范围"),
    ("Overlay", "叠加"),
    ("Show this mask's actual coverage as a red semi-transparent overlay (geometry × range × strength, shortcut O)",
        "用红色半透明显示这个蒙版的实际作用范围（几何×范围×强度，快捷键 O）"),
    ("Move up (renders earlier)", "上移（更早渲染）"),
    ("Move down (renders later)", "下移（更晚渲染）"),
    ("Invert", "反转"),
    ("Range mask", "范围蒙版"),
    ("None", "无"),
    ("Color", "颜色"),
    ("Colour range: click the colour to pick in the image", "颜色范围：点击图中要选取的颜色"),
    ("Lum. low", "亮度下限 Lo"),
    ("Lum. high", "亮度上限 Hi"),
    ("Feather", "羽化 Feather"),
    ("🎯 Click in image…", "🎯 点击图中…"),
    ("🎯 Sample", "🎯 取样"),
    ("Click the colour to pick in the image (the same colour at other brightnesses is also selected)",
        "在图上点击要选取的颜色（亮暗不同的同色也会被选中）"),
    ("Tolerance", "容差 Tolerance"),
    ("Amount", "强度"),
    ("Temp", "色温"),
    ("Noise Red.", "降噪"),
    ("More (XMP/Lightroom only)", "更多 · More（仅 XMP/Lightroom 生效）"),
    ("Texture", "纹理"),
    ("Lightroom-style local adjustments: add a gradient to darken the sky, a radial to brighten the subject. AI Analyze also writes to this list.",
        "像 Lightroom 的局部调整：加一个渐变压暗天空、径向提亮主体。AI Analyze 也会写到同一列表。"),

    // ── Develop · Versions ───────────────────────────────────────────────────
    ("Versions ({n})", "版本 · Versions ({n})"),
    ("＋ Save as version", "＋ 存为版本"),
    ("Save all current develop parameters as a numbered snapshot (./out/<name>.v<N>.recipe.json), reloadable anytime",
        "把当前全部 develop 参数存为一个编号快照（./out/<名>.v<N>.recipe.json），随时可回"),
    ("Load", "载入"),
    ("Replace current parameters (one Ctrl+Z to undo)", "替换当前参数（一步 Ctrl+Z 可撤销）"),
    ("Like LR virtual copies: store multiple parameter sets for one photo (B&W, cropped…) without overwriting.",
        "像 LR 虚拟副本：一张照片存多套参数（黑白版/裁剪版…），互不覆盖。"),

    // ── Develop · export bar sliders (in update()) ───────────────────────────
    ("Output sharpening", "输出锐化"),
    ("JPEG quality", "JPEG 质量"),

    // ── Develop · tone curve (curve_editor) ──────────────────────────────────
    ("Master", "主"),
    ("Clear the current channel's curve", "清空当前通道曲线"),

    // ── Develop · histogram + clipping triangles (histogram_ui) ──────────────
    ("shadow crush", "阴影死黑"),
    ("highlight clip", "高光溢出"),
    ("{what}: {chan} channel(s) — click to toggle clipping warning (J)",
        "{what}：{chan} 通道 — 点击切换削波警告 (J)"),
    ("{what} indicator (clean) — click to toggle clipping warning (J)",
        "{what}指示（干净）— 点击切换削波警告 (J)"),

    // ── Canvas · mode hints + zoom / clip / preview-edge (after_view) ────────
    ("Before (source) — release B to return to editing", "Before (source) — 松开 B 回到编辑"),
    ("Crop — drag the handles to adjust, drag inside to move", "裁剪 — 拖角柄调整，框内拖动移动"),
    ("Local adjustment — drag on the image to draw the gradient area", "局部调整 — 在图上拖拽画出渐变范围"),
    ("WB eyedropper — click a spot that should be neutral grey/white", "WB 吸管 — 点击应为中性灰/白的位置"),
    ("Colour range — click the colour to pick in the image", "颜色范围 — 点击图中要选取的颜色"),
    ("Stamp — Alt+click to set the source · drag to brush the area to cover",
        "图章 — Alt+点击取源点 · 拖动涂要覆盖的区域"),
    ("After — paint over the area to fill / heal", "After — 涂抹要填充/修复的区域"),
    ("After — drag a box = local AI · scroll to zoom · space/middle-drag to pan · hold B to compare",
        "After — 拖框=局部AI · 滚轮缩放 · 空格/中键平移 · 按住B对比"),
    ("Preview pixels 1:1 (double-click the image to toggle)", "预览像素 1:1（双击图片可切换）"),
    ("Fit the whole image to the canvas (double-click the image to toggle)", "整图适配画布（双击图片可切换）"),
    ("Clipping warning (J): red = highlight clip, blue = shadow crush (judged on export pixels)",
        "削波警告 (J)：红 = 高光溢出，蓝 = 阴影死黑（按导出像素判定）"),
    ("1280px · fluid", "1280px 流畅"),
    ("4096px · inspect", "4096px 检查"),
    ("Working preview resolution: 1280 is smoothest on the sliders; 2560/4096 for 1:1 focus/noise checks (slower on every adjustment)",
        "工作预览分辨率：1280 滑杆最流畅；2560/4096 供 1:1 查合焦/噪点（每次调整更慢）"),

    // ── Develop · variant strip (variant_strip) ──────────────────────────────
    ("Variants", "版本"),
    ("Click to switch to this variant (lossless)", "点击切到此版本（无损）"),
    ("Delete this variant", "删除此版本"),

    // ── Toolbar · top row (update()) ─────────────────────────────────────────
    ("Batch {done}/{total}", "批量 {done}/{total}"),
    ("Open photo…", "打开照片…"),
    ("Ctrl+O · or drag a file into the window", "Ctrl+O · 或直接拖拽进窗口"),
    ("✨ AI Analyze", "✨ AI 分析"),
    ("AI proposes a recipe (GPT proposal + validation), written into the sliders — undoable",
        "AI 提配方（GPT 提议 + 验证），写进滑杆 — 可撤销"),
    ("Refine", "微调"),
    ("Adjust the CURRENT edit instead of proposing from scratch", "在当前编辑基础上微调，而不是从零提议"),
    ("Reset", "重置"),
    ("Clear every slider back to neutral (one undo brings it back)", "清空全部滑杆回中性（一步撤销可回来）"),
    ("↶ Undo", "↶ 撤销"),
    ("↷ Redo", "↷ 重做"),
    ("Style", "风格"),
    ("Personal style strength: how far AI proposals lean toward your past XMP editing habits (0 = ignore)",
        "个人风格强度：AI 提案向你过往 XMP 编辑习惯靠拢的程度（0 = 不参考）"),
    ("Personal style strength: how far AI proposals lean toward your past editing habits",
        "个人风格强度：AI 提案向你过往编辑习惯靠拢的程度"),
    ("⿲ Compare", "⿲ 对比"),
    ("Before/After side by side", "原图/成片并排"),
    ("⬛ Single", "⬛ 单图"),
    ("The edit fills the canvas; hold B to quickly compare the original", "编辑图占满画布；按住 B 快速对比原图"),
    ("⚙ Settings", "⚙ 设置"),
    ("AI provider / model / API key", "AI 提供方 / 模型 / API 密钥"),
    ("Keyboard shortcuts (F1 / ?)", "快捷键速查（F1 / ?）"),

    // ── Toolbar · export bar (update()) ──────────────────────────────────────
    ("Direction:", "方向："),
    ("e.g. warmer and moodier, lift the shadows", "例如：更暖更有氛围，提亮阴影"),
    ("16-bit TIFF", "16 位 TIFF"),
    ("Long edge", "长边"),
    ("Original size", "原尺寸"),
    ("Colour space", "色彩空间"),
    ("sRGB (universal)", "sRGB（通用）"),
    ("Display P3 (wide-gamut screens)", "Display P3（广色域屏）"),
    ("Adobe RGB (print)", "Adobe RGB（印刷）"),
    ("AI Denoise", "AI 降噪"),
    ("SCUNet AI denoise before developing — high-ISO / astro (slow, GPU; needs the python sidecar)",
        "显影前做 SCUNet AI 降噪 — 高 ISO / 星空（慢，需 GPU 与 python sidecar）"),
    ("Export → ./out", "导出 → ./out"),
    ("Ctrl+E · full-resolution render to ./out (follows the current variant's pixels)",
        "Ctrl+E · 全分辨率渲染到 ./out（跟随当前变体像素）"),
    ("Download…", "下载…"),
    ("Save as… (full-resolution export to a path you choose)", "另存为…（选路径的全分辨率导出）"),
    ("Save XMP", "保存 XMP"),
    ("Ctrl+S · write a Lightroom/ACR sidecar to ./out (RAW only)", "Ctrl+S · 写 Lightroom/ACR sidecar 到 ./out（仅 RAW）"),

    // ── Empty-state landing screen (update()) ────────────────────────────────
    ("AI auto-develop · RAW develop", "AI 自动出片 · RAW develop"),
    ("📷 Open photo…  (Ctrl+O)", "📷 打开照片…  (Ctrl+O)"),
    ("🗂 Open folder…", "🗂 打开文件夹…"),
    ("or drag a RAW / image straight into the window · drag & drop anywhere",
        "或把 RAW / 图片直接拖进窗口 · drag & drop anywhere"),

    // ── Variant strip · kind labels + switch status ──────────────────────────
    ("▣ Original", "▣ 原片"),
    ("✨ AI generated", "✨ AI 生成"),
    ("◭ Reverse-fit", "◭ 反推"),
    ("Switched to variant「{name}」 — variants are independent, switching is lossless",
        "已切到「{name}」变体 — 各版本独立，切换无损"),

    // ── Canvas mask badge + model-picker placeholder ─────────────────────────
    ("▨ Bitmap mask", "▨ 位图蒙版"),
    ("Select…", "选择… / pick"),

    // ── Versions · save / load snapshots (status) ────────────────────────────
    ("Version v{n} saved → {path}", "版本 v{n} 已存 → {path}"),
    ("Save version failed: {err}", "存版本失败: {err}"),
    ("Loaded version v{n} — Ctrl+Z returns to before the load", "已载入版本 v{n} — Ctrl+Z 可回到载入前"),
    ("Load v{n} failed: {err}", "载入 v{n} 失败: {err}"),

    // ── Tone curve caption (curve_editor) ────────────────────────────────────
    ("Click to add a point · drag to move · drag outside the box to delete — preview / export / XMP all match",
        "点击加点 · 拖动移点 · 拖出框外删点 — 预览/导出/XMP 同源生效"),

    // ── AI segmentation · subject / sky (labels + status) ────────────────────
    ("Subject", "主体"),
    ("Sky", "天空"),
    ("AI segmenting {what}… (first run auto-downloads the model — watch the console log)",
        "AI {what}分割中…（首次运行会自动下载模型，看控制台日志）"),
    ("AI「{what}」mask added — adjust its sliders (exposure / contrast / saturation…) to take effect",
        "AI「{what}」蒙版已加入 — 调它的滑杆（曝光/对比/饱和…）即刻生效"),
    ("AI segmentation failed", "AI 分割失败"),

    // ── Status bar · open / decode / scan / settings / esc ───────────────────
    ("decoding {path} …", "解码 {path} …"),
    ("scanning {path} …", "扫描 {path} …"),
    ("settings saved — applies to the next Analyze", "设置已保存 — 下次分析时生效"),
    ("preview develop failed", "预览显影失败"),
    ("ready — adjust sliders or run AI Analyze", "就绪 — 拉滑杆或运行 AI 分析"),
    ("could not open", "打开失败"),
    ("1 photo — click a thumbnail to open", "1 张照片 — 点击缩略图打开"),
    ("{n} photos — click a thumbnail to open", "{n} 张照片 — 点击缩略图打开"),
    ("scan failed", "扫描失败"),
    ("busy — wait for the current task to finish before opening", "忙 — 等当前任务完成再打开"),
    ("unsupported file type: {path}", "不支持的文件类型: {path}"),
    ("Exited the current tool (Esc)", "已退出当前工具（Esc）"),

    // ── Status bar · AI analyze / render / export / region ───────────────────
    ("refining your current edit with AI…", "AI 微调当前编辑中…"),
    ("analyzing with AI (GPT + Claude)…", "AI 分析中（GPT + Claude）…"),
    ("AI develop applied", "AI 显影已应用"),
    ("analyze failed", "分析失败"),
    ("rendering + AI denoise → {path} … (GPU sidecar, can take minutes)",
        "渲染 + AI 降噪 → {path} …（GPU sidecar，可能数分钟）"),
    ("rendering full-resolution → {path} …", "全分辨率渲染 → {path} …"),
    ("exported → {path}", "已导出 → {path}"),
    ("export failed", "导出失败"),
    ("retouch failed", "修饰失败"),
    ("region {w}×{h}% — type a direction, then AI Analyze (click to clear)",
        "选区 {w}×{h}% — 输入方向语后 AI 分析（点击清除）"),

    // ── Status bar · batch render / paste / preview re-decode ────────────────
    ("Batch-rendering {n} photos → ./out …", "批量渲染 {n} 张 → ./out …"),
    ("./out — batch {n} done", "./out — 批量 {n} 张完成"),
    ("Batch: {ok} succeeded, {fail} failed: {detail}", "批量：{ok} 成功、{fail} 失败：{detail}"),
    ("Batch-rendering {done}/{total} → ./out …", "批量渲染 {done}/{total} → ./out …"),
    ("Pasting recipe to {n} photos…", "粘贴配方到 {n} 张…"),
    ("Recipe pasted to {ok} photos ({xmp} XMP) → ./out", "配方已粘贴到 {ok} 张（{xmp} 个 XMP）→ ./out"),
    ("{ok} succeeded, {fail} failed: {detail}", "{ok} 成功、{fail} 失败：{detail}"),
    ("batch paste", "批量粘贴"),
    ("Preview resolution {px}px — re-decoded", "预览分辨率 {px}px — 已重解码"),

    // ── Status bar · XMP save ────────────────────────────────────────────────
    ("A generated variant's look lives in its pixels — there's no parametric recipe to export; run 「Reverse-fit」 first to get an exportable XMP",
        "生成变体的观感在像素里，没有参数配方可导；先「反推配方」得到可导出的 XMP"),
    ("XMP applies to RAW files only", "XMP 仅适用于 RAW 文件"),
    ("XMP saved → {path}", "XMP 已保存 → {path}"),
    ("XMP save failed: {err}", "XMP 保存失败: {err}"),

    // ── Status bar · WB / range pick + manual mask placement ─────────────────
    ("WB eyedropper: {k} K · tint {tint} — fine-tune in the Tone section",
        "WB 吸管：{k} K · tint {tint} — 可在色调区微调"),
    ("Colour range: sampled — the 「Tolerance」 slider adjusts the selection width",
        "颜色范围：已取样 — 「容差」滑杆调节选中宽度"),
    ("Manual {n}", "手动 {n}"),
    ("mask placed — pull its sliders in 「Local Masks」 at left (all 0 now, no visible effect yet)",
        "mask 已放置 — 在左侧「局部调整」里拉滑杆（当前全为 0，无可见效果）"),

    // ── Status bar · generative fill / heal / clone ──────────────────────────
    ("write what should fill the painted area", "写下涂抹区域该填入什么"),
    ("paint the area to remove/fill first (tick Paint mask)", "先涂抹要移除/填充的区域（勾选「涂抹蒙版」）"),
    ("generative fill (full-res render)… (slow, minutes)", "生成填充（全分辨率渲染）…（慢，数分钟）"),
    ("generative fill via gpt-image… (~15-40s)", "gpt-image 生成填充中…（约 15-40 秒）"),
    ("filled → {path} (updated current variant)", "已填充 → {path}（更新当前变体）"),
    ("tick Paint mask and paint the spots, then Heal painted area",
        "勾选「涂抹蒙版」并涂抹瑕疵，再「修复涂抹区域」"),
    ("healing painted area…", "修复涂抹区域中…"),
    ("AI healing… (~10-30s)", "AI 去瑕疵中…（约 10-30 秒）"),
    ("healed {n} spot(s) → {path}", "已修复 {n} 处 → {path}"),
    ("Clone source sampled — brush the area to cover, then 「⎘ Clone painted area」",
        "克隆源已取样 — 画笔涂要覆盖的区域，然后「⎘ 克隆已涂区域」"),
    ("Alt+click to set the clone source first", "先 Alt+点击取克隆源点"),
    ("Brush the area to clone over first", "先用画笔涂要克隆覆盖的区域"),
    ("Cloning… (local pixel compute)", "克隆中…（本地像素运算）"),
    ("Cloned {n} spot(s) → {path}", "克隆 {n} 处 → {path}"),

    // ── Status bar · reimagine / reverse-fit / style prompt ──────────────────
    ("AI generating… (gpt-image, ~15–60s; hi-res input needs a full-frame develop first)",
        "AI 生成出片中…（gpt-image，约 15–60 秒；高分辨率输入需先全幅显影）"),
    ("「AI generated」variant created → {path} · keep tweaking or 「Reverse-fit」",
        "已生成「AI 生成」变体 → {path} · 可继续微调或「反推配方」"),
    ("Reverse-fitting… (statistical fit + sky segmentation; first run downloads the model)",
        "反推配方中…（统计拟合 + 天空分割，首次分割会下载模型）"),
    ("Reverse-fitting… (statistical fit, local compute)", "反推配方中…（统计拟合，本地运算）"),
    ("Reverse-fit done: look residual {before}→{after} · created a「Reverse-fit」variant (editable / XMP / full-res)",
        "反推完成：look 残差 {before}→{after} · 已建「反推」变体（可编辑/导 XMP/出全分辨率）"),
    (" · includes sky-zone correction (adjustable in the mask panel; XMP carries the global part only)",
        " · 含天空分区校正（蒙版面板可调；XMP 只带全局部分）"),
    ("Reverse-fit failed", "反推失败"),
    ("Style prompt extracted → filled into Direction (also saved ./out/<stem>.style.txt)",
        "风格提示词已提取 → 已填入 Direction（同时存 ./out/<stem>.style.txt）"),
    ("Extracting style prompt… (vision, ~5-20s)", "提取风格提示词中…（vision，约 5-20 秒）"),
    ("Style extraction failed", "风格提取失败"),

    // ── Shortcuts cheat-sheet window (title + both columns) ───────────────────
    ("⌨ Shortcuts", "⌨ 快捷键 · Shortcuts"),
    ("Open photo", "打开照片"),
    ("Save XMP sidecar", "保存 XMP sidecar"),
    ("Undo / Redo", "撤销 / 重做"),
    ("Step through the library", "图库走图"),
    ("B (hold)", "B（按住）"),
    ("Compare original", "对比原图"),
    ("Toggle mask overlay", "蒙版覆盖层开关"),
    ("Toggle clipping warning", "削波警告开关"),
    ("Exit tool / close this window", "退出当前工具 / 关闭本窗"),
    ("This cheat-sheet", "本速查表"),
    ("Scroll", "滚轮"),
    ("Zoom (toward cursor)", "缩放（指向光标）"),
    ("Double-click canvas", "双击画布"),
    ("Space+drag / middle-drag", "空格+拖 / 中键拖"),
    ("Pan", "平移"),
    ("Drag when zoomed", "放大后直接拖"),
    ("Pan (Ctrl+drag = box-select)", "平移（Ctrl+拖 = 框选）"),
    ("Alt+click", "Alt+点击"),
    ("Sample clone source", "克隆取源点"),
    ("Slider double-click", "滑杆双击"),
    ("Reset to zero", "归零"),
    ("Curve: click / drag / drag-out", "曲线：点击/拖/拖出框"),
    ("Add / move / delete point", "加点 / 移点 / 删点"),
    ("Drag a mask handle", "蒙版手柄拖拽"),
    ("Reshape / move the selected mask", "改形 / 移动选中蒙版"),

    // ── Drag & drop overlay ──────────────────────────────────────────────────
    ("Drop to open", "松开打开 · Drop to open"),
];
