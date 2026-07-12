//! XMP sidecar writer — render an [`EditRecipe`] as an Adobe Camera Raw /
//! Lightroom `.xmp` sidecar (the `crs:` namespace), so the AI's edit opens as
//! adjustable develop sliders in the user's catalog.
//!
//! Key names, value conventions, and structure were verified against a real ACR
//! sidecar from the user's own library (`DSC08724.xmp`): `ProcessVersion=15.4`,
//! signed-integer sliders, `Sharpness` on 0..100, tone curve as an `rdf:Seq` of
//! `"x, y"` strings (see `docs/M1_PLAN.md` §5 and §9). We emit only the keys we
//! set; Lightroom fills the rest from defaults.

use crate::recipe::{ColorGrade, Crop, CurvePoint, EditRecipe, Hsl, LocalAdjustment, MaskGeometry, RangeMask};

/// Format an integer-valued slider the way ACR writes it: explicit `+` for
/// positives (`"+14"`, `"-12"`, `"0"`).
fn signed(v: f32) -> String {
    let i = v.round() as i64;
    if i > 0 {
        format!("+{i}")
    } else {
        i.to_string()
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn attr(buf: &mut String, key: &str, val: &str) {
    buf.push_str(&format!("\n    crs:{key}=\"{val}\""));
}

/// Format a LOCAL adjustment value the way ACR writes it: a bare decimal, no
/// forced `+` (e.g. `"-0.075"`, `"0"`). Distinct from the global `signed()`.
fn local_fmt(v: f32) -> String {
    if v == 0.0 {
        "0".to_string()
    } else {
        format!("{v}")
    }
}

/// A stable 32-uppercase-hex GUID derived from `seed` (no external uuid dep).
/// Deterministic so re-emitting the same recipe yields the same sidecar; the
/// per-mask seed includes the index so masks within a file stay unique.
fn guid(seed: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut h1);
    let a = h1.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    (seed, a).hash(&mut h2);
    let b = h2.finish();
    format!("{a:016X}{b:016X}")
}

/// `(crs:What value, extra geometry attributes)` for a mask geometry, or
/// `None` for geometries classic ACR XMP cannot express (raster bitmaps —
/// the writer skips those corrections; the render still applies them).
/// Coordinates are written raw (unclamped) — ACR gradients legitimately use
/// values outside [0,1].
fn mask_geom_xml(g: &MaskGeometry) -> Option<(&'static str, String)> {
    match g {
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => Some((
            "Mask/Gradient",
            format!(
                " crs:ZeroX=\"{zero_x}\" crs:ZeroY=\"{zero_y}\" crs:FullX=\"{full_x}\" crs:FullY=\"{full_y}\""
            ),
        )),
        MaskGeometry::Radial { top, left, bottom, right, feather, roundness, flipped } => Some((
            "Mask/CircularGradient",
            format!(
                " crs:Top=\"{top}\" crs:Left=\"{left}\" crs:Bottom=\"{bottom}\" crs:Right=\"{right}\" \
crs:Feather=\"{feather}\" crs:Roundness=\"{roundness}\" crs:Flipped=\"{flipped}\""
            ),
        )),
        MaskGeometry::Bitmap { .. } => None,
    }
}

/// A `Mask/RangeMask` component `<rdf:li>` intersected with the correction's
/// geometric mask (empty string when the adjustment has no range). Component
/// structure and attribute values verified against the user's own Lightroom
/// sidecars (`_DSC9245.xmp` luminance, `_DSC9303.xmp` colour): the intersect
/// encoding is `MaskBlendMode="1" + MaskInverted="true" + MaskValue="0"` —
/// i.e. "paint 0 wherever the range does NOT match", which erases everything
/// outside geometry ∩ range. Luminance uses the attribute form
/// (`crs:LumRange="lo_outer lo hi hi_outer"`); colour uses the child-element
/// form with one `crs:PointModels` entry `"r g b px py 0"` (last three numbers
/// assumed sample-point + reserved; see ROADMAP §A for the verification note).
fn range_mask_xml(range: &Option<RangeMask>, sync_id: &str) -> String {
    let Some(rm) = range else { return String::new() };
    let head = |name: &str| {
        format!(
            "         <rdf:li>\n\
          <rdf:Description\n\
           crs:What=\"Mask/RangeMask\" crs:MaskActive=\"true\" crs:MaskName=\"{name}\"\n\
           crs:MaskBlendMode=\"1\" crs:MaskInverted=\"true\" crs:MaskSyncID=\"{sync_id}\"\n\
           crs:MaskValue=\"0\">\n"
        )
    };
    match rm {
        RangeMask::Luminance { lo_outer, lo, hi, hi_outer } => format!(
            "{}\
           <crs:CorrectionRangeMask\n\
            crs:Version=\"3\"\n\
            crs:Type=\"2\"\n\
            crs:Invert=\"false\"\n\
            crs:SampleType=\"0\"\n\
            crs:LumRange=\"{lo_outer:.6} {lo:.6} {hi:.6} {hi_outer:.6}\"\n\
            crs:LuminanceDepthSampleInfo=\"0 0.500000 0.500000\"/>\n\
          </rdf:Description>\n\
         </rdf:li>\n",
            head("Luminance Range"),
        ),
        RangeMask::Color { r, g, b, amount, px, py } => format!(
            "{}\
           <crs:CorrectionRangeMask>\n\
            <rdf:Description\n\
             crs:Version=\"3\"\n\
             crs:Type=\"1\"\n\
             crs:ColorAmount=\"{amount:.6}\"\n\
             crs:Invert=\"false\"\n\
             crs:SampleType=\"0\">\n\
            <crs:PointModels>\n\
             <rdf:Seq>\n\
              <rdf:li>{r:.6} {g:.6} {b:.6} {px:.6} {py:.6} 0</rdf:li>\n\
             </rdf:Seq>\n\
            </crs:PointModels>\n\
            </rdf:Description>\n\
           </crs:CorrectionRangeMask>\n\
          </rdf:Description>\n\
         </rdf:li>\n",
            head("Color Range"),
        ),
    }
}

/// Build the `<crs:MaskGroupBasedCorrections>` child element (empty string when
/// there are no masks). Local sliders convert UI scale → ACR local scale:
/// exposure stops ÷4, every other slider ÷100 (verified against the user's real
/// sidecar; see docs/V2_PLAN.md §2a). All 26 `Local*` fields are emitted (unused
/// = 0) as Lightroom expects the full block.
fn masks_xml(r: &EditRecipe) -> String {
    if r.masks.is_empty() {
        return String::new();
    }
    let mut items = String::new();
    for (i, m) in r.masks.iter().enumerate() {
        let name = if m.name.is_empty() { format!("Autoshop {}", i + 1) } else { m.name.clone() };
        let corr_id = guid(&format!("corr-{i}-{name}"));
        let mask_id = guid(&format!("mask-{i}-{name}"));
        // Raster (bitmap) masks have no classic-XMP encoding — skip this
        // correction; the deterministic render still applies it (§A tradeoff).
        let Some((what, geom)) = mask_geom_xml(&m.mask) else { continue };
        items.push_str(&format!(
            "     <rdf:li>\n\
      <rdf:Description\n\
       crs:What=\"Correction\" crs:CorrectionAmount=\"{amount}\" crs:CorrectionActive=\"true\"\n\
       crs:CorrectionName=\"{name}\" crs:CorrectionSyncID=\"{corr_id}\"\n\
       crs:LocalExposure=\"0\" crs:LocalHue=\"0\" crs:LocalSaturation=\"{sat}\"\n\
       crs:LocalContrast=\"0\" crs:LocalClarity=\"0\" crs:LocalSharpness=\"0\"\n\
       crs:LocalBrightness=\"0\" crs:LocalToningHue=\"0\" crs:LocalToningSaturation=\"0\"\n\
       crs:LocalExposure2012=\"{exp}\" crs:LocalContrast2012=\"{con}\"\n\
       crs:LocalHighlights2012=\"{hi}\" crs:LocalShadows2012=\"{sh}\"\n\
       crs:LocalWhites2012=\"{wh}\" crs:LocalBlacks2012=\"{bl}\"\n\
       crs:LocalClarity2012=\"{cl}\" crs:LocalDehaze=\"{dh}\" crs:LocalLuminanceNoise=\"{nr}\"\n\
       crs:LocalMoire=\"0\" crs:LocalDefringe=\"0\" crs:LocalTemperature=\"{temp}\"\n\
       crs:LocalTint=\"{tint}\" crs:LocalTexture=\"{tex}\" crs:LocalGrain=\"0\"\n\
       crs:LocalCurveRefineSaturation=\"100\">\n\
       <crs:CorrectionMasks>\n\
        <rdf:Seq>\n\
         <rdf:li crs:What=\"{what}\" crs:MaskActive=\"true\" crs:MaskName=\"{mname}\"\n\
          crs:MaskBlendMode=\"0\" crs:MaskInverted=\"{inv}\" crs:MaskSyncID=\"{mask_id}\"\n\
          crs:MaskValue=\"1\"{geom}/>\n\
{range}\
        </rdf:Seq>\n\
       </crs:CorrectionMasks>\n\
      </rdf:Description>\n\
     </rdf:li>\n",
            range = range_mask_xml(&m.range, &guid(&format!("range-{i}-{name}"))),
            amount = local_fmt(m.amount),
            name = xml_escape(&name),
            corr_id = corr_id,
            sat = local_fmt(m.saturation / 100.0),
            exp = local_fmt(m.exposure_ev / 4.0),
            con = local_fmt(m.contrast / 100.0),
            hi = local_fmt(m.highlights / 100.0),
            sh = local_fmt(m.shadows / 100.0),
            wh = local_fmt(m.whites / 100.0),
            bl = local_fmt(m.blacks / 100.0),
            cl = local_fmt(m.clarity / 100.0),
            dh = local_fmt(m.dehaze / 100.0),
            temp = local_fmt(m.temperature / 100.0),
            tint = local_fmt(m.tint / 100.0),
            tex = local_fmt(m.texture / 100.0),
            nr = local_fmt(m.noise_reduction / 100.0),
            what = what,
            mname = xml_escape(&format!("{name} mask")),
            inv = m.inverted,
            mask_id = mask_id,
            geom = geom,
        ));
    }
    // All masks may have been raster-skipped — no empty wrapper block then.
    if items.is_empty() {
        return String::new();
    }
    format!(
        "\n   <crs:MaskGroupBasedCorrections>\n    <rdf:Seq>\n{items}    </rdf:Seq>\n   </crs:MaskGroupBasedCorrections>"
    )
}

/// Render `recipe` as a complete `.xmp` sidecar document.
pub fn recipe_to_xmp(r: &EditRecipe) -> String {
    let mut a = String::new();

    // ProcessVersion 15.4 / Version 15.5.1 are the verified current values from
    // the user's real sidecar (not the research's guessed 11.0/15.0).
    attr(&mut a, "Version", "15.5.1");
    attr(&mut a, "ProcessVersion", "15.4");

    // White balance: an explicit temperature means Custom WB; otherwise leave
    // it As Shot and only carry a tint if non-neutral.
    if let Some(k) = r.temperature_k {
        attr(&mut a, "WhiteBalance", "Custom");
        attr(&mut a, "Temperature", &(k.round() as i64).to_string());
        attr(&mut a, "Tint", &signed(r.tint));
    } else {
        attr(&mut a, "WhiteBalance", "As Shot");
        if r.tint != 0.0 {
            attr(&mut a, "Tint", &signed(r.tint));
        }
    }

    // Exposure as a plain decimal (Lightroom parses signed or unsigned).
    attr(&mut a, "Exposure2012", &format!("{:.2}", r.exposure_ev));
    attr(&mut a, "Contrast2012", &signed(r.contrast));
    attr(&mut a, "Highlights2012", &signed(r.highlights));
    attr(&mut a, "Shadows2012", &signed(r.shadows));
    attr(&mut a, "Whites2012", &signed(r.whites));
    attr(&mut a, "Blacks2012", &signed(r.blacks));
    attr(&mut a, "Clarity2012", &signed(r.clarity));
    attr(&mut a, "Dehaze", &signed(r.dehaze));
    attr(&mut a, "Vibrance", &signed(r.vibrance));
    attr(&mut a, "Saturation", &signed(r.saturation));

    // Per-colour HSL / Color mixer (8 ACR bands). Emit only when non-neutral so
    // a plain global recipe still produces a minimal, v1-compatible sidecar.
    if !r.hsl.is_neutral() {
        for (i, band) in crate::recipe::HSL_BANDS.iter().enumerate() {
            attr(&mut a, &format!("HueAdjustment{band}"), &signed(r.hsl.hue[i]));
            attr(&mut a, &format!("SaturationAdjustment{band}"), &signed(r.hsl.saturation[i]));
            attr(&mut a, &format!("LuminanceAdjustment{band}"), &signed(r.hsl.luminance[i]));
        }
    }

    // Colour grading (3-wheel + global). ACR convention VERIFIED against the
    // user's own sidecar: shadow/highlight hue+sat round-trip via the legacy
    // SplitToning* keys; lum, midtone, global, blending via ColorGrade*; balance
    // via SplitToningBalance. Hue/sat/blending are unsigned, lum/balance signed.
    if !r.color_grade.is_neutral() {
        let cg = &r.color_grade;
        let uns = |v: f32| (v.round() as i64).to_string();
        attr(&mut a, "SplitToningShadowHue", &uns(cg.shadow_hue));
        attr(&mut a, "SplitToningShadowSaturation", &uns(cg.shadow_sat));
        attr(&mut a, "SplitToningHighlightHue", &uns(cg.highlight_hue));
        attr(&mut a, "SplitToningHighlightSaturation", &uns(cg.highlight_sat));
        attr(&mut a, "SplitToningBalance", &signed(cg.balance));
        attr(&mut a, "ColorGradeShadowLum", &signed(cg.shadow_lum));
        attr(&mut a, "ColorGradeMidtoneHue", &uns(cg.midtone_hue));
        attr(&mut a, "ColorGradeMidtoneSat", &uns(cg.midtone_sat));
        attr(&mut a, "ColorGradeMidtoneLum", &signed(cg.midtone_lum));
        attr(&mut a, "ColorGradeHighlightLum", &signed(cg.highlight_lum));
        attr(&mut a, "ColorGradeGlobalHue", &uns(cg.global_hue));
        attr(&mut a, "ColorGradeGlobalSat", &uns(cg.global_sat));
        attr(&mut a, "ColorGradeGlobalLum", &signed(cg.global_lum));
        attr(&mut a, "ColorGradeBlending", &uns(cg.blending));
    }

    // recipe sharpening is 0..150; crs Sharpness is 0..100 — rescale + clamp.
    let sharp = ((r.sharpening * 2.0 / 3.0).round() as i64).clamp(0, 100);
    attr(&mut a, "Sharpness", &sharp.to_string());
    let nr = (r.noise_reduction.round() as i64).clamp(0, 100);
    attr(&mut a, "LuminanceSmoothing", &nr.to_string());

    // Manual lens-vignette correction. `VignetteAmount` name verified against
    // the user's sidecars (present, =0, in 140 of them); the Midpoint companion
    // key follows the documented ACR pair and is only emitted when the amount
    // is set — a zero-amount recipe stays byte-identical to the old writer.
    if r.lens_vignette != 0.0 {
        attr(&mut a, "VignetteAmount", &signed(r.lens_vignette));
        attr(&mut a, "VignetteMidpoint", &(r.lens_vignette_mid.round() as i64).to_string());
    }

    // Manual distortion correction — key name verified against the user's
    // sidecars (`LensManualDistortionAmount="0"` in 148 of them). Same
    // only-when-set policy as the vignette pair. NB: our render's amount→curve
    // gain is our own calibration; Adobe's is unpublished, so LR's slider at
    // the same number may correct a somewhat different physical strength.
    if r.lens_distortion != 0.0 {
        attr(&mut a, "LensManualDistortionAmount", &signed(r.lens_distortion));
    }

    // Crop (normalised [0,1]); only applied by Lightroom when HasCrop is True.
    if let Some(c) = &r.crop {
        attr(&mut a, "HasCrop", "True");
        attr(&mut a, "CropTop", &format!("{:.6}", c.top));
        attr(&mut a, "CropLeft", &format!("{:.6}", c.left));
        attr(&mut a, "CropBottom", &format!("{:.6}", c.bottom));
        attr(&mut a, "CropRight", &format!("{:.6}", c.right));
    } else {
        attr(&mut a, "HasCrop", "False");
    }
    if r.straighten_deg != 0.0 {
        attr(&mut a, "CropAngle", &format!("{:.1}", r.straighten_deg));
    }

    attr(
        &mut a,
        "ToneCurveName2012",
        if r.tone_curve.is_empty() { "Linear" } else { "Custom" },
    );

    // Tone curves are child elements (rdf:Seq of "x, y" strings), not attributes.
    // One builder for the master + the three per-channel curves (verified key
    // names against the user's sidecar: ToneCurvePV2012Red/Green/Blue).
    let curve_elem = |tag: &str, points: &[crate::recipe::CurvePoint]| -> String {
        if points.is_empty() {
            return String::new();
        }
        let pts: String = points
            .iter()
            .map(|p| format!("     <rdf:li>{}, {}</rdf:li>\n", p.input, p.output))
            .collect();
        format!("\n   <crs:{tag}>\n    <rdf:Seq>\n{pts}    </rdf:Seq>\n   </crs:{tag}>")
    };
    let tone = curve_elem("ToneCurvePV2012", &r.tone_curve);
    let rgb_curves = format!(
        "{}{}{}",
        curve_elem("ToneCurvePV2012Red", &r.red_curve),
        curve_elem("ToneCurvePV2012Green", &r.green_curve),
        curve_elem("ToneCurvePV2012Blue", &r.blue_curve),
    );

    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Autoshop\">\n\
 <!-- Generated by Autoshop. AI rationale: {rationale} (confidence {conf:.2}) -->\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"{attrs}\n\
    crs:HasSettings=\"True\">{tone}{rgb_curves}{masks}\n\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n",
        rationale = xml_escape(&r.rationale),
        conf = r.confidence,
        attrs = a,
        tone = tone,
        rgb_curves = rgb_curves,
        masks = masks_xml(r),
    )
}

