use crate::cliffs::CliffSet;
use crate::config::Config;
use crate::formlines::FormLineSelection;
use crate::geometry::Classification;
use crate::geometry::ContourKind;
use crate::geometry::Point2;
use crate::geometry::Polylines;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::knolls::DotKnollSet;
use crate::mapframe::{MapFrame, WorldFile};
use crate::merge::ContourSet;
use crate::vegetation::VegetationFrame;
use image::buffer::ConvertBuffer;
use image::{ImageBuffer, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut};
use log::info;
use std::error::Error;
use std::f64::consts::PI;
use std::path::Path;

/// The ground model's debug intermediate, which a re-render reads for [`MapInputs::ground`]; a
/// tile run writes it only with debug_intermediates=1.
pub const GROUND_DUMP: &str = "xyz2.hmap";

/// The temp folder files a re-render needs: the dumps of the [`MapInputs`] and the
/// vegetation rasters. A tile run leaves the dumps only with debug_intermediates=1. The
/// re-render also draws [`OPTIONAL_RENDER_INPUTS`] when they are there.
const RENDER_INPUTS: [&str; 8] = [
    VEGETATION_PNG,
    VEGETATION_PGW,
    UNDERGROWTH_PNG,
    GROUND_DUMP,
    crate::merge::CONTOURS_DUMP,
    crate::knolls::DOT_KNOLLS_DUMP,
    crate::cliffs::PASSABLE_DUMP,
    crate::cliffs::IMPASSABLE_DUMP,
];

/// The vegetation raster (a product of the raster family), [`VegetationLayers::vegetation`].
pub const VEGETATION_PNG: &str = "vegetation.png";
/// The vegetation raster's world file, [`VegetationLayers::world`].
pub const VEGETATION_PGW: &str = "vegetation.pgw";
/// The undergrowth raster (a product of the raster family),
/// [`VegetationLayers::undergrowth`].
pub const UNDERGROWTH_PNG: &str = "undergrowth.png";
/// The water and buildings debug intermediate, [`VegetationLayers::water_buildings`].
pub const WATER_BUILDINGS_DUMP: &str = "blueblack.png";
/// The blocks debug intermediate, [`MapInputs::blocks`].
pub const BLOCKS_DUMP: &str = "blocks.png";
/// The shape files' layer under the contours, a debug intermediate, [`ShapeLayers::low`].
pub const SHAPES_LOW_DUMP: &str = "low.png";
/// The shape files' layer over the cliffs, a debug intermediate, [`ShapeLayers::high`].
pub const SHAPES_HIGH_DUMP: &str = "high.png";

/// The temp folder files a re-render draws when they are there: a tile run writes them
/// only with debug_intermediates=1, blocks with detectbuildings and the shape layers with
/// shape files.
pub const OPTIONAL_RENDER_INPUTS: [&str; 4] = [
    WATER_BUILDINGS_DUMP,
    BLOCKS_DUMP,
    SHAPES_LOW_DUMP,
    SHAPES_HIGH_DUMP,
];

/// An error when `outputs` leaves out the raster family: the map is a raster product, so
/// rendering it (a re-render, `render`, a shape-file zip) would write what was not asked
/// for.
pub fn check_raster(config: &Config) -> Result<(), Box<dyn Error>> {
    if config.outputs.raster {
        Ok(())
    } else {
        Err("outputs has no raster family: nothing to render".into())
    }
}

/// An error naming the [`RENDER_INPUTS`] missing from `tmpfolder`, if any: re-rendering
/// needs the debug intermediates of a tile run.
pub fn check_inputs(fs: &impl FileSystem, tmpfolder: &Path) -> Result<(), Box<dyn Error>> {
    let missing: Vec<&str> = RENDER_INPUTS
        .into_iter()
        .filter(|name| !fs.exists(tmpfolder.join(name)))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "cannot render from {}: {} missing. Re-rendering reads the tile's debug \
         intermediates: re-run the tile with debug_intermediates=1",
        tmpfolder.display(),
        missing.join(", ")
    )
    .into())
}

