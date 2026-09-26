//! Pixel comparison of two same-sized images.
//!
//! Every pair gets a changed-pixel count. When the two images together hold
//! few distinct colours (a classified raster such as `vegetation.png`, or a
//! reference map painted in the same palette), each colour is also treated
//! as a class and scored with intersection-over-union.

use std::collections::BTreeMap;
use std::collections::HashMap;

use image::{Rgb, RgbImage, RgbaImage};
use serde::Serialize;

use super::round6;

/// Above this many distinct colours the image is a rendered map, not a
/// classified raster, and per-colour scores are skipped.
pub const MAX_CLASSES: usize = 32;

/// Agreement for one colour class.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ClassAgreement {
    pub baseline_px: u64,
    pub candidate_px: u64,
    /// Pixels of this colour in both, over pixels of this colour in either.
    pub iou: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RasterComparison {
    pub width: u32,
    pub height: u32,
    pub changed_pixels: u64,
    pub changed_percent: f64,
    /// Per-colour IoU keyed by `#rrggbb` (or `#rrggbbaa` when not opaque);
    /// absent when the images hold more than [`MAX_CLASSES`] colours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<BTreeMap<String, ClassAgreement>>,
}

fn hex(c: [u8; 4]) -> String {
    let [r, g, b, a] = c;
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// Compare two images pixel by pixel.
pub fn compare(baseline: &RgbaImage, candidate: &RgbaImage) -> anyhow::Result<RasterComparison> {
    let (width, height) = baseline.dimensions();
    anyhow::ensure!(
        candidate.dimensions() == (width, height),
        "size differs: baseline {width}x{height}, candidate {}x{}",
        candidate.width(),
        candidate.height()
    );
    let mut changed = 0u64;
    // colour -> (baseline count, candidate count, count in both)
    let mut classes: Option<HashMap<[u8; 4], (u64, u64, u64)>> = Some(HashMap::new());
    for (b, c) in baseline.pixels().zip(candidate.pixels()) {
        if b != c {
            changed += 1;
        }
        if let Some(map) = classes.as_mut() {
            map.entry(b.0).or_default().0 += 1;
            let e = map.entry(c.0).or_default();
            e.1 += 1;
            if b == c {
                e.2 += 1;
            }
            if map.len() > MAX_CLASSES {
                classes = None;
            }
        }
    }
    let total = u64::from(width) * u64::from(height);
    let classes = classes.map(|map| {
        map.into_iter()
            .map(|(colour, (b, c, both))| {
                let class = ClassAgreement {
                    baseline_px: b,
                    candidate_px: c,
                    iou: round6(both as f64 / (b + c - both) as f64),
                };
                (hex(colour), class)
            })
            .collect()
    });
    Ok(RasterComparison {
        width,
        height,
        changed_pixels: changed,
        changed_percent: if total == 0 {
            0.0
        } else {
            round6(changed as f64 * 100.0 / total as f64)
        },
        classes,
    })
}

/// The baseline washed out to light grey, with changed pixels in red.
pub fn diff_image(baseline: &RgbaImage, candidate: &RgbaImage) -> RgbImage {
    RgbImage::from_fn(baseline.width(), baseline.height(), |x, y| {
        let b = baseline.get_pixel(x, y);
        if b != candidate.get_pixel(x, y) {
            return Rgb([255, 0, 0]);
        }
        let [r, g, bl, a] = b.0.map(u32::from);
        let luma = (r * 299 + g * 587 + bl * 114) / 1000;
        // composite on white, then keep a quarter of the contrast
        let on_white = (luma * a + 255 * (255 - a)) / 255;
        let v = (255 - (255 - on_white) / 4) as u8;
        Rgb([v, v, v])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    const GREEN: Rgba<u8> = Rgba([0, 200, 0, 255]);
    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);

    /// A 10x10 image whose first `green_cols` columns are green.
    fn striped(green_cols: u32) -> RgbaImage {
        RgbaImage::from_fn(10, 10, |x, _| if x < green_cols { GREEN } else { WHITE })
    }

    #[test]
    fn identical_images_have_no_change() {
        let r = compare(&striped(4), &striped(4)).unwrap();
        assert_eq!((r.changed_pixels, r.changed_percent), (0, 0.0));
        let classes = r.classes.unwrap();
        assert_eq!(classes["#00c800"].iou, 1.0);
        assert_eq!(classes["#ffffff"].iou, 1.0);
    }

    #[test]
    fn class_iou_is_exact() {
        // baseline green in columns 0..4, candidate in 0..6
        let r = compare(&striped(4), &striped(6)).unwrap();
        assert_eq!(r.changed_pixels, 20);
        assert_eq!(r.changed_percent, 20.0);
        let classes = r.classes.unwrap();
        let green = classes["#00c800"];
        assert_eq!((green.baseline_px, green.candidate_px), (40, 60));
        assert_eq!(green.iou, 0.666667);
        assert_eq!(classes["#ffffff"].iou, 0.666667);
    }

    #[test]
    fn many_colours_skip_classes() {
        let a = RgbaImage::from_fn(10, 10, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let r = compare(&a, &a).unwrap();
        assert!(r.classes.is_none());
        assert_eq!(r.changed_pixels, 0);
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let err = compare(&striped(1), &RgbaImage::new(5, 5)).unwrap_err();
        assert!(err.to_string().contains("size differs"), "{err}");
    }

    #[test]
    fn diff_image_marks_changes_red() {
        let d = diff_image(&striped(4), &striped(6));
        assert_eq!(*d.get_pixel(5, 0), Rgb([255, 0, 0]));
        assert_eq!(*d.get_pixel(9, 0), Rgb([255, 255, 255]));
        assert_ne!(*d.get_pixel(0, 0), Rgb([255, 0, 0]));
    }

    #[test]
    fn transparent_colour_keys_carry_alpha() {
        assert_eq!(hex([1, 2, 3, 255]), "#010203");
        assert_eq!(hex([1, 2, 3, 0]), "#01020300");
    }
}
