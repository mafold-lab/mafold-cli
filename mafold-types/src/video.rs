//! Seedance on BytePlus ModelArk: which aspect ratio a job may ask for, by its
//! model and the roles of its refs — and what a clip costs (further down: the
//! vendor's count and list price, one copy for the api's estimate and the
//! cli's own-key quote and bill check). One copy for both doors a submit goes
//! through — the api's `videoSubmit` (Mafold's key, and the dry run `mafold
//! video submit` asks before every own-key submit) and mafold-core's
//! `ark-video` driver (the owner's own key, also reached by a direct
//! `mafold connection call`).
//!
//! ModelArk's own table (Create video generation task → `ratio`, read
//! 2026-10-30):
//!
//! ```text
//!                                   Seedance 2.5          Seedance 2.0 family
//! text-to-video                     adaptive or fixed     adaptive or fixed
//! first frame / first+last frame    adaptive only (1)     adaptive or fixed (2)
//! reference-to-video                adaptive or fixed     adaptive or fixed
//!   (reference_image / _video / _audio)
//! video edit / extend               adaptive only (3)     adaptive or fixed
//! ```
//!
//! (1) 2.5 keeps the aspect of the image given as `first_frame`.
//! (2) 2.0 center-crops the frame to the ratio asked for.
//! (3) 2.5 picks the video to edit or extend from the prompt and keeps its
//!     aspect. A submit can't tell an edit from a reference — both are a
//!     `reference_video` — so a fixed ratio with a reference video goes out
//!     (reference-to-video is the documented use, e.g. the director stage's
//!     animatic), and an edit / extend on 2.5 gets the vendor's own answer.
//!
//! Until 2026-10-30 both doors refused ANY ref with a fixed ratio ("the vendor
//! forces adaptive whenever a frame / reference is given"): an agent shooting a
//! vertical film on 2.5 with four `reference_image`s was held to adaptive, got
//! 16:9, and its retry with `--ratio 9:16` was refused.

/// Seedance 2.5 under any dated build — `dreamina-seedance-2-5-260628` today.
fn is_seedance_2_5(model: &str) -> bool {
    match model.strip_prefix("dreamina-seedance-2-5") {
        Some(rest) => rest.is_empty() || rest.starts_with('-'),
        None => false,
    }
}

/// May a job on `model`, with refs in `roles`, ask for `ratio`? `Err` is the
/// sentence to refuse with, before anything is sent or held. Only Seedance 2.5
/// with a `first_frame` / `last_frame` is held to `adaptive`; every other model
/// and role takes the ratio as asked (the table above). Whether `ratio` is one
/// of the vendor's values at all is the caller's check.
pub fn check_ratio<'a>(model: &str, ratio: &str, roles: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    if ratio == "adaptive" || !is_seedance_2_5(model) {
        return Ok(());
    }
    if roles.into_iter().any(|r| r == "first_frame" || r == "last_frame") {
        return Err(format!(
            "Seedance 2.5 with a first_frame / last_frame keeps the first frame's aspect, so ratio must be \
             adaptive (got {ratio}) — give the first frame the shape you want, or pass the image as a \
             reference_image, which takes any ratio"
        ));
    }
    Ok(())
}

// ── what a clip costs ──────────────────────────────────────────────────────
//
// ModelArk's own price page (Model pricing → Video generation, read
// 2026-10-31), USD per million tokens:
//
// ```text
//                        480p / 720p   1080p   input includes a video
// Seedance 2.5              10.7        11.7    6.4 · 7.0 at 1080p
// Seedance 2.0               7.0         7.7    4.3 · 4.7 at 1080p
// Seedance 2.0 fast          5.6          —     3.3
// Seedance 2.0 mini          3.5          —     2.1
// ```
//
// Sound is not priced apart on these: a clip with audio bills the same count
// at the same rate as a silent one (2026-10-29: 2.5 · 1080p · 5 s with audio =
// 245,025 tokens, 2.0 · 720p · 5 s with audio = 108,900 — the formula, to the
// token).
//
// The count: the clip's frames (24 × seconds + 1) × the frame's pixels / 1024.
// A reference video adds its own seconds × 24 frames, counted at the OUTPUT's
// size and never under 4 s' worth ("minimum token consumption applies when the
// input includes video"; 2.0's table prices a 2–4 s input alike) — 2026-10-30:
// 2.0 · 480p · 5 s with a 5 s animatic = 100,858 = 864 × 496 × (121 + 120) / 1024,
// against a dry run that had said 48,437. Reference images add nothing (the
// 10-30 film: four of them on every scene, each bill the plain formula).

/// Which line of the price page a model is on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Line {
    V25,
    V20,
    V20Fast,
    V20Mini,
}