/// The vegetation the map is drawn on, at one pixel per metre (`undergrowth` at its own
/// pitch), in the colours the PNGs decode to.
pub struct VegetationLayers {
    /// The open land over the green shades (`vegetation.png`).
    pub vegetation: RgbaImage,
    /// The undergrowth (`undergrowth.png`).
    pub undergrowth: RgbaImage,
    /// The water and buildings (`blueblack.png`), None when a re-render has no dump.
    pub water_buildings: Option<RgbaImage>,
    /// The frame of `vegetation` and `water_buildings` (`vegetation.pgw`).
    pub world: WorldFile,
}

impl VegetationLayers {
    /// The frame of the vegetation raster, which the shape files are drawn in.
    pub fn frame(&self) -> VegetationFrame {
        VegetationFrame {
            x_origin: self.world.x_origin,
            y_origin: self.world.y_origin,
            width: self.vegetation.width(),
            height: self.vegetation.height(),
        }
    }
}

/// The shape files drawn at the map's pixels.
pub struct ShapeLayers {
    /// Drawn under the north lines and contours (`low.png` in the debug intermediates).
    pub low: RgbaImage,
    /// Drawn over the cliffs (`high.png` in the debug intermediates).
    pub high: RgbaImage,
}

/// The values [`render`] draws the map from.
#[derive(Clone, Copy)]
pub struct MapInputs<'a> {
    /// The ground model (`xyz2.hmap` in the debug intermediates).
    pub ground: &'a HeightMap,
    /// smoothjoin's contours (`out2.dxf.bin` in the debug intermediates).
    pub contours: &'a ContourSet,
    /// The dot knolls (`dotknolls.dxf.bin` in the debug intermediates).
    pub dot_knolls: &'a DotKnollSet,
    /// The cliffs (`c2g.dxf.bin` and `c3g.dxf.bin` in the debug intermediates).
    pub cliffs: &'a CliffSet,
    /// The form lines, None without them.
    pub form_lines: Option<&'a FormLineSelection>,
    /// The vegetation rasters.
    pub vegetation: &'a VegetationLayers,
    /// The blocks (`blocks.png` in the debug intermediates), None without
    /// detectbuildings.
    pub blocks: Option<&'a RgbImage>,
    /// The shape files' layers, None without shape files.
    pub shapes: Option<&'a ShapeLayers>,
}

/// A rendered map and its world file.
pub struct MapOutput {
    pub image: RgbaImage,
    pub world: WorldFile,
}

/// The file name stem of the map rendered on `thread`: `pullautus{thread}`, or
/// `pullautus_depr{thread}` with the depressions.
pub fn map_stem(thread: &str, nodepressions: bool) -> String {
    if nodepressions {
        format!("pullautus{thread}")
    } else {
        format!("pullautus_depr{thread}")
    }
}

/// Writes `map` as `{stem}.png`, its world file `{stem}.pgw` and the CRS sidecar.
pub fn write_map(
    fs: &impl FileSystem,
    stem: &str,
    map: &MapOutput,
    epsg: Option<u32>,
) -> Result<(), Box<dyn Error>> {
    map.image.write_to(
        &mut fs.create(format!("{stem}.png"))?,
        image::ImageFormat::Png,
    )?;
    map.world.write(&mut fs.create(format!("{stem}.pgw"))?)?;
    crate::crs::write_raster_crs(fs, format!("{stem}.png"), epsg)?;
    Ok(())
}

/// Parameters of [`render`].
#[derive(Debug, Clone, PartialEq)]
pub struct RenderParams {
    /// The contours' and form lines' parameters, with the sheet the map is drawn on.
    pub curves: CurveRenderParams,
    /// The north lines' angle in degrees (ini `northlinesangle`).
    pub north_lines_angle_deg: f64,
    /// The north lines' width in pixels (ini `northlineswidth`); none at 999.
    pub north_lines_width: usize,
    /// Colour the cliffs by the pass that found them (ini `cliffdebug`).
    pub cliffdebug: bool,
}

