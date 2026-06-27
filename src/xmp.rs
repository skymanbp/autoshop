//! XMP sidecar writer — render an [`EditRecipe`] as an Adobe Camera Raw /
//! Lightroom `.xmp` sidecar (the `crs:` namespace), so the AI's edit opens as
//! adjustable develop sliders in the user's catalog.
//!
//! Key names, value conventions, and structure were verified against a real ACR
//! sidecar from the user's own library (`DSC08724.xmp`): `ProcessVersion=15.4`,
//! signed-integer sliders, `Sharpness` on 0..100, tone curve as an `rdf:Seq` of
//! `"x, y"` strings (see `docs/M1_PLAN.md` §5 and §9). We emit only the keys we
//! set; Lightroom fills the rest from defaults.

use crate::recipe::{EditRecipe, MaskGeometry};

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

/// `(crs:What value, extra geometry attributes)` for a mask geometry.
/// Coordinates are written raw (unclamped) — ACR gradients legitimately use
/// values outside [0,1].
fn mask_geom_xml(g: &MaskGeometry) -> (&'static str, String) {
    match g {
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => (
            "Mask/Gradient",
            format!(
                " crs:ZeroX=\"{zero_x}\" crs:ZeroY=\"{zero_y}\" crs:FullX=\"{full_x}\" crs:FullY=\"{full_y}\""
            ),
        ),
        MaskGeometry::Radial { top, left, bottom, right, feather, roundness, flipped } => (
            "Mask/CircularGradient",
            format!(
                " crs:Top=\"{top}\" crs:Left=\"{left}\" crs:Bottom=\"{bottom}\" crs:Right=\"{right}\" \
crs:Feather=\"{feather}\" crs:Roundness=\"{roundness}\" crs:Flipped=\"{flipped}\""
            ),
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
        let (what, geom) = mask_geom_xml(&m.mask);
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
        </rdf:Seq>\n\
       </crs:CorrectionMasks>\n\
      </rdf:Description>\n\
     </rdf:li>\n",
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

    // recipe sharpening is 0..150; crs Sharpness is 0..100 — rescale + clamp.
    let sharp = ((r.sharpening * 2.0 / 3.0).round() as i64).clamp(0, 100);
    attr(&mut a, "Sharpness", &sharp.to_string());
    let nr = (r.noise_reduction.round() as i64).clamp(0, 100);
    attr(&mut a, "LuminanceSmoothing", &nr.to_string());

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

    // Tone curve is a child element (rdf:Seq of "x, y" strings), not an attribute.
    let tone = if r.tone_curve.is_empty() {
        String::new()
    } else {
        let pts: String = r
            .tone_curve
            .iter()
            .map(|p| format!("     <rdf:li>{}, {}</rdf:li>\n", p.input, p.output))
            .collect();
        format!(
            "\n   <crs:ToneCurvePV2012>\n    <rdf:Seq>\n{pts}    </rdf:Seq>\n   </crs:ToneCurvePV2012>"
        )
    };

    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Autoshop\">\n\
 <!-- Generated by Autoshop. AI rationale: {rationale} (confidence {conf:.2}) -->\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"{attrs}\n\
    crs:HasSettings=\"True\">{tone}{masks}\n\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n",
        rationale = xml_escape(&r.rationale),
        conf = r.confidence,
        attrs = a,
        tone = tone,
        masks = masks_xml(r),
    )
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
}