// ───────────────────────── XMP → EditRecipe (reader) ─────────────────────────
//
// The inverse of [`recipe_to_xmp`], so a sidecar written earlier (by us or by
// Lightroom) can be loaded back into the editor. Scan-based like the eval
// harness's parser: classic-ACR values are flat `crs:Key="value"` attributes,
// verified against the user's real LR sidecars, so plain text scanning
// round-trips everything the writer emits without an XML dependency. Fields
// classic XMP cannot carry (bitmap masks, recolour gains, mask roles) simply
// don't come back — the app-internal recipe.json is the lossless sidecar; this
// reader is the recovery path when only an XMP exists.

/// Raw string value of a `crs:<key>="…"` attribute (first occurrence). The
/// `crs:` anchor makes prefixed cousins unambiguous (`crs:Tint` can never match
/// inside `crs:LocalTint`).
fn crs_str<'a>(xmp: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("crs:{key}=\"");
    let start = xmp.find(&needle)? + needle.len();
    let rest = &xmp[start..];
    Some(&rest[..rest.find('"')?])
}

/// Numeric `crs:` attribute, tolerating ACR's explicit `+` (`"+22"`). `None` if
/// the key is absent or unparsable. Shared with the eval harness + style index
/// (re-exported through `eval`).
pub(crate) fn crs_f32(xmp: &str, key: &str) -> Option<f32> {
    crs_str(xmp, key)?.trim().trim_start_matches('+').parse::<f32>().ok()
}