/// Draws the map from `inputs` with `params`, the depression contours unless
/// `nodepressions`.
pub fn render(params: &RenderParams, inputs: &MapInputs, nodepressions: bool) -> MapOutput {
    info!("Rendering...");

    let frame = params.curves.frame;
    let nwidth = params.north_lines_width;

    let angle = -params.north_lines_angle_deg / 180.0 * PI;

    // Draw vegetation ----------
    let vege_frame = &inputs.vegetation.world;
    let x0 = vege_frame.x_origin;
    let y0 = vege_frame.y_origin;

    let img = &inputs.vegetation.vegetation;
    let imgug = &inputs.vegetation.undergrowth;

    let w = img.width();
    let h = img.height();

    // the north lines' phase on the sheet; it used to skip the scale (`/ 254 * 600`)
    let eastoff = -frame.to_px(
        (x0 - (-angle).tan() * y0)
            - ((x0 - (-angle).tan() * y0) / (250.0 / angle.cos())).floor() * (250.0 / angle.cos()),
    );

    let new_width = frame.to_px(w as f64) as u32;
    let new_height = frame.to_px(h as f64) as u32;
    let mut img = image::imageops::resize(
        img,
        new_width,
        new_height,
        image::imageops::FilterType::Nearest,
    );

    let imgug = image::imageops::resize(
        imgug,
        new_width,
        new_height,
        image::imageops::FilterType::Nearest,
    );

    image::imageops::overlay(&mut img, &imgug, 0, 0);

    if let Some(shapes) = inputs.shapes {
        let low = image::imageops::resize(
            &shapes.low,
            new_width,
            new_height,
            image::imageops::FilterType::Nearest,
        );
        image::imageops::overlay(&mut img, &low, 0, 0);
    }

    // north lines ----------------
    if angle != 999.0 {
        let mut i: f64 = eastoff - frame.to_px(250.0) / angle.cos() * 100.0;
        while i < frame.to_px(w as f64 * 5.0) {
            for m in 0..nwidth {
                draw_line_segment_mut(
                    &mut img,
                    (i as f32 + m as f32, 0.0),
                    (
                        (i as f32 + frame.to_px(angle.tan() * (h as f64)) as f32) + m as f32,
                        frame.to_px(h as f64) as f32,
                    ),
                    Rgba([0, 0, 200, 255]),
                );
            }
            i += frame.to_px(250.0) / angle.cos();
        }
    }

    draw_curves(
        &params.curves,
        &mut img,
        inputs.ground,
        inputs.contours,
        inputs.form_lines,
        nodepressions,
    );

    // dotknolls----------
    for (point, layer) in inputs.dot_knolls.points.iter() {
        if *layer != Classification::Dotknoll {
            continue;
        }

        // convert point to image coordinates
        let x = frame.to_px(point.x - x0);
        let y = frame.to_px(y0 - point.y);

        let color = Rgba([166, 85, 43, 255]);
        draw_filled_circle_mut(&mut img, (x as i32, y as i32), 7, color)
    }
    // blocks -------------
    if let Some(blocks) = inputs.blocks {
        let mut blockpurple: RgbaImage = blocks.convert();
        for p in blockpurple.pixels_mut() {
            if p[0] == 255 && p[1] == 255 && p[2] == 255 {
                p[3] = 0;
            }
        }
        let blockpurple = image::imageops::crop(&mut blockpurple, 0, 0, w, h).to_image();
        let blockpurple_thumb = image::imageops::resize(
            &blockpurple,
            new_width,
            new_height,
            image::imageops::FilterType::Nearest,
        );

        for i in 0..3 {
            for j in 0..3 {
                image::imageops::overlay(
                    &mut img,
                    &blockpurple_thumb,
                    (i as i64 - 1) * 2,
                    (j as i64 - 1) * 2,
                );
            }
        }
        image::imageops::overlay(&mut img, &blockpurple_thumb, 0, 0);
    }
    // blueblack -------------
    if let Some(imgbb) = &inputs.vegetation.water_buildings {
        let mut imgbb = imgbb.clone();
        for p in imgbb.pixels_mut() {
            if p[0] == 255 && p[1] == 255 && p[2] == 255 {
                p[3] = 0;
            }
        }
        let imgbb = image::imageops::crop(&mut imgbb, 0, 0, w, h).to_image();
        let imgbb_thumb = image::imageops::resize(
            &imgbb,
            new_width,
            new_height,
            image::imageops::FilterType::Nearest,
        );
        image::imageops::overlay(&mut img, &imgbb_thumb, 0, 0);
    }

    // the passable cliffs, then the impassable ones over them
    draw_cliffs(params, &inputs.cliffs.passable, &mut img, x0, y0);
    draw_cliffs(params, &inputs.cliffs.impassable, &mut img, x0, y0);

    // high -------------
    if let Some(shapes) = inputs.shapes {
        let high_thumb = image::imageops::resize(
            &shapes.high,
            new_width,
            new_height,
            image::imageops::FilterType::Nearest,
        );
        image::imageops::overlay(&mut img, &high_thumb, 0, 0);
    }

    info!("Done");
    MapOutput {
        image: img,
        world: map_world_file(vege_frame, &frame),
    }
}

