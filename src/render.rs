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
use image::ImageBuffer;
use image::Rgba;
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut};
use log::info;
use std::error::Error;
use std::f64::consts::PI;
use std::path::Path;

/// The ground model's debug intermediate, which a re-render reads for [`MapInputs::ground`]; a
/// tile run writes it only with debug_intermediates=1.
pub const GROUND_DUMP: &str = "xyz2.hmap";

/// The temp folder files a re-render reads: the dumps of the [`MapInputs`] and the files
/// [`render`] opens itself. A tile run leaves them only with debug_intermediates=1.
const RENDER_INPUTS: [&str; 8] = [
    "vegetation.png",
    "vegetation.pgw",
    "undergrowth.png",
    GROUND_DUMP,
    crate::merge::CONTOURS_DUMP,
    crate::knolls::DOT_KNOLLS_DUMP,
    crate::cliffs::PASSABLE_DUMP,
    crate::cliffs::IMPASSABLE_DUMP,
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

/// The values [`render`] draws the map from, next to the files it reads from the temp
/// folder. Later stages move their results here as they stop going through files.
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
}

/// Draws the map from `inputs` and the stages' files in `tmpfolder`; a re-render first
/// checks them with [`check_inputs`].
#[allow(clippy::too_many_arguments)]
pub fn render(
    fs: &impl FileSystem,
    config: &Config,
    thread: &String,
    tmpfolder: &Path,
    inputs: &MapInputs,
    angle_deg: f64,
    nwidth: usize,
    nodepressions: bool,
) -> Result<(), Box<dyn Error>> {
    info!("Rendering...");
    check_raster(config)?;

    let frame = config.map_frame;

    let angle = -angle_deg / 180.0 * PI;

    // Draw vegetation ----------
    let tfw_in = tmpfolder.join("vegetation.pgw");
    let vege_frame = WorldFile::read(fs, tfw_in).expect("PGW file does not exist");
    let x0 = vege_frame.x_origin;
    let y0 = vege_frame.y_origin;

    let mut img_reader = image::ImageReader::new(
        fs.open(tmpfolder.join("vegetation.png"))
            .expect("Opening vegetation image failed"),
    );
    img_reader.set_format(image::ImageFormat::Png);
    img_reader.no_limits();
    let img = img_reader.decode().unwrap();

    let mut imgug_reader = image::ImageReader::new(
        fs.open(tmpfolder.join("undergrowth.png"))
            .expect("Opening undergrowth image failed"),
    );
    imgug_reader.set_format(image::ImageFormat::Png);
    imgug_reader.no_limits();
    let imgug = imgug_reader.decode().unwrap();

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
        &img,
        new_width,
        new_height,
        image::imageops::FilterType::Nearest,
    );

    let imgug = image::imageops::resize(
        &imgug,
        new_width,
        new_height,
        image::imageops::FilterType::Nearest,
    );

    image::imageops::overlay(&mut img, &imgug, 0, 0);

    let low_file = tmpfolder.join("low.png");
    if fs.exists(&low_file) {
        let mut low_reader =
            image::ImageReader::new(fs.open(low_file).expect("Opening low image failed"));
        low_reader.set_format(image::ImageFormat::Png);
        low_reader.no_limits();
        let low = low_reader.decode().unwrap();
        let low = image::imageops::resize(
            &low,
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
        &config.curves,
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
    let blocks_file = tmpfolder.join("blocks.png");
    if fs.exists(&blocks_file) {
        let mut blockpurple_reader =
            image::ImageReader::new(fs.open(blocks_file).expect("Opening blocks image failed"));
        blockpurple_reader.set_format(image::ImageFormat::Png);
        blockpurple_reader.no_limits();
        let blockpurple = blockpurple_reader.decode().unwrap();
        let mut blockpurple = blockpurple.to_rgba8();
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
    let blueblack_file = tmpfolder.join("blueblack.png");
    if fs.exists(&blueblack_file) {
        let mut imgbb_reader = image::ImageReader::new(
            fs.open(blueblack_file)
                .expect("Opening blueblack image failed"),
        );
        imgbb_reader.set_format(image::ImageFormat::Png);
        imgbb_reader.no_limits();
        let imgbb = imgbb_reader.decode().unwrap();
        let mut imgbb = imgbb.to_rgba8();
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
    draw_cliffs(config, &inputs.cliffs.passable, &mut img, x0, y0);
    draw_cliffs(config, &inputs.cliffs.impassable, &mut img, x0, y0);

    // high -------------
    let high_file = tmpfolder.join("high.png");
    if fs.exists(&high_file) {
        let mut high_reader =
            image::ImageReader::new(fs.open(high_file).expect("Opening high image failed"));
        high_reader.set_format(image::ImageFormat::Png);
        high_reader.no_limits();
        let high = high_reader.decode().unwrap();
        let high_thumb = image::imageops::resize(
            &high,
            new_width,
            new_height,
            image::imageops::FilterType::Nearest,
        );
        image::imageops::overlay(&mut img, &high_thumb, 0, 0);
    }

    let filename = if nodepressions {
        format!("pullautus{thread}")
    } else {
        format!("pullautus_depr{thread}")
    };

    img.write_to(
        &mut fs
            .create(format!("{filename}.png"))
            .expect("could not save output png"),
        image::ImageFormat::Png,
    )
    .expect("could not write image");

    let mut pgw_file_out = fs
        .create(format!("{filename}.pgw"))
        .expect("Unable to create file");
    map_world_file(&vege_frame, &frame)
        .write(&mut pgw_file_out)
        .expect("Unable to write to file");
    crate::crs::write_raster_crs(fs, format!("{filename}.png"), config.epsg)?;
    info!("Done");
    Ok(())
}

/// Draws the cliff dashes `lines` on `img`, the sheet whose top left corner is at
/// (`x0`, `y0`) in world coordinates.
fn draw_cliffs(
    config: &Config,
    lines: &Polylines<Point2, Classification>,
    img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x0: f64,
    y0: f64,
) {
    let frame = config.map_frame;

    // one buffer for every dash's points in pixel space
    let mut line = Vec::new();
    for (dash, &class) in lines.iter() {
        // based on the layer we select the cliffcolor
        let cliffcolor = if config.cliffdebug {
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