/// Undo [`xml_escape`]. `&amp;` is decoded LAST so an escaped `&lt;` cannot
/// cascade into a second decode.
fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&")
}

/// The text between `open` and `close` (first occurrence of each, in order).
fn block_between<'a>(xmp: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = xmp.find(open)? + open.len();
    let rest = &xmp[start..];
    Some(&rest[..rest.find(close)?])
}

/// Parse one `<crs:ToneCurvePV2012…>` `rdf:Seq` of `"x, y"` points back into
/// curve control points. A 2-point identity (0,0 → 255,255) collapses to empty:
/// Lightroom ALWAYS writes the master curve (even "Linear"), while our writer
/// omits empty curves — collapsing keeps a re-import equal to a recipe that
/// never touched the curve.
fn parse_curve(xmp: &str, tag: &str) -> Vec<CurvePoint> {
    let Some(body) = block_between(xmp, &format!("<crs:{tag}>"), &format!("</crs:{tag}>")) else {
        return Vec::new();
    };
    let mut pts = Vec::new();
    for chunk in body.split("<rdf:li>").skip(1) {
        let Some(end) = chunk.find("</rdf:li>") else { continue };
        let mut it = chunk[..end].split(',');
        if let (Some(x), Some(y)) = (it.next(), it.next())
            && let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>())
        {
            pts.push(CurvePoint {
                input: x.clamp(0.0, 255.0).round() as u8,
                output: y.clamp(0.0, 255.0).round() as u8,
            });
        }
    }
    let identity = [CurvePoint { input: 0, output: 0 }, CurvePoint { input: 255, output: 255 }];
    if pts == identity { Vec::new() } else { pts }
}

