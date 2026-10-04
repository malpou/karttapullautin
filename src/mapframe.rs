//! The map frame: the sheet's resolution and map scale. At 600 dpi and 1:10 000 one map
//! inch is 254 ground metres, so the sheet has 600 / 254 pixels per ground metre.
//!
//! The conversions keep the operator order the render sites always had
//! (`d * dpi / 254 / (scale / 10 000)` and its inverse) because `(d * 600.0) / 254.0` and
//! `d * (600.0 / 254.0)` round differently; `px_per_metre()` is only for sites that
//! already computed the fused factor first. The north lines are the exception: they now
//! follow the map scale through `to_px` (off by default).

use std::io::{BufRead, Write};
use std::path::Path;

use crate::io::fs::FileSystem;

/// Ground metres covered by one map inch at 1:10 000.
const GROUND_METRES_PER_INCH_AT_10K: f64 = 254.0;

/// The rendered sheet's resolution and map scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapFrame {
    /// Dots per map inch of the rendered sheet (600; no ini key).
    pub dpi: f64,
    /// The map scale's denominator, 10 000 for 1:10 000 (ini `scalefactor` x 10 000).
    pub scale_denominator: f64,
}

impl Default for MapFrame {
    fn default() -> Self {
        Self {
            dpi: 600.0,
            scale_denominator: 10_000.0,
        }
    }
}

impl MapFrame {
    /// The map scale as a multiple of 1:10 000, which the 254 m inch is given for.
    fn per_10k(&self) -> f64 {
        self.scale_denominator / 10_000.0
    }

    /// Pixels per ground metre, as one fused factor.
    pub fn px_per_metre(&self) -> f64 {
        self.dpi / GROUND_METRES_PER_INCH_AT_10K / self.per_10k()
    }

    /// Ground metres per pixel, as one fused factor.
    pub fn metres_per_px(&self) -> f64 {
        GROUND_METRES_PER_INCH_AT_10K / self.dpi * self.per_10k()
    }

    /// A ground length in metres as sheet pixels, multiplying first.
    pub fn to_px(&self, metres: f64) -> f64 {
        metres * self.dpi / GROUND_METRES_PER_INCH_AT_10K / self.per_10k()
    }

    /// A sheet length in pixels as ground metres: the inverse of [`Self::to_px`].
    pub fn to_metres(&self, px: f64) -> f64 {
        px / self.dpi * GROUND_METRES_PER_INCH_AT_10K * self.per_10k()
    }
}

/// A six-line world file: pixel size x, rotation, rotation, pixel size y, x origin, y origin.
///
/// The world file places the map frame in ground coordinates; the origin is the centre of the
/// upper-left pixel.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldFile {
    pub pixel_size_x: f64,
    pub rotation_y: f64,
    pub rotation_x: f64,
    pub pixel_size_y: f64,
    pub x_origin: f64,
    pub y_origin: f64,
}

impl WorldFile {
    /// Parse six lines; each line is trimmed (so `\r\n` files work). Errors on fewer than six
    /// lines or a value that is not an f64. Lines after the sixth are ignored.
    pub fn parse<R: BufRead>(reader: R) -> anyhow::Result<Self> {
        let mut values = [0.0f64; 6];
        let mut lines = reader.lines();
        for (i, value) in values.iter_mut().enumerate() {
            let Some(line) = lines.next() else {
                anyhow::bail!("world file has only {i} lines, expected 6");
            };
            let line = line?;
            let text = line.trim();
            *value = text.parse::<f64>().map_err(|e| {
                anyhow::anyhow!("world file line {}: {text:?} is not a number: {e}", i + 1)
            })?;
        }
        Ok(Self {
            pixel_size_x: values[0],
            rotation_y: values[1],
            rotation_x: values[2],
            pixel_size_y: values[3],
            x_origin: values[4],
            y_origin: values[5],
        })
    }

    /// Open `path` on `fs` and parse it as a world file.
    pub fn read(fs: &impl FileSystem, path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let reader = fs
            .open(path)
            .map_err(|e| anyhow::anyhow!("cannot open world file {}: {e}", path.display()))?;
        Self::parse(reader)
    }