fn line(model: &str) -> Option<Line> {
    if is_seedance_2_5(model) {
        return Some(Line::V25);
    }
    let rest = model.strip_prefix("dreamina-seedance-2-0")?;
    if rest.starts_with("-fast") {
        Some(Line::V20Fast)
    } else if rest.starts_with("-mini") {
        Some(Line::V20Mini)
    } else if rest.is_empty() || rest.starts_with('-') {
        Some(Line::V20)
    } else {
        None
    }
}

/// The least a reference video is counted at, in seconds.
pub const MIN_INPUT_VIDEO_SECS: f64 = 4.0;

/// The pixels ModelArk counts one frame at. A tier is a pixel budget, not a
/// height — 720p is 1280 × 720 landscape and 720 × 1280 portrait — so the
/// shape never moves the price. 480p differs by model: 2.0 renders 16:9 at
/// 864 × 496 (every 2.0 480p bill since 10-29), 2.5 at 854 × 480 (10-06).
pub fn frame_pixels(model: &str, resolution: &str) -> u64 {
    match resolution {
        "1080p" => 1920 * 1080,
        "480p" => match line(model) {
            Some(Line::V20 | Line::V20Fast | Line::V20Mini) => 864 * 496,
            _ => 854 * 480,
        },
        _ => 1280 * 720,
    }
}

/// ModelArk's token count for a clip of `secs` seconds (≤ 0 = the model picks;
/// counted as 5) with reference video(s) totalling `input_video_secs`.
pub fn clip_tokens(model: &str, resolution: &str, secs: i64, input_video_secs: Option<f64>) -> u64 {
    let secs = if secs <= 0 { 5 } else { secs as u64 };
    let mut frames = 24 * secs + 1;
    if let Some(input) = input_video_secs {
        frames += (24.0 * input.max(MIN_INPUT_VIDEO_SECS)).round() as u64;
    }
    frame_pixels(model, resolution) * frames / 1024
}

/// ModelArk's list price, USD per million tokens, for `model` at `resolution`
/// — the lower rate when the input includes a video. `None`: a model or tier
/// not on the price page (fast and mini don't do 1080p; an `ep-…` endpoint).
pub fn list_usd_per_m(model: &str, resolution: &str, video_input: bool) -> Option<f64> {
    let (plain, with_video) = match (line(model)?, resolution == "1080p") {
        (Line::V25, false) => (10.7, 6.4),
        (Line::V25, true) => (11.7, 7.0),
        (Line::V20, false) => (7.0, 4.3),
        (Line::V20, true) => (7.7, 4.7),
        (Line::V20Fast, false) => (5.6, 3.3),
        (Line::V20Mini, false) => (3.5, 2.1),
        (Line::V20Fast | Line::V20Mini, true) => return None,
    };
    Some(if video_input { with_video } else { plain })
}

/// How a bill compares with what was quoted: `"match"` within half a percent
/// (a frame's rounding); `close: …` within 5% — an `adaptive` clip takes its
/// first frame's shape, and a shape off the tier's own moves the count a few
/// percent (10-05: 2.5 · 480p · 4 s from a photo, 39,749 against 38,830);
/// past that `OFF: …`, the gap in words a person can act on.
pub fn check_bill(quoted: u64, billed: u64) -> String {
    if quoted == 0 {
        return "no quote to compare with".to_string();
    }
    let gap = billed as f64 / quoted as f64 - 1.0;
    let pct = gap * 100.0;
    if gap.abs() <= 0.005 {
        "match".to_string()
    } else if gap.abs() <= 0.05 {
        format!("close: billed {billed} against a quote of {quoted} ({pct:+.1}%) — the frame's shape moves the count a little")
    } else {
        format!(
            "OFF: billed {billed} against a quote of {quoted} ({pct:+.0}%) — the estimate missed something; say so \
             to whoever was quoted"
        )
    }
}

#[cfg(test)]
mod video_tests {
    use super::*;

    const V25: &str = "dreamina-seedance-2-5-260628";
    const V20: &str = "dreamina-seedance-2-0-260128";

    /// Every count below is a real ModelArk bill.
    #[test]
    fn the_count_is_the_vendors_to_the_token() {
        assert_eq!(clip_tokens(V25, "720p", 5, None), 108_900, "09-12");
        assert_eq!(clip_tokens(V25, "480p", 4, None), 38_830, "10-06 dev e2e");
        assert_eq!(clip_tokens(V25, "1080p", 6, None), 293_625, "10-30, 9:16");
        assert_eq!(clip_tokens(V25, "1080p", 11, None), 536_625, "10-30 film, scene 8");
        assert_eq!(clip_tokens(V20, "480p", 5, None), 50_638, "10-30 director A");
        assert_eq!(clip_tokens(V20, "480p", 4, None), 40_594, "10-29");
        assert_eq!(clip_tokens(V20, "480p", 15, None), 151_078, "10-30 reshoot");
        assert_eq!(clip_tokens(V20, "1080p", 5, None), 245_025, "10-29, with audio");
        // A 5 s animatic as the reference video (director B and C).
        assert_eq!(clip_tokens(V20, "480p", 5, Some(5.0)), 100_858, "10-30 director B/C");
        // A 2 s reference counts as 4 s.
        assert_eq!(clip_tokens(V20, "480p", 5, Some(2.0)), clip_tokens(V20, "480p", 5, Some(4.0)));
        // The shape doesn't move it; "the model picks" is priced as 5 s.
        assert_eq!(clip_tokens(V25, "720p", -1, None), 108_900);
    }