/// Local-mask corrections back from `<crs:MaskGroupBasedCorrections>` —
/// parametric geometries only, exactly what [`masks_xml`] can emit (LR brush /
/// AI masks and our own Bitmap rasters have no classic-XMP encoding; those
/// corrections are skipped, matching the writer's own skip rule).
fn parse_masks(xmp: &str) -> Vec<LocalAdjustment> {
    let Some(block) =
        block_between(xmp, "<crs:MaskGroupBasedCorrections>", "</crs:MaskGroupBasedCorrections>")
    else {
        return Vec::new();
    };
    let starts: Vec<usize> =
        block.match_indices("crs:What=\"Correction\"").map(|(i, _)| i).collect();
    let mut out = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(block.len());
        if let Some(m) = parse_one_correction(&block[s..end]) {
            out.push(m);
        }
    }
    out
}

/// One `crs:What="Correction"` segment → a [`LocalAdjustment`]. Slider scales
/// invert the writer's: exposure ×4 (a power-of-two rescale, exact in binary
/// FP), every other slider ×100 snapped to 4 decimals so `"0.3" → 30.0` lands
/// back on the UI grid instead of 30.000002.
fn parse_one_correction(seg: &str) -> Option<LocalAdjustment> {
    let q100 =
        |k: &str| crs_f32(seg, k).map_or(0.0, |v| (v * 100.0 * 10_000.0).round() / 10_000.0);
    // The geometry component decides the mask shape; a correction with no
    // parametric geometry is not representable here.
    let (mask, geom_at) = if let Some(p) = seg.find("crs:What=\"Mask/Gradient\"") {
        let g = &seg[p..];
        (
            MaskGeometry::Linear {
                zero_x: crs_f32(g, "ZeroX")?,
                zero_y: crs_f32(g, "ZeroY")?,
                full_x: crs_f32(g, "FullX")?,
                full_y: crs_f32(g, "FullY")?,
            },
            p,
        )
    } else if let Some(p) = seg.find("crs:What=\"Mask/CircularGradient\"") {
        let g = &seg[p..];
        (
            MaskGeometry::Radial {
                top: crs_f32(g, "Top")?,
                left: crs_f32(g, "Left")?,
                bottom: crs_f32(g, "Bottom")?,
                right: crs_f32(g, "Right")?,
                feather: crs_f32(g, "Feather")?,
                roundness: crs_f32(g, "Roundness")?,
                flipped: crs_str(g, "Flipped") == Some("true"),
            },
            p,
        )
    } else {
        return None;
    };
    // Optional range component. Its head repeats `MaskInverted="true"` as part
    // of the intersect ENCODING (see `range_mask_xml`), so user intent is read
    // from the geometry component only — hence the `geom_at`-anchored scan.
    let range = seg.find("crs:What=\"Mask/RangeMask\"").and_then(|p| {
        let r = &seg[p..];
        if let Some(lum) = crs_str(r, "LumRange") {
            let v: Vec<f32> = lum.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            (v.len() == 4).then(|| RangeMask::Luminance {
                lo_outer: v[0],
                lo: v[1],
                hi: v[2],
                hi_outer: v[3],
            })
        } else if let Some(amount) = crs_f32(r, "ColorAmount") {
            // PointModels entry: "r g b px py 0" (writer + LR convention).
            let li = block_between(r, "<rdf:li>", "</rdf:li>")?;
            let v: Vec<f32> = li.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            (v.len() >= 5)
                .then(|| RangeMask::Color { r: v[0], g: v[1], b: v[2], amount, px: v[3], py: v[4] })
        } else {
            None
        }
    });
    Some(LocalAdjustment {
        mask,
        range,
        name: crs_str(seg, "CorrectionName").map(xml_unescape).unwrap_or_default(),
        amount: crs_f32(seg, "CorrectionAmount").unwrap_or(1.0),
        inverted: crs_str(&seg[geom_at..], "MaskInverted") == Some("true"),
        exposure_ev: crs_f32(seg, "LocalExposure2012").unwrap_or(0.0) * 4.0,
        contrast: q100("LocalContrast2012"),
        highlights: q100("LocalHighlights2012"),
        shadows: q100("LocalShadows2012"),
        whites: q100("LocalWhites2012"),
        blacks: q100("LocalBlacks2012"),
        clarity: q100("LocalClarity2012"),
        dehaze: q100("LocalDehaze"),
        texture: q100("LocalTexture"),
        saturation: q100("LocalSaturation"),
        temperature: q100("LocalTemperature"),
        tint: q100("LocalTint"),
        noise_reduction: q100("LocalLuminanceNoise"),
        // color_gains / role are engine-only and never reach a sidecar.
        ..Default::default()
    })
}