/// Draws the cliff dashes `lines` on `img`, the sheet whose top left corner is at
/// (`x0`, `y0`) in world coordinates.
fn draw_cliffs(
    params: &RenderParams,
    lines: &Polylines<Point2, Classification>,
    img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x0: f64,
    y0: f64,
) {
    let frame = params.curves.frame;

    // one buffer for every dash's points in pixel space
    let mut line = Vec::new();
    for (dash, &class) in lines.iter() {
        // based on the layer we select the cliffcolor
        let cliffcolor = if params.cliffdebug {
            match class {
                Classification::Cliff2 => Rgba([100, 0, 100, 255]),
                Classification::Cliff3 => Rgba([0, 100, 100, 255]),
                Classification::Cliff4 => Rgba([100, 100, 0, 255]),
                _ => Rgba([0, 0, 0, 255]), // black
            }
        } else {
            Rgba([0, 0, 0, 255]) // black
        };

        // scale and flip all points into pixel-space
        line.clear();
        line.extend(
            dash.iter()
                .map(|p| Point2::new(frame.to_px(p.x - x0), frame.to_px(y0 - p.y))),
        );

        if line.first() != line.last() {
            // trick to borrow both first and last as mutable at the same time. If not possible (eg
            // len == 0, then we should skip this line anyways)
            let [first, .., last] = &mut line[..] else {
                continue;
            };

            let dx = first.x - last.x;
            let dy = first.y - last.y;
            let dist = (dx.powi(2) + dy.powi(2)).sqrt();
            if dist > 0.0 {
                first.x += dx / dist * 1.5;
                first.y += dy / dist * 1.5;
                last.x -= dx / dist * 1.5;
                last.y -= dy / dist * 1.5;
                draw_filled_circle_mut(img, (first.x as i32, first.y as i32), 3, cliffcolor);
                draw_filled_circle_mut(img, (last.x as i32, last.y as i32), 3, cliffcolor);
            }
        }
        for i in 1..line.len() {
            for n in 0..6 {
                for m in 0..6 {
                    draw_line_segment_mut(
                        img,
                        (
                            (line[i - 1].x + (n as f64) - 3.0).floor() as f32,
                            (line[i - 1].y + (m as f64) - 3.0).floor() as f32,
                        ),
                        (
                            (line[i].x + (n as f64) - 3.0).floor() as f32,
                            (line[i].y + (m as f64) - 3.0).floor() as f32,
                        ),
                        cliffcolor,
                    )
                }
            }
        }
    }
}

/// Is a closed form line ring smaller than ISOM allows the symbol to be drawn?
///
/// Parameters of [`draw_curves`], which draws the contours and dashes the form lines.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveRenderParams {
    /// The sheet the lines are drawn on (ini `mapscale`).
    pub frame: MapFrame,
    /// Form line dash length in pixels (ini `dashlength`).
    pub dashlength: f64,
    /// Form line gap length in pixels (ini `gaplength`).
    pub gaplength: f64,
    /// Colour of depression contours (ini `depressions_color`).
    pub depressions_color: (u8, u8, u8),
}