    /// Write the six values with `{}` formatting and `\r\n` line endings, in field order.
    pub fn write<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        write!(
            writer,
            "{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n",
            self.pixel_size_x,
            self.rotation_y,
            self.rotation_x,
            self.pixel_size_y,
            self.x_origin,
            self.y_origin
        )
    }

    /// A north-up frame with square pixels of `pixel_size` ground metres and the given origin.
    pub fn north_up(pixel_size: f64, x_origin: f64, y_origin: f64) -> Self {
        Self {
            pixel_size_x: pixel_size,
            rotation_y: 0.0,
            rotation_x: 0.0,
            pixel_size_y: -pixel_size,
            x_origin,
            y_origin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_per_metre_is_the_literal() {
        let frame = MapFrame::default();
        assert_eq!(frame.px_per_metre().to_bits(), (600.0f64 / 254.0).to_bits());
        assert_eq!(
            frame.metres_per_px().to_bits(),
            (254.0f64 / 600.0).to_bits()
        );
    }

    #[test]
    fn a_coarser_scale_has_fewer_pixels_per_metre() {
        let frame = MapFrame {
            scale_denominator: 15_000.0,
            ..MapFrame::default()
        };
        let default = MapFrame::default().px_per_metre();
        assert!((frame.px_per_metre() - default * 2.0 / 3.0).abs() < 1e-15);
        assert_eq!(frame.to_px(381.0), 600.0);
        assert_eq!(frame.to_metres(600.0), 381.0);
    }

    /// Documents why the frame converts in two ways: the fused factor rounds some values
    /// differently from multiplying first, so each site keeps its order. At 1:10 000 the
    /// conversions are the old `d * 600 / 254 / 1` and its inverse, bit for bit.
    #[test]
    fn operator_order_is_preserved() {
        let frame = MapFrame::default();
        for d in [3.0, 1234.567, 98765.4321, 0.1] {
            assert_eq!(
                frame.to_px(d).to_bits(),
                (d * 600.0 / 254.0 / 1.0).to_bits()
            );
            assert_eq!(
                frame.to_metres(d).to_bits(),
                (d / 600.0 * 254.0 * 1.0).to_bits()
            );
        }
        let any_differs = (1..1000).any(|i| {
            let d = i as f64 * 1.001;
            frame.to_px(d).to_bits() != (d * frame.px_per_metre()).to_bits()
        });
        assert!(any_differs, "fused and unfused conversions never differed");
    }

    #[test]
    fn parse_reads_crlf_and_write_reproduces_it() {
        let text = "2.5\r\n0\r\n0\r\n-2.5\r\n123456.25\r\n7891011.5\r\n";
        let w = WorldFile::parse(text.as_bytes()).unwrap();
        assert_eq!(
            w,
            WorldFile {
                pixel_size_x: 2.5,
                rotation_y: 0.0,
                rotation_x: 0.0,
                pixel_size_y: -2.5,
                x_origin: 123456.25,
                y_origin: 7891011.5,
            }
        );
        let mut out = Vec::new();
        w.write(&mut out).unwrap();
        assert_eq!(out, text.as_bytes());
        assert_eq!(WorldFile::parse(out.as_slice()).unwrap(), w);
    }

    #[test]
    fn parse_rejects_five_lines() {
        assert!(WorldFile::parse("1\r\n0\r\n0\r\n-1\r\n5\r\n".as_bytes()).is_err());
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(WorldFile::parse("1\r\n0\r\nabc\r\n-1\r\n5\r\n6\r\n".as_bytes()).is_err());
    }

    /// `vegetation.pgw` and `_vege.pgw`: one pixel per ground metre.
    #[test]
    fn unit_north_up_frame_text() {
        let mut out = Vec::new();
        WorldFile::north_up(1.0, 1.5, 2.0).write(&mut out).unwrap();
        assert_eq!(out, b"1\r\n0\r\n0\r\n-1\r\n1.5\r\n2\r\n");
    }

    /// `undergrowth.pgw`: the pixel pitch is the reciprocal of the f32 factor the raster is
    /// drawn with, printed with f64 digits; it survives a parse exactly.
    #[test]
    fn undergrowth_frame_text() {
        let tmpfactor = MapFrame::default().px_per_metre() as f32;
        let w = WorldFile::north_up(1.0 / f64::from(tmpfactor), 381234.0, 6671298.0);
        let mut out = Vec::new();
        w.write(&mut out).unwrap();
        assert_eq!(
            out,
            b"0.42333332155810494\r\n0\r\n0\r\n-0.42333332155810494\r\n381234\r\n6671298\r\n"
        );
        for scale_denominator in [5_000.0, 10_000.0, 13_000.0] {
            let frame = MapFrame {
                scale_denominator,
                ..MapFrame::default()
            };
            let tmpfactor = frame.px_per_metre() as f32;
            let w = WorldFile::north_up(1.0 / f64::from(tmpfactor), 381234.0, 6671298.0);
            let mut out = Vec::new();
            w.write(&mut out).unwrap();
            assert_eq!(WorldFile::parse(out.as_slice()).unwrap(), w);
        }
    }
}