/// Parse an ACR / Lightroom `.xmp` sidecar into an [`EditRecipe`] — the inverse
/// of [`recipe_to_xmp`] over every field classic XMP can carry. Absent keys stay
/// neutral, so a foreign XML parses to (nearly) a default recipe rather than
/// erroring. Two provenance rules keep a FOREIGN sidecar honest:
///   * `Temperature` counts only under `WhiteBalance="Custom"` — an "As Shot"
///     sidecar records the CAMERA's Kelvin, which is not an edit, and importing
///     it would visibly shift the render.
///   * Same for `Tint`, except sidecars we wrote ourselves (marked
///     `x:xmptk="Autoshop"`), whose Tint is always a real edit.
///
/// Callers should [`EditRecipe::clamp`] the result before use, like any other
/// untrusted recipe input.
pub fn xmp_to_recipe(xmp: &str) -> EditRecipe {
    let ours = xmp.contains("x:xmptk=\"Autoshop\"");
    let custom_wb = crs_str(xmp, "WhiteBalance") == Some("Custom");
    let f = |k: &str| crs_f32(xmp, k).unwrap_or(0.0);

    let mut hsl = Hsl::default();
    for (i, band) in crate::recipe::HSL_BANDS.iter().enumerate() {
        hsl.hue[i] = f(&format!("HueAdjustment{band}"));
        hsl.saturation[i] = f(&format!("SaturationAdjustment{band}"));
        hsl.luminance[i] = f(&format!("LuminanceAdjustment{band}"));
    }
    let color_grade = ColorGrade {
        shadow_hue: f("SplitToningShadowHue"),
        shadow_sat: f("SplitToningShadowSaturation"),
        shadow_lum: f("ColorGradeShadowLum"),
        midtone_hue: f("ColorGradeMidtoneHue"),
        midtone_sat: f("ColorGradeMidtoneSat"),
        midtone_lum: f("ColorGradeMidtoneLum"),
        highlight_hue: f("SplitToningHighlightHue"),
        highlight_sat: f("SplitToningHighlightSaturation"),
        highlight_lum: f("ColorGradeHighlightLum"),
        global_hue: f("ColorGradeGlobalHue"),
        global_sat: f("ColorGradeGlobalSat"),
        global_lum: f("ColorGradeGlobalLum"),
        blending: crs_f32(xmp, "ColorGradeBlending").unwrap_or(ColorGrade::default().blending),
        balance: f("SplitToningBalance"),
    };
    // Our own comment header carries the AI provenance back (best-effort; the
    // escaped rationale cannot contain a raw "-->", so the scan is unambiguous).
    let (rationale, confidence) = block_between(xmp, "AI rationale: ", " -->")
        .and_then(|body| {
            let cut = body.rfind(" (confidence ")?;
            let conf =
                body[cut + " (confidence ".len()..].trim_end_matches(')').parse::<f32>().ok()?;
            Some((xml_unescape(&body[..cut]), conf))
        })
        .unwrap_or_default();

    EditRecipe {
        temperature_k: custom_wb.then(|| crs_f32(xmp, "Temperature")).flatten(),
        tint: if custom_wb || ours { f("Tint") } else { 0.0 },
        exposure_ev: f("Exposure2012"),
        contrast: f("Contrast2012"),
        highlights: f("Highlights2012"),
        shadows: f("Shadows2012"),
        whites: f("Whites2012"),
        blacks: f("Blacks2012"),
        clarity: f("Clarity2012"),
        dehaze: f("Dehaze"),
        vibrance: f("Vibrance"),
        saturation: f("Saturation"),
        hsl,
        color_grade,
        // crs Sharpness is 0..100, recipe sharpening 0..150 (writer scales ×⅔).
        sharpening: f("Sharpness") * 1.5,
        noise_reduction: f("LuminanceSmoothing"),
        lens_vignette: f("VignetteAmount"),
        lens_vignette_mid: crs_f32(xmp, "VignetteMidpoint").unwrap_or(50.0),
        lens_distortion: f("LensManualDistortionAmount"),
        straighten_deg: f("CropAngle"),
        crop: (crs_str(xmp, "HasCrop") == Some("True"))
            .then(|| {
                Some(Crop {
                    left: crs_f32(xmp, "CropLeft")?,
                    top: crs_f32(xmp, "CropTop")?,
                    right: crs_f32(xmp, "CropRight")?,
                    bottom: crs_f32(xmp, "CropBottom")?,
                })
            })
            .flatten(),
        tone_curve: parse_curve(xmp, "ToneCurvePV2012"),
        red_curve: parse_curve(xmp, "ToneCurvePV2012Red"),
        green_curve: parse_curve(xmp, "ToneCurvePV2012Green"),
        blue_curve: parse_curve(xmp, "ToneCurvePV2012Blue"),
        masks: parse_masks(xmp),
        rationale,
        confidence,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{CurvePoint, EditRecipe, LocalAdjustment};

    #[test]
    fn renders_local_masks_with_correct_scale() {
        let r = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    name: "sky".into(),
                    exposure_ev: -0.4,  // ÷4 → -0.1
                    highlights: -50.0,  // ÷100 → -0.5
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Radial {
                        top: 0.3, left: 0.35, bottom: 0.7, right: 0.65,
                        feather: 0.5, roundness: 0.0, flipped: false,
                    },
                    name: "subject".into(),
                    shadows: 20.0,      // ÷100 → 0.2
                    inverted: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        // Write it out so well-formedness can be validated by an XML parser
        // (out/ is gitignored). Verification aid, not a behavioural assertion.
        std::fs::create_dir_all("out").ok();
        std::fs::write("out/_masks_test.xmp", &xmp).ok();
        assert!(xmp.contains("<crs:MaskGroupBasedCorrections>"));
        assert!(xmp.contains(r#"crs:What="Mask/Gradient""#));
        assert!(xmp.contains(r#"crs:What="Mask/CircularGradient""#));
        // local scale conversions
        assert!(xmp.contains(r#"crs:LocalExposure2012="-0.1""#)); // -0.4 / 4
        assert!(xmp.contains(r#"crs:LocalHighlights2012="-0.5""#)); // -50 / 100
        assert!(xmp.contains(r#"crs:LocalShadows2012="0.2""#)); // 20 / 100
        assert!(xmp.contains(r#"crs:MaskInverted="true""#));
        assert!(xmp.contains(r#"crs:ZeroX="0.5""#));
        assert!(xmp.contains(r#"crs:Feather="0.5""#));
        // unset masks ⇒ no mask block (v1-compatible)
        assert!(!recipe_to_xmp(&EditRecipe::default()).contains("MaskGroupBasedCorrections"));
    }

    #[test]
    fn renders_manual_vignette_only_when_set() {
        let r = EditRecipe { lens_vignette: 35.0, lens_vignette_mid: 60.0, ..Default::default() };
        let xmp = recipe_to_xmp(&r);
        assert!(xmp.contains(r#"crs:VignetteAmount="+35""#));
        assert!(xmp.contains(r#"crs:VignetteMidpoint="60""#));
        // A neutral recipe emits neither key (byte-compatible with the old writer).
        let neutral = recipe_to_xmp(&EditRecipe::default());
        assert!(!neutral.contains("VignetteAmount") && !neutral.contains("VignetteMidpoint"));
    }

    #[test]
    fn renders_manual_distortion_only_when_set() {
        let r = EditRecipe { lens_distortion: -24.0, ..Default::default() };
        assert!(recipe_to_xmp(&r).contains(r#"crs:LensManualDistortionAmount="-24""#));
        let pos = EditRecipe { lens_distortion: 80.0, ..Default::default() };
        assert!(recipe_to_xmp(&pos).contains(r#"crs:LensManualDistortionAmount="+80""#));
        // Zero amount emits no key at all (byte-compatible with the old writer).
        assert!(!recipe_to_xmp(&EditRecipe::default()).contains("LensManualDistortionAmount"));
    }

    #[test]
    fn bitmap_masks_are_skipped_by_the_xmp_writer() {
        use crate::recipe::MaskGeometry;
        // Mixed parametric + raster: only the parametric mask is written.
        let mixed = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    exposure_ev: -1.0,
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Bitmap { path: "out/subject.png".into() },
                    exposure_ev: 0.6,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&mixed);
        assert!(xmp.contains("Mask/Gradient"), "the parametric mask must survive");
        assert_eq!(xmp.matches("crs:What=\"Correction\"").count(), 1, "raster correction skipped");
        assert!(!xmp.contains("subject.png"), "no raster path may leak into the sidecar");
        // All-raster: the whole corrections block disappears (no empty shell).
        let all_bitmap = EditRecipe {
            masks: vec![LocalAdjustment {
                mask: MaskGeometry::Bitmap { path: "out/sky.png".into() },
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!recipe_to_xmp(&all_bitmap).contains("MaskGroupBasedCorrections"));
    }

    #[test]
    fn renders_range_masks_as_intersected_components() {
        use crate::recipe::RangeMask;
        let r = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    range: Some(RangeMask::Luminance { lo_outer: 0.4, lo: 0.5, hi: 1.0, hi_outer: 1.0 }),
                    name: "sky".into(),
                    highlights: -40.0,
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Radial {
                        top: 0.3, left: 0.35, bottom: 0.7, right: 0.65,
                        feather: 0.5, roundness: 0.0, flipped: false,
                    },
                    range: Some(RangeMask::Color { r: 0.9, g: 0.6, b: 0.2, amount: 0.5, px: 0.4, py: 0.7 }),
                    name: "subject".into(),
                    saturation: 20.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        // Both range components present, encoded as intersections (the decoded
        // ACR algebra: BlendMode 1 + Inverted true + Value 0 = keep only where
        // the range matches).
        assert_eq!(xmp.matches(r#"crs:What="Mask/RangeMask""#).count(), 2);
        assert_eq!(
            xmp.matches(r#"crs:MaskBlendMode="1" crs:MaskInverted="true""#).count(), 2
        );
        // Luminance: attribute form, LumRange in ACR's 4-number trapezoid.
        assert!(xmp.contains(r#"crs:Type="2""#));
        assert!(xmp.contains(r#"crs:LumRange="0.400000 0.500000 1.000000 1.000000""#));
        // Colour: child-element form with one PointModels entry.
        assert!(xmp.contains(r#"crs:Type="1""#));
        assert!(xmp.contains(r#"crs:ColorAmount="0.500000""#));
        assert!(xmp.contains("<rdf:li>0.900000 0.600000 0.200000 0.400000 0.700000 0</rdf:li>"));
        // A mask WITHOUT a range emits no RangeMask component at all.
        let plain = EditRecipe {
            masks: vec![LocalAdjustment { name: "plain".into(), ..Default::default() }],
            ..Default::default()
        };
        assert!(!recipe_to_xmp(&plain).contains("RangeMask"));
    }

    #[test]
    fn renders_expected_crs_keys() {
        let r = EditRecipe {
            exposure_ev: 0.32,
            contrast: 14.0,
            highlights: -12.0,
            temperature_k: Some(5600.0),
            tint: 3.0,
            sharpening: 45.0, // -> Sharpness 30
            tone_curve: vec![
                CurvePoint { input: 0, output: 0 },
                CurvePoint { input: 255, output: 255 },
            ],
            rationale: "warm & contrasty <test> & \"q\"".into(),
            confidence: 0.82,
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        assert!(xmp.contains(r#"crs:ProcessVersion="15.4""#));
        assert!(xmp.contains(r#"crs:Exposure2012="0.32""#));
        assert!(xmp.contains(r#"crs:Contrast2012="+14""#));
        assert!(xmp.contains(r#"crs:Highlights2012="-12""#));
        assert!(xmp.contains(r#"crs:WhiteBalance="Custom""#));
        assert!(xmp.contains(r#"crs:Temperature="5600""#));
        assert!(xmp.contains(r#"crs:Sharpness="30""#)); // 45 * 2/3
        assert!(xmp.contains("<crs:ToneCurvePV2012>"));
        assert!(xmp.contains("<rdf:li>0, 0</rdf:li>"));
        // rationale is XML-escaped in the comment
        assert!(xmp.contains("&lt;test&gt;"));
    }

    #[test]
    fn renders_hsl_bands_only_when_set() {
        let r = EditRecipe {
            hsl: crate::recipe::Hsl {
                hue: [0.0, 15.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], // orange +15
                saturation: [0.0, 0.0, 0.0, -40.0, 0.0, 0.0, 0.0, 0.0], // green -40
                ..Default::default()
            },
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        assert!(xmp.contains(r#"crs:HueAdjustmentOrange="+15""#));
        assert!(xmp.contains(r#"crs:SaturationAdjustmentGreen="-40""#));
        assert!(xmp.contains(r#"crs:LuminanceAdjustmentRed="0""#)); // full 24-key block
        // A neutral recipe emits NO HSL keys (minimal, v1-compatible sidecar).
        assert!(!recipe_to_xmp(&EditRecipe::default()).contains("HueAdjustment"));
    }

    #[test]
    fn renders_color_grade_with_verified_split_toning_mapping() {
        let r = EditRecipe {
            color_grade: crate::recipe::ColorGrade {
                shadow_hue: 220.0, shadow_sat: 30.0,
                highlight_hue: 45.0, highlight_sat: 20.0,
                midtone_lum: -10.0, balance: 15.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        // shadow/highlight hue+sat round-trip via the legacy SplitToning* keys
        assert!(xmp.contains(r#"crs:SplitToningShadowHue="220""#));
        assert!(xmp.contains(r#"crs:SplitToningShadowSaturation="30""#));
        assert!(xmp.contains(r#"crs:SplitToningHighlightHue="45""#));
        assert!(xmp.contains(r#"crs:SplitToningBalance="+15""#));
        // lum / midtone / global / blending via ColorGrade*
        assert!(xmp.contains(r#"crs:ColorGradeMidtoneLum="-10""#));
        assert!(xmp.contains(r#"crs:ColorGradeBlending="50""#)); // ACR default
        // A neutral recipe emits NO grading keys at all.
        let neutral = recipe_to_xmp(&EditRecipe::default());
        assert!(!neutral.contains("ColorGrade") && !neutral.contains("SplitToning"));
    }

    #[test]
    fn renders_per_channel_rgb_curves() {
        let r = EditRecipe {
            red_curve: vec![CurvePoint { input: 0, output: 10 }, CurvePoint { input: 255, output: 250 }],
            blue_curve: vec![
                CurvePoint { input: 0, output: 0 },
                CurvePoint { input: 128, output: 110 },
                CurvePoint { input: 255, output: 255 },
            ],
            ..Default::default()
        };
        let xmp = recipe_to_xmp(&r);
        assert!(xmp.contains("<crs:ToneCurvePV2012Red>"));
        assert!(xmp.contains("<rdf:li>0, 10</rdf:li>"));
        assert!(xmp.contains("<crs:ToneCurvePV2012Blue>"));
        assert!(xmp.contains("<rdf:li>128, 110</rdf:li>"));
        // The empty green channel emits no element.
        assert!(!xmp.contains("ToneCurvePV2012Green"));
        // A neutral recipe emits no per-channel curves at all.
        assert!(!recipe_to_xmp(&EditRecipe::default()).contains("ToneCurvePV2012Red"));
    }

    // ── reader (xmp_to_recipe) ───────────────────────────────────────────────

    #[test]
    fn globals_round_trip_through_xmp() {
        // Values are chosen to survive the writer's documented rounding: integer
        // sliders (`signed()`), 2-decimal exposure, integer Kelvin, 1-decimal
        // straighten, %.6f crop — so the reader must land EXACTLY back.
        let r = EditRecipe {
            exposure_ev: 0.32,
            contrast: 14.0,
            highlights: -12.0,
            shadows: 25.0,
            whites: 8.0,
            blacks: -6.0,
            temperature_k: Some(5600.0),
            tint: 3.0,
            vibrance: 18.0,
            saturation: -5.0,
            clarity: 10.0,
            dehaze: 7.0,
            hsl: crate::recipe::Hsl {
                hue: [0.0, 15.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                saturation: [0.0, 0.0, 0.0, -40.0, 0.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            color_grade: crate::recipe::ColorGrade {
                shadow_hue: 220.0,
                shadow_sat: 30.0,
                highlight_hue: 45.0,
                highlight_sat: 20.0,
                midtone_lum: -10.0,
                balance: 15.0,
                ..Default::default()
            },
            sharpening: 45.0, // → crs 30 → ×1.5 → 45 exactly
            noise_reduction: 20.0,
            lens_vignette: 35.0,
            lens_vignette_mid: 60.0,
            lens_distortion: -24.0,
            straighten_deg: 1.5,
            crop: Some(Crop { left: 0.05, top: 0.0, right: 0.95, bottom: 1.0 }),
            tone_curve: vec![
                CurvePoint { input: 0, output: 8 },
                CurvePoint { input: 255, output: 247 },
            ],
            red_curve: vec![
                CurvePoint { input: 0, output: 10 },
                CurvePoint { input: 255, output: 250 },
            ],
            rationale: "warm & contrasty <test> & \"q\"".into(),
            confidence: 0.82,
            ..Default::default()
        };
        let back = xmp_to_recipe(&recipe_to_xmp(&r));
        assert_eq!(back, r);
    }

    #[test]
    fn as_shot_tint_round_trips_only_for_our_own_sidecars() {
        // Our writer emits a non-neutral Tint even under "As Shot"; the Autoshop
        // marker tells the reader it is a real edit.
        let r = EditRecipe { tint: 3.0, ..Default::default() };
        assert_eq!(xmp_to_recipe(&recipe_to_xmp(&r)).tint, 3.0);
    }

    #[test]
    fn parametric_masks_round_trip_through_xmp() {
        let r = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    range: Some(RangeMask::Luminance { lo_outer: 0.4, lo: 0.5, hi: 1.0, hi_outer: 1.0 }),
                    name: "sky & sea".into(),
                    amount: 0.75,
                    inverted: true,
                    exposure_ev: -0.4, // ÷4 → ×4 is a power-of-two rescale: exact
                    contrast: 30.0,    // "0.3" ×100 needs the 4-decimal snap: exact
                    highlights: -50.0,
                    shadows: 60.0,
                    whites: 10.0,
                    blacks: -20.0,
                    clarity: 40.0,
                    dehaze: 5.0,
                    texture: 15.0,
                    saturation: 20.0,
                    temperature: 25.0,
                    tint: -10.0,
                    noise_reduction: 30.0,
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Radial {
                        top: 0.3, left: 0.35, bottom: 0.7, right: 0.65,
                        feather: 0.5, roundness: 0.0, flipped: true,
                    },
                    range: Some(RangeMask::Color { r: 0.9, g: 0.6, b: 0.2, amount: 0.5, px: 0.4, py: 0.7 }),
                    name: "subject".into(),
                    shadows: 20.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let back = xmp_to_recipe(&recipe_to_xmp(&r));
        assert_eq!(back.masks, r.masks);
    }

    #[test]
    fn bitmap_masks_do_not_come_back_from_xmp() {
        // The writer skips raster corrections (no classic-XMP encoding), so the
        // reader must return only the parametric mask — never a phantom.
        let mixed = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    exposure_ev: -1.0,
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Bitmap { path: "out/subject.png".into() },
                    exposure_ev: 0.6,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let back = xmp_to_recipe(&recipe_to_xmp(&mixed));
        assert_eq!(back.masks.len(), 1);
        assert_eq!(back.masks[0].mask, mixed.masks[0].mask);
        assert_eq!(back.masks[0].exposure_ev, -1.0);
    }

    #[test]
    fn foreign_as_shot_sidecar_imports_no_wb_and_drops_identity_curves() {
        // A Lightroom-style sidecar (no Autoshop marker): "As Shot" Temperature
        // and Tint are the CAMERA's values, not edits — they must NOT import.
        // LR also always writes the master curve; the 2-point identity means
        // "no curve" and must collapse to empty.
        let lr = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    crs:WhiteBalance="As Shot"
    crs:Temperature="5150"
    crs:Tint="+10"
    crs:Exposure2012="+0.65"
    crs:Contrast2012="+22"
    crs:Sharpness="40"
    crs:HasSettings="True">
   <crs:ToneCurvePV2012>
    <rdf:Seq>
     <rdf:li>0, 0</rdf:li>
     <rdf:li>255, 255</rdf:li>
    </rdf:Seq>
   </crs:ToneCurvePV2012>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;
        let r = xmp_to_recipe(lr);
        assert_eq!(r.temperature_k, None, "as-shot Kelvin is not an edit");
        assert_eq!(r.tint, 0.0, "as-shot tint is not an edit");
        assert_eq!(r.exposure_ev, 0.65);
        assert_eq!(r.contrast, 22.0);
        assert_eq!(r.sharpening, 60.0); // crs 40 × 1.5
        assert!(r.tone_curve.is_empty(), "identity curve must collapse");
        // A Custom-WB foreign sidecar DOES import its Kelvin + tint.
        let custom = lr.replace("As Shot", "Custom");
        let rc = xmp_to_recipe(&custom);
        assert_eq!(rc.temperature_k, Some(5150.0));
        assert_eq!(rc.tint, 10.0);
    }
}