/// Draw `contours` onto `canvas`, the half-interval lines as the dashed form lines
/// `form_lines` selected (none without form lines). The `ground` model places the map;
/// with `nodepressions` the depressions are left out.
pub fn draw_curves(
    params: &CurveRenderParams,
    canvas: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    ground: &HeightMap,
    contours: &ContourSet,
    form_lines: Option<&FormLineSelection>,
    nodepressions: bool,
) {
    // Drawing curves --------------
    let &CurveRenderParams {
        frame,
        dashlength,
        gaplength,
        depressions_color,
    } = params;
    let selective = form_lines.is_some();

    let x0 = ground.minx();
    let y0 = ground.maxy();
    let input_lines = &contours.lines;

    let mut last_curve_drawn = false;
    let mut should_draw_next_slope_line = true;
    let next_slopeline_starts = input_lines
        .iter()
        .map(|(line, (layer, _))| {
            if *layer == Classification::SlopeLine {
                line.first()
                    .map(|point| (frame.to_px(point.x - x0), frame.to_px(y0 - point.y)))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for (ii, (line, &(layer, _height))) in input_lines.iter().enumerate() {
        let mut line = line.clone();
        if layer == Classification::SlopeLine && (!last_curve_drawn || !should_draw_next_slope_line)
        {
            should_draw_next_slope_line = true;
            continue;
        }
        last_curve_drawn = false;

        // flip and scale the line points
        for p in line.iter_mut() {
            p.x = frame.to_px(p.x - x0);
            p.y = frame.to_px(y0 - p.y);
        }

        // TEMP: split x and y values
        let x = line.iter().map(|p| p.x).collect::<Vec<_>>();
        let y = line.iter().map(|p| p.y).collect::<Vec<_>>();

        // The slope line is part of symbol 101, so it carries depression contour color
        // weight — it just belongs to a depression, hence the nodepressions gate below.
        let kind = layer.contour_kind();
        // a contour, index contour or half-interval line that is not a depression
        let contour_line = kind.is_some_and(|k| !k.depression());
        let color = if contour_line {
            Rgba([166, 85, 43, 255]) // brown
        } else {
            Rgba([
                depressions_color.0,
                depressions_color.1,
                depressions_color.2,
                255,
            ]) // Default purple
        };

        if !nodepressions || contour_line {
            let index = kind.is_some_and(ContourKind::index);
            let mut curvew = 2.0;
            if index {
                curvew = 3.0;
            }
            if selective {
                if kind.is_some_and(ContourKind::half_interval) {
                    curvew = 1.5
                }
                if index {
                    curvew = 3.5
                }
            }

            // what the form-line selection keeps of a half-interval line
            let keep = (curvew == 1.5).then(|| {
                form_lines
                    .and_then(|f| f.keep[ii].as_ref())
                    .expect("the form-line selection covers every half-interval line")
            });
            let kept = keep.map_or(&[][..], |k| k.vertices.as_slice());
            let drawn = |i: usize| keep.is_none_or(|k| k.keeps(i));

            let mut linedist = 0.0;
            let mut onegapdone = false;
            let mut gap = 0.0;

            if layer == Classification::SmallDepression {
                curvew = 3.0;
            }

            // Check if next symbol is a slopeline and get its location
            let next_slopeline_start = if layer.is_depression() {
                next_slopeline_starts.get(ii + 1).copied().flatten()
            } else {
                None
            };

            for i in 1..x.len() {
                if !drawn(i)
                    && should_draw_next_slope_line
                    && let Some((next_x, next_y)) = next_slopeline_start
                    && x[i] == next_x
                    && y[i] == next_y
                {
                    should_draw_next_slope_line = false;
                }
                if drawn(i) {
                    if curvew == 1.5 {
                        let step = ((x[i - 1] - x[i]).powi(2) + (y[i - 1] - y[i]).powi(2)).sqrt();
                        if i < 4 {
                            linedist = 0.0
                        }
                        linedist += step;
                        if linedist > dashlength && i > 10 && i < x.len() - 11 {
                            let mut sum = 0.0;
                            for k in (i - 4)..(i + 6) {
                                sum +=
                                    ((x[k - 1] - x[k]).powi(2) + (y[k - 1] - y[k]).powi(2)).sqrt()
                            }
                            let mut toonearend = false;
                            for k in (i - 10)..(i + 10) {
                                if !kept[k] {
                                    toonearend = true;
                                    break;
                                }
                            }
                            if !toonearend
                                && ((x[i - 5] - x[i + 5]).powi(2) + (y[i - 5] - y[i + 5]).powi(2))
                                    .sqrt()
                                    * 1.138
                                    > sum
                            {
                                linedist = 0.0;
                                gap = gaplength;
                                onegapdone = true;
                            }
                        }
                        if !onegapdone && (i < x.len() - 9) && i > 6 {
                            gap = gaplength * 0.82;
                            onegapdone = true;
                            linedist = 0.0
                        }
                        if gap > 0.0 {
                            gap -= step;
                            if gap < 0.0 && onegapdone && step > 0.0 {
                                let mut n = -curvew - 0.5;
                                while n < curvew + 0.5 {
                                    let mut m = -curvew - 0.5;
                                    while m < curvew + 0.5 {
                                        draw_line_segment_mut(
                                            canvas,
                                            (
                                                ((-x[i - 1] * gap + (step + gap) * x[i]) / step + n)
                                                    as f32,
                                                ((-y[i - 1] * gap + (step + gap) * y[i]) / step + m)
                                                    as f32,
                                            ),
                                            ((x[i] + n) as f32, (y[i] + m) as f32),
                                            color,
                                        );

                                        m += 1.0;
                                    }
                                    n += 1.0;
                                    last_curve_drawn = true;
                                }
                                gap = 0.0;
                            }
                        } else {
                            let mut n = -curvew - 0.5;
                            while n < curvew + 0.5 {
                                let mut m = -curvew - 0.5;
                                while m < curvew + 0.5 {
                                    draw_line_segment_mut(
                                        canvas,
                                        ((x[i - 1] + n) as f32, (y[i - 1] + m) as f32),
                                        ((x[i] + n) as f32, (y[i] + m) as f32),
                                        color,
                                    );
                                    m += 1.0;
                                    last_curve_drawn = true;
                                }
                                n += 1.0;
                            }
                        }
                    } else {
                        let mut n = -curvew;
                        while n < curvew {
                            let mut m = -curvew;
                            while m < curvew {
                                draw_line_segment_mut(
                                    canvas,
                                    ((x[i - 1] + n) as f32, (y[i - 1] + m) as f32),
                                    ((x[i] + n) as f32, (y[i] + m) as f32),
                                    color,
                                );
                                m += 1.0;
                                last_curve_drawn = true;
                            }
                            n += 1.0;
                        }
                    }
                }
            }
        }
    }
}

/// The rendered map's world file: `vegetation.pgw`'s frame (one pixel per ground metre) with
/// the map's pixel size.
fn map_world_file(vege_frame: &WorldFile, frame: &MapFrame) -> WorldFile {
    WorldFile {
        pixel_size_x: frame.to_metres(vege_frame.pixel_size_x),
        pixel_size_y: frame.to_metres(vege_frame.pixel_size_y),
        ..vege_frame.clone()
    }
}

#[cfg(test)]
mod tests {
    use crate::mapframe::MapFrame;

    #[test]
    fn rendering_a_pruned_folder_asks_for_the_debug_intermediates() {
        use crate::io::fs::FileSystem;
        use std::path::Path;
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let temp = Path::new("temp");
        fs.create_dir_all(temp).unwrap();
        // what a tile run without debug_intermediates leaves
        for name in [
            "vegetation.png",
            "vegetation.pgw",
            "undergrowth.png",
            "out2.dxf",
        ] {
            fs.create(temp.join(name)).unwrap();
        }
        let err = super::check_inputs(&fs, temp).unwrap_err().to_string();
        assert!(err.contains("debug_intermediates=1"), "{err}");
        assert!(err.contains("xyz2.hmap, out2.dxf.bin"), "{err}");
        assert!(!err.contains("vegetation.png"), "{err}");

        for name in super::RENDER_INPUTS {
            fs.create(temp.join(name)).unwrap();
        }
        assert!(super::check_inputs(&fs, temp).is_ok());
    }

    /// A map drawn from values only: the vegetation's frame at the sheet's pixels, a dot
    /// knoll and a building where the inputs put them.
    #[test]
    fn render_draws_the_inputs_in_the_vegetation_frame() {
        use super::*;
        use crate::geometry::{Bounds, Points};
        use crate::vec2d::Vec2D;
        let config = Config::from_file(Path::new("pullauta.default.ini")).unwrap();
        let frame = config.map_frame;
        let (x0, y0) = (1000.0, 2010.0);
        let bounds = Bounds::new(x0, x0 + 12.0, y0 - 10.0, y0);
        let ground = HeightMap {
            xoffset: x0,
            yoffset: y0 - 10.0,
            scale: 2.0,
            grid: Vec2D::new(6, 5, 100.0),
        };
        let contours = ContourSet {
            lines: Polylines::new(),
            bounds: bounds.clone(),
        };
        let mut points = Points::new();
        points.push(Point2::new(x0 + 3.0, y0 - 3.0), Classification::Dotknoll);
        let dot_knolls = DotKnollSet {
            points,
            bounds: bounds.clone(),
        };
        let cliffs = CliffSet {
            passable: Polylines::new(),
            impassable: Polylines::new(),
            bounds,
        };
        let white = Rgba([255, 255, 255, 255]);
        let mut water_buildings = RgbaImage::from_pixel(12, 10, white);
        water_buildings.put_pixel(9, 8, Rgba([0, 0, 0, 255]));
        let vegetation = VegetationLayers {
            vegetation: RgbaImage::from_pixel(12, 10, white),
            undergrowth: RgbaImage::from_pixel(28, 24, Rgba([255, 255, 255, 0])),
            water_buildings: Some(water_buildings),
            world: WorldFile::north_up(1.0, x0, y0),
        };
        let inputs = MapInputs {
            ground: &ground,
            contours: &contours,
            dot_knolls: &dot_knolls,
            cliffs: &cliffs,
            form_lines: None,
            vegetation: &vegetation,
            blocks: None,
            shapes: None,
        };

        let map = render(&config.render, &inputs, true);
        assert_eq!(
            map.image.dimensions(),
            (frame.to_px(12.0) as u32, frame.to_px(10.0) as u32)
        );
        assert_eq!(map.world, map_world_file(&vegetation.world, &frame));
        let px = |x: f64, y: f64| {
            *map.image
                .get_pixel(frame.to_px(x) as u32, frame.to_px(y) as u32)
        };
        assert_eq!(px(3.0, 3.0), Rgba([166, 85, 43, 255]));
        assert_eq!(px(9.5, 8.5), Rgba([0, 0, 0, 255]));
        assert_eq!(px(6.0, 1.0), white);
    }

    #[test]
    fn map_world_file_scales_pixel_sizes_and_keeps_the_origin() {
        use super::map_world_file;
        use crate::mapframe::WorldFile;
        let vege = WorldFile::north_up(1.0, 123456.5, 7891011.5);
        let map = map_world_file(&vege, &MapFrame::default());
        assert_eq!(map.pixel_size_x, 1.0 / 600.0 * 254.0);
        assert_eq!(map.pixel_size_y, -1.0 / 600.0 * 254.0);
        assert_eq!((map.rotation_x, map.rotation_y), (0.0, 0.0));
        assert_eq!((map.x_origin, map.y_origin), (123456.5, 7891011.5));
        assert_eq!(
            map_world_file(&vege, &MapFrame::at_scale(20_000.0)).pixel_size_x,
            2.0 * map.pixel_size_x
        );
    }
}
