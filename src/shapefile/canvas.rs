use std::io::Write;

use tiny_skia::{PathBuilder, Transform};

use crate::io::fs::FileSystem;

pub struct Canvas<'a> {
    pixmap: tiny_skia::Pixmap,
    ppaint: tiny_skia::Paint<'a>,
    stroke: tiny_skia::Stroke,
}

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl Canvas<'_> {
    pub fn new(width: u32, height: u32) -> Self {
        Self::from_pixmap(tiny_skia::Pixmap::new(width, height).unwrap())
    }

    fn from_pixmap(pixmap: tiny_skia::Pixmap) -> Self {
        let mut ppaint = tiny_skia::Paint::default();
        ppaint.set_color(tiny_skia::Color::BLACK);
        ppaint.anti_alias = false;

        let stroke = tiny_skia::Stroke {
            width: 1.0,
            ..Default::default()
        };

        Canvas {
            pixmap,
            ppaint,
            stroke,
        }
    }

    #[inline]
    pub fn set_line_width(&mut self, width: f32) {
        self.stroke.width = width;
    }

    #[inline]
    pub fn set_color(&mut self, rgb: Color) {
        self.ppaint
            .set_color(tiny_skia::Color::from_rgba8(rgb.r, rgb.g, rgb.b, 255));
    }

    #[inline]
    pub fn set_transparent_color(&mut self) {
        self.ppaint.blend_mode = tiny_skia::BlendMode::SourceIn;
        self.ppaint
            .set_color(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
    }

    #[inline]
    pub fn set_stroke_cap_round(&mut self) {
        self.stroke.line_cap = tiny_skia::LineCap::Round;
    }

    #[inline]
    pub fn unset_stroke_cap(&mut self) {
        self.stroke.line_cap = tiny_skia::LineCap::Butt;
    }

    #[inline]
    pub fn set_dash(&mut self, interval_on: f32, interval_off: f32) {
        self.stroke.dash = tiny_skia::StrokeDash::new(vec![interval_on, interval_off], 0.0);
    }

    #[inline]
    pub fn unset_dash(&mut self) {
        self.stroke.dash = None;
    }

    #[inline]
    pub fn draw_polyline(&mut self, pts: &[(f32, f32)]) {
        let mut pb = PathBuilder::new();

        pb.move_to(pts[0].0, pts[0].1);
        for pt in pts.iter() {
            pb.line_to(pt.0, pt.1);
        }
        let path = pb.finish().unwrap();

        self.pixmap.stroke_path(
            &path,
            &self.ppaint,
            &self.stroke,
            Transform::identity(),
            None,
        );
    }

    #[inline]
    pub fn draw_closed_polyline(&mut self, pts: &[(f32, f32)]) {
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for pt in pts.iter() {
            pb.line_to(pt.0, pt.1);
        }
        let path = pb.finish().unwrap();

        self.pixmap.stroke_path(
            &path,
            &self.ppaint,
            &self.stroke,
            Transform::identity(),
            None,
        );
    }

    #[inline]
    pub fn draw_filled_polygon(&mut self, apts: &[Vec<(f32, f32)>]) {
        let mut pb = PathBuilder::new();
        for pts in apts {
            pb.move_to(pts[0].0, pts[0].1);
            for pt in pts.iter() {
                pb.line_to(pt.0, pt.1);
            }
        }
        let path = pb.finish().unwrap();

        self.stroke.width = 1.0;

        self.pixmap.fill_path(
            &path,
            &self.ppaint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );

        self.pixmap.stroke_path(
            &path,
            &self.ppaint,
            &self.stroke,
            Transform::identity(),
            None,
        );
    }

    #[inline]
    pub fn save_as(&self, fs: &impl FileSystem, filename: &std::path::Path) -> anyhow::Result<()> {
        let data = self.pixmap.encode_png()?;

        let mut file = fs.create(filename)?;
        file.write_all(&data)?;
        Ok(())
    }

    /// The canvas as the PNG [`Self::save_as`] writes decodes: RGBA, demultiplied.
    pub fn into_rgba(self) -> image::RgbaImage {
        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        image::RgbaImage::from_raw(width, height, self.pixmap.take_demultiplied())
            .expect("a pixmap holds four bytes per pixel")
    }

    #[inline]
    pub fn overlay(&mut self, other_canvas: &mut Canvas, x: f32, y: f32) {
        self.pixmap.draw_pixmap(
            x as i32,
            y as i32,
            other_canvas.pixmap.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::fs::memory::MemoryFileSystem;
    use std::path::Path;

    #[test]
    fn into_rgba_is_the_saved_png_decoded() {
        let mut canvas = Canvas::new(8, 6);
        canvas.set_color(Color::new(29, 190, 255));
        canvas.draw_filled_polygon(&[vec![(1.0, 1.0), (6.0, 1.5), (3.0, 5.0), (1.0, 1.0)]]);
        canvas.set_transparent_color();
        canvas.draw_filled_polygon(&[vec![(0.0, 3.0), (8.0, 3.0), (8.0, 4.0), (0.0, 3.0)]]);

        let fs = MemoryFileSystem::new();
        canvas.save_as(&fs, Path::new("high.png")).unwrap();
        let decoded = fs.read_image_png("high.png").unwrap();
        assert!(matches!(decoded, image::DynamicImage::ImageRgba8(_)));
        assert!(decoded.to_rgba8() == canvas.into_rgba());
    }
}
