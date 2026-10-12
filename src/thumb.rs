//! What a picture is sent as — `.docs/thumbnail-v1.md` Phase 2, the cli's half.
//!
//! The RULE lives in `client-shared/thumbPlan.ts` for every TypeScript client;
//! this re-states it in Rust, and both test suites read the same case table
//! (`client-shared/thumbPlan.cases.json`), so the two can't drift.
//!
//! For each picture: the VIEW (fitted into 2560×2560, shrink only — what
//! everyone is shown and what models read), a 40px inline STRIP, and the
//! original only when asked for (`mafold attach --original`). Screenshots stay
//! lossless (WebP lossless); photos are JPEG q85 — the `image` crate encodes
//! WebP losslessly only, and JPEG is the fallback the rule names. Anything that
//! can't be decoded here (HEIC) goes up as it is, exactly as before.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader};

pub const VIEW_EDGE: u32 = 2560;
pub const STRIP_EDGE: u32 = 40;
pub const STRIP_QUALITY: f64 = 0.6;
pub const PHOTO_QUALITY: f64 = 0.85;
pub const SMALL_ENOUGH_BYTES: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Lossless,
    Lossy,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ViewPlan {
    Reuse,
    Encode { w: u32, h: u32, encoding: Encoding, quality: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadPlan {
    pub view: ViewPlan,
    pub strip: Size,
    pub offers_original: bool,
}

fn base_mime(mime: &str) -> String {
    mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

const LOSSLESS: &[&str] = &["image/png", "image/bmp", "image/x-ms-bmp", "image/tiff"];
const LOSSY: &[&str] = &["image/jpeg", "image/jpg", "image/pjpeg", "image/heic", "image/heif", "image/webp"];

pub fn derivable(mime: &str) -> bool {
    let m = base_mime(mime);
    LOSSLESS.contains(&m.as_str()) || LOSSY.contains(&m.as_str())
}

pub fn encoding_for(mime: &str) -> Encoding {
    if LOSSLESS.contains(&base_mime(mime).as_str()) {
        Encoding::Lossless
    } else {
        Encoding::Lossy
    }
}

/// Fit w×h inside edge×edge, ratio kept, never enlarged, never below 1px.
/// Rounds half away from zero, like the TypeScript `Math.round` on positives.
pub fn fit(w: u32, h: u32, edge: u32) -> Size {
    let long = w.max(h);
    if long <= edge {
        return Size { w: w.max(1), h: h.max(1) };
    }
    let k = edge as f64 / long as f64;
    Size {
        w: ((w as f64 * k).round() as u32).max(1),
        h: ((h as f64 * k).round() as u32).max(1),
    }
}

/// `planUpload` in client-shared/thumbPlan.ts.
pub fn plan_upload(w: u32, h: u32, mime: &str, bytes: u64) -> Option<UploadPlan> {
    if !derivable(mime) || w == 0 || h == 0 {
        return None;
    }
    let strip = fit(w, h, STRIP_EDGE);
    let encoding = encoding_for(mime);
    let fits = w.max(h) <= VIEW_EDGE;
    if fits && (bytes <= SMALL_ENOUGH_BYTES || encoding == Encoding::Lossy) {
        return Some(UploadPlan { view: ViewPlan::Reuse, strip, offers_original: false });
    }
    let size = fit(w, h, VIEW_EDGE);
    let quality = if encoding == Encoding::Lossless { 1.0 } else { PHOTO_QUALITY };
    Some(UploadPlan {
        view: ViewPlan::Encode { w: size.w, h: size.h, encoding, quality },
        strip,
        offers_original: !fits || encoding == Encoding::Lossy,
    })
}

/// One upload's parts: `view` is what everyone is shown.
pub struct Derived {
    pub view: Vec<u8>,
    pub view_mime: &'static str,
    pub strip: Option<Vec<u8>>,
    /// The original, when asked for and the view replaces it.
    pub orig: Option<Vec<u8>>,
}

/// `keepEncodedView`: keep the new view only if it came out smaller than the
/// original. One that didn't saves nobody a byte, so the original goes instead
/// (and is then the only copy) — even past the 2560 box, which is there to
/// save bytes, not to be met.
pub fn keep_encoded_view(plan: &UploadPlan, encoded_bytes: u64, original_bytes: u64) -> bool {
    matches!(plan.view, ViewPlan::Encode { .. }) && encoded_bytes < original_bytes
}

/// Decode, plan, encode. `None` = send the bytes as they are (not a picture
/// we derive from, or one this build can't decode).
pub fn derive(bytes: &[u8], mime: &str, original: bool) -> Option<Derived> {
    if !derivable(mime) {
        return None;
    }
    let img = decode_oriented(bytes)?;
    let (w0, h0) = (img.width(), img.height());
    let plan = plan_upload(w0, h0, mime, bytes.len() as u64)?;
    let strip = encode_jpeg(&img.thumbnail(plan.strip.w, plan.strip.h), STRIP_QUALITY);
    let as_is = |strip| Some(Derived { view: bytes.to_vec(), view_mime: "", strip, orig: None });
    let ViewPlan::Encode { w, h, encoding, quality } = plan.view else {
        return as_is(strip);
    };
    let scaled = if (w, h) == (w0, h0) { img } else { img.resize_exact(w, h, image::imageops::FilterType::Lanczos3) };
    let (view, view_mime) = match encoding {
        Encoding::Lossless => (encode_webp_lossless(&scaled)?, "image/webp"),
        Encoding::Lossy => (encode_jpeg(&scaled, quality)?, "image/jpeg"),
    };
    if !keep_encoded_view(&plan, view.len() as u64, bytes.len() as u64) {
        return as_is(strip);
    }
    Some(Derived { view, view_mime, strip, orig: (original && plan.offers_original).then(|| bytes.to_vec()) })
}

/// Decode with the EXIF orientation applied, so w×h are as displayed.
fn decode_oriented(bytes: &[u8]) -> Option<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Some(img)
}

fn encode_jpeg(img: &DynamicImage, quality: f64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let q = (quality * 100.0).round().clamp(1.0, 100.0) as u8;
    let rgb = img.to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode_image(&rgb).ok()?;
    Some(out)
}

fn encode_webp_lossless(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let rgba = img.to_rgba8();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        w: u32,
        h: u32,
        mime: String,
        bytes: u64,
        plan: Option<UploadPlan>,
    }

    /// The same table client-shared/thumbPlan.ts is tested against.
    #[test]
    fn the_rule_matches_the_shared_case_table() {
        #[derive(serde::Deserialize)]
        struct Table {
            cases: Vec<Case>,
        }
        let t: Table = serde_json::from_str(include_str!("../../client-shared/thumbPlan.cases.json")).unwrap();
        assert!(t.cases.len() >= 8);
        for c in t.cases {
            assert_eq!(plan_upload(c.w, c.h, &c.mime, c.bytes), c.plan, "case {:?}", c.name);
        }
    }

    /// The same `keep` table thumbPlan.spec.ts reads for `keepEncodedView`.
    #[test]
    fn keeping_the_view_matches_the_shared_case_table() {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Keep {
            name: String,
            w: u32,
            h: u32,
            mime: String,
            bytes: u64,
            encoded_bytes: u64,
            keep: bool,
        }
        #[derive(serde::Deserialize)]
        struct Table {
            keep: Vec<Keep>,
        }
        let t: Table = serde_json::from_str(include_str!("../../client-shared/thumbPlan.cases.json")).unwrap();
        assert!(t.keep.len() >= 5);
        for c in t.keep {
            let plan = plan_upload(c.w, c.h, &c.mime, c.bytes).expect(&c.name);
            assert!(matches!(plan.view, ViewPlan::Encode { .. }), "case {:?}", c.name);
            assert_eq!(keep_encoded_view(&plan, c.encoded_bytes, c.bytes), c.keep, "case {:?}", c.name);
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 7 * 30) as u8]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img).write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x / 16) as u8, (y / 12) as u8, 128]));
        encode_jpeg(&DynamicImage::ImageRgb8(img), 0.95).unwrap()
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        let i = image::load_from_memory(bytes).unwrap();
        (i.width(), i.height())
    }

    /// A screenshot bigger than the box: a lossless WebP view at 2560, the
    /// strip, and — only when asked — the untouched original beside it.
    #[test]
    fn a_big_screenshot_goes_up_as_a_lossless_view() {
        let shot = png(3200, 1800);
        let d = derive(&shot, "image/png", false).expect("derived");
        assert_eq!(d.view_mime, "image/webp");
        assert_eq!(dims(&d.view), (2560, 1440));
        assert!(d.view.starts_with(b"RIFF") && &d.view[12..16] == b"VP8L", "lossless WebP");
        assert_eq!(dims(d.strip.as_deref().unwrap()), (40, 23));
        assert!(d.orig.is_none(), "not asked for");
        assert_eq!(derive(&shot, "image/png", true).unwrap().orig.as_deref(), Some(&shot[..]));
    }

    #[test]
    fn a_big_photo_goes_up_as_a_jpeg_view() {
        let photo = jpeg(4032, 3024);
        let d = derive(&photo, "image/jpeg", true).expect("derived");
        assert_eq!(d.view_mime, "image/jpeg");
        assert_eq!(dims(&d.view), (2560, 1920));
        assert!(d.view.len() < photo.len());
        assert_eq!(d.orig.as_deref().map(<[u8]>::len), Some(photo.len()));
    }

    /// Within the box and small: the original IS the view — sent as is, with
    /// its strip, and there is no second copy to offer.
    #[test]
    fn a_small_picture_goes_up_as_it_is() {
        let small = png(800, 600);
        let d = derive(&small, "image/png", true).expect("derived");
        assert_eq!(d.view, small);
        assert_eq!(d.view_mime, "");
        assert!(d.orig.is_none());
        assert!(d.strip.is_some());
    }

    #[test]
    fn what_cant_be_derived_goes_up_untouched() {
        assert!(derive(b"GIF89a....", "image/gif", true).is_none());
        assert!(derive(b"not a picture", "image/jpeg", false).is_none(), "undecodable: as is");
    }
}