    #[test]
    fn prices_are_the_vendors_page() {
        assert_eq!(list_usd_per_m(V25, "720p", false), Some(10.7));
        assert_eq!(list_usd_per_m(V25, "480p", false), Some(10.7));
        assert_eq!(list_usd_per_m(V25, "1080p", false), Some(11.7));
        assert_eq!(list_usd_per_m(V25, "1080p", true), Some(7.0));
        assert_eq!(list_usd_per_m(V25, "720p", true), Some(6.4));
        assert_eq!(list_usd_per_m(V20, "480p", false), Some(7.0));
        assert_eq!(list_usd_per_m(V20, "480p", true), Some(4.3));
        assert_eq!(list_usd_per_m(V20, "1080p", true), Some(4.7));
        assert_eq!(list_usd_per_m("dreamina-seedance-2-0-fast-260128", "720p", false), Some(5.6));
        assert_eq!(list_usd_per_m("dreamina-seedance-2-0-mini-260615", "480p", true), Some(2.1));
        assert_eq!(list_usd_per_m("dreamina-seedance-2-0-mini-260615", "1080p", false), None, "no mini at 1080p");
        assert_eq!(list_usd_per_m("ep-20261030123456-abcde", "720p", false), None);
    }

    #[test]
    fn a_bill_is_checked_against_its_quote() {
        assert_eq!(check_bill(108_900, 108_900), "match");
        assert_eq!(check_bill(48_437, 48_437 + 100), "match", "a frame's rounding");
        let close = check_bill(38_830, 39_749);
        assert!(close.starts_with("close") && close.contains("+2.4%"), "{close}");
        let off = check_bill(48_437, 100_858);
        assert!(off.starts_with("OFF") && off.contains("+108%"), "{off}");
        assert_eq!(check_bill(0, 5), "no quote to compare with");
    }

    fn check(model: &str, ratio: &str, roles: &[&str]) -> Result<(), String> {
        check_ratio(model, ratio, roles.iter().copied())
    }

    /// 2026-10-30: a vertical film on 2.5 from four reference images.
    #[test]
    fn references_take_a_fixed_ratio_on_every_model() {
        for m in [V25, V20, "dreamina-seedance-2-0-fast-260128", "dreamina-seedance-2-0-mini-260615"] {
            assert_eq!(check(m, "9:16", &["reference_image"; 4]), Ok(()), "{m}");
            assert_eq!(check(m, "9:16", &["reference_video", "reference_image", "reference_audio"]), Ok(()), "{m}");
            assert_eq!(check(m, "21:9", &[]), Ok(()), "{m}: text-to-video");
        }
    }

    #[test]
    fn only_2_5_with_a_frame_is_held_to_adaptive() {
        let frames: [&[&str]; 3] = [&["first_frame"], &["first_frame", "last_frame"], &["last_frame"]];
        for roles in frames {
            let e = check(V25, "9:16", roles).unwrap_err();
            assert!(e.contains("Seedance 2.5") && e.contains("first_frame") && e.contains("adaptive"), "{e}");
            assert!(e.contains("9:16"), "says what it was asked: {e}");
            assert_eq!(check(V25, "adaptive", roles), Ok(()), "{roles:?}");
            // 2.0 center-crops the frame to the ratio.
            assert_eq!(check(V20, "9:16", roles), Ok(()), "{roles:?}");
            assert_eq!(check(V20, "adaptive", roles), Ok(()), "{roles:?}");
        }
    }

    /// An endpoint id (`ep-…`) or anything unknown isn't 2.5 as far as we can
    /// tell: it goes out and the vendor answers for it.
    #[test]
    fn the_2_5_family_is_known_by_its_id() {
        assert!(is_seedance_2_5(V25));
        assert!(is_seedance_2_5("dreamina-seedance-2-5"));
        assert!(!is_seedance_2_5(V20));
        assert!(!is_seedance_2_5("ep-20261030123456-abcde"));
        assert_eq!(check("ep-20261030123456-abcde", "9:16", &["first_frame"]), Ok(()));
    }
}
