use image::{RgbImage, Rgba, RgbaImage};
use log::info;
use std::error::Error;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::contours::{contours_from_lines, join_contours};
use crate::geometry::{
    BinaryDxf, Classification, ContourLevels, Geometry, Point2, Point3, Points, Polylines, Ring,
};
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::mapframe::{IsomMinima, WorldFile};
use crate::vec2d::Vec2D;
use image::buffer::ConvertBuffer;

/// The merged `.dxf.bin` of every tile's terrain, written to the working directory by
/// [`bindxfmerge`] with debug_intermediates=1.
const MERGED_DXF_BIN: &str = "merged.dxf.bin";

/// Whether a file in the batch output folder is a merge output (`merged.png`,
/// `merged_vege.png`, ...), which the png merges write there and must not read back.
fn is_merge_output(filename: &str) -> bool {
    filename.starts_with(crate::geojson::MERGED_PREFIX)
}

fn merge_png(
    fs: &impl FileSystem,
    config: &Config,
    png_files: Vec<PathBuf>,
    outfilename: &str,
    scale: f64,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut xmin = f64::MAX;
    let mut ymin = f64::MAX;
    let mut xmax = f64::MIN;
    let mut ymax = f64::MIN;
    let mut min_res = f64::MAX;
    for png in png_files.iter() {
        let filename = png.as_path().file_name().unwrap().to_str().unwrap();
        let full_filename = format!("{batchoutfolder}/{filename}");
        let img = fs
            .read_image_png(&full_filename)
            .expect("Opening image failed");

        let width = img.width() as f64;
        let height = img.height() as f64;
        let pgw = full_filename.replace(".png", ".pgw");
        let input = Path::new(&pgw);
        if fs.exists(input) {
            let w = WorldFile::read(fs, input).expect("Can not read input file");
            let res = w.pixel_size_x;
            let tfw4 = w.x_origin;
            let tfw5 = w.y_origin;

            if res < min_res {
                min_res = res
            }
            if tfw4 < xmin {
                xmin = tfw4;
            }
            if (tfw4 + width * res) > xmax {
                xmax = tfw4 + width * res;
            }
            if tfw5 > ymax {
                ymax = tfw5;
            }
            if (tfw5 - height * res) < ymin {
                ymin = tfw5 - height * res;
            }
        }
    }
    let mut im = RgbaImage::from_pixel(
        ((xmax - xmin) / min_res / scale) as u32,
        ((ymax - ymin) / min_res / scale) as u32,
        Rgba([255, 255, 255, 0]),
    );
    for png in png_files.iter() {
        let filename = png.as_path().file_name().unwrap().to_str().unwrap();
        let png = format!("{batchoutfolder}/{filename}");
        let pgw = png.replace(".png", ".pgw");
        let png = Path::new(&png);
        let pgw = Path::new(&pgw);
        let filesize = fs.file_size(png).unwrap();
        if fs.exists(png) && fs.exists(pgw) && filesize > 0 {
            let img = fs.read_image_png(png).expect("Opening image failed");
            let width = img.width() as f64;
            let height = img.height() as f64;

            let w = WorldFile::read(fs, pgw).expect("Can not read input file");
            let res = w.pixel_size_x;
            let tfw4 = w.x_origin;
            let tfw5 = w.y_origin;

            let img2 = image::imageops::thumbnail(
                &img,
                (res / min_res / scale * width + 0.5) as u32,
                (res / min_res / scale * height + 0.5) as u32,
            );

            image::imageops::overlay(
                &mut im,
                &img2,
                ((tfw4 - xmin) / min_res / scale) as i64,
                ((ymax - tfw5) / min_res / scale) as i64,
            );
        }
    }

    // the merged image belongs next to the tiles it merges, not in the working directory
    let outfilename = format!("{batchoutfolder}/{outfilename}");

    let im_rgb8: RgbImage = im.convert();
    im_rgb8
        .write_to(
            &mut fs
                .create(format!("{outfilename}.jpg"))
                .expect("could not save output jpg"),
            image::ImageFormat::Jpeg,
        )
        .expect("could not save output jpg");

    im.write_to(
        &mut fs
            .create(format!("{outfilename}.png"))
            .expect("could not save output png"),
        image::ImageFormat::Png,
    )
    .expect("could not save output Png");

    let mut tfw_file = fs
        .create(format!("{outfilename}.pgw"))
        .expect("Unable to create file");
    WorldFile {
        pixel_size_x: min_res * scale,
        rotation_y: 0.0,
        rotation_x: 0.0,
        pixel_size_y: -min_res * scale,
        x_origin: xmin,
        y_origin: ymax,
    }
    .write(&mut tfw_file)
    .expect("Could not write to file");
    drop(tfw_file);
    fs.copy(
        Path::new(&format!("{outfilename}.pgw")),
        Path::new(&format!("{outfilename}.jgw")),
    )
    .expect("Could not copy file");
    for extension in ["png", "jpg"] {
        crate::crs::write_raster_crs(fs, format!("{outfilename}.{extension}"), config.epsg)?;
    }
    Ok(())
}

pub fn pngmergevege(
    fs: &impl FileSystem,
    config: &Config,
    scale: f64,
    include_undergrowth: bool,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut png_files: Vec<PathBuf> = Vec::new();
    for path in fs.list(batchoutfolder).unwrap() {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if is_merge_output(filename) {
            continue;
        }
        if filename.ends_with("_vege.png")
            || (include_undergrowth && filename.ends_with("_undergrowth.png"))
        {
            png_files.push(path);
        }
    }
    if png_files.is_empty() {
        info!("No _vege.png files found in output directory");
        return Ok(());
    }

    let output_name = if include_undergrowth {
        "merged_vege_undergrowth"
    } else {
        "merged_vege"
    };

    merge_png(fs, config, png_files, output_name, scale).unwrap();
    Ok(())
}

pub fn pngmerge(
    fs: &impl FileSystem,
    config: &Config,
    scale: f64,
    depr: bool,
) -> Result<(), Box<dyn Error>> {
    let batchoutfolder = &config.batchoutfolder;

    let mut png_files: Vec<PathBuf> = Vec::new();
    for path in fs.list(batchoutfolder).unwrap() {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if is_merge_output(filename) {
            continue;
        }
        if filename.ends_with(".png")
            && !filename.ends_with("_undergrowth.png")
            && !filename.ends_with("_undergrowth_bit.png")
            && !filename.ends_with("_vege.png")
            && !filename.ends_with("_vege_bit.png")
            && ((depr && filename.ends_with("_depr.png"))
                || (!depr && !filename.ends_with("_depr.png")))
        {
            png_files.push(path);
        }
    }

    if png_files.is_empty() {
        info!("No files to merge found in output directory");
        return Ok(());
    }
    let mut outfilename = "merged";
    if depr {
        outfilename = "merged_depr";
    }
    merge_png(fs, config, png_files, outfilename, scale).unwrap();
    Ok(())
}

/// Merge the tiles' `.dxf.bin` crops in the batch output folder, per stage and all
/// together, into `merged_<stage>.dxf` and `merged.dxf` in the working directory with
/// the dxf family in `outputs`; the `.dxf.bin` merges are intermediates, written with
/// debug_intermediates=1.
pub fn bindxfmerge(fs: &impl FileSystem, config: &Config) -> anyhow::Result<()> {
    let batchoutfolder = &config.batchoutfolder;

    // These are the different file suffixes we expect:
    let suffixes_to_merge = [
        "contours",
        // "c2f", // No such files exist anymore
        // "c2",  // No such files exist anymore
        "c2g",
        "basemap",
        "c3g",
        "formlines",
        "dotknolls",
        "detected",
    ];

    // a list of files for each suffix
    let mut dxf_files: Vec<Vec<PathBuf>> = vec![Vec::new(); suffixes_to_merge.len()];

    for path in fs.list(batchoutfolder).unwrap() {
        if let Some(filename) = path.file_name() {
            let filename = filename.to_str().unwrap();

            // check if this file matches any of the suffiexes we expect
            for (i, suffix) in suffixes_to_merge.iter().enumerate() {
                if filename.ends_with(&format!("_{suffix}.dxf.bin")) {
                    dxf_files[i].push(path.clone());
                }
            }
        }
    }

    if dxf_files.iter().all(|f| f.is_empty()) {
        info!("No dxf files found in output directory");
        return Ok(());
    }

    // For now (originally) we always use the bounds of the first file loaded for all the generated
    // files. TODO: use the actual new bounds from the loaded files instead.
    let mut first_file_bounds = None;

    let mut all_geometries = Vec::<Geometry>::new();
    for (suffix, files) in suffixes_to_merge.iter().zip(dxf_files) {
        if files.is_empty() {
            info!("No files found for suffix: {suffix}, skipping...");
            continue;
        }

        info!("Merging {} files for suffix: {suffix}", files.len());

        let output_file = PathBuf::from(format!("merged_{suffix}.dxf.bin"));

        let mut geometries: Vec<Geometry> = Vec::with_capacity(files.len());

        for file in files {
            let loaded = BinaryDxf::from_reader(&mut fs.open(&file)?)?;

            // we always use the bounds of the first loaded file
            if first_file_bounds.is_none() {
                first_file_bounds = Some(loaded.bounds().clone());
            }

            let geometry = loaded.take_geometry();

            geometries.extend(geometry.iter().cloned());

            // for the contours, we filter out the half-interval lines for the all_geometries
            if *suffix == "contours" {
                for geo in geometry {
                    let filtered_geo: Geometry = match geo {
                        Geometry::Points(points) => {
                            let mut filtered_points = Points::with_capacity(points.len());

                            for (p, c) in points.into_iter() {
                                if !c.is_half_interval_line() {
                                    filtered_points.push(p, c);
                                }
                            }

                            filtered_points.into()
                        }
                        Geometry::Polylines2(polylines) => {
                            let mut filtered_lines = Polylines::with_capacity(polylines.len());
                            for (l, c) in polylines.into_iter() {
                                if !c.is_half_interval_line() {
                                    filtered_lines.push(l, c);
                                }
                            }
                            filtered_lines.into()
                        }
                        Geometry::Polylines3(polylines) => {
                            let mut filtered_lines = Polylines::with_capacity(polylines.len());
                            for (l, c) in polylines.into_iter() {
                                if !c.0.is_half_interval_line() {
                                    filtered_lines.push(l, c);
                                }
                            }
                            filtered_lines.into()
                        }
                    };

                    all_geometries.push(filtered_geo);
                }
            } else {
                all_geometries.extend(geometry);
            }
        }

        // write output file
        let output = BinaryDxf::new(
            first_file_bounds
                .clone()
                .expect("this should be set since we load at least one file"),
            geometries,
        );
        if config.debug_intermediates {
            output.to_writer(&mut fs.create(&output_file)?)?;
        }
        if config.outputs.dxf {
            let output_file = PathBuf::from(format!("merged_{suffix}.dxf"));
            output.to_dxf(&mut fs.create(&output_file)?)?;
        }
    }

    // output all geometries to a single file
    if let Some(all_bounds) = first_file_bounds {
        let out_merged = BinaryDxf::new(all_bounds, all_geometries);
        if config.debug_intermediates {
            out_merged.to_writer(&mut fs.create(MERGED_DXF_BIN)?)?;
        }
        if config.outputs.dxf {
            out_merged.to_dxf(&mut fs.create("merged.dxf")?)?;
        }
    }

    Ok(())
}

/// The slope-line tick for a depression ring, or `None` when the ring is too small to
/// carry the symbol.
///
/// ISOM 2017-2 (symbol 101) requires at least one slope line on a depression, drawn
/// perpendicular to the contour and pointing downslope — i.e. into the ring. Its length
/// is 0.4 mm on the 1:15,000 original, `minima.slope_line` on the ground (6 m at 1:15 000
/// and 1:10 000); coordinates here are ground metres. A depression below ISOM's minimum
/// size (1.1 x 0.7 mm, `minima.ring_length` x `minima.ring_width`, 16.5 x 10.5 m) is not
/// drawable as a contour depression at all — those are the small-depression symbol's
/// job — so it gets no tick.
///
/// Direction is decided by testing whether the tick's far end lands INSIDE the ring, not
/// by aiming at the ring's centroid: a depression ring is often a long crescent, and a
/// crescent's centroid lies outside it, so the centroid test pointed the tick out of the
/// depression — the "wrong side" seen on Nyrup Hegn.
///
/// Placement then picks, among candidates spread around the ring, the one whose tick end
/// sits deepest inside — furthest from any part of the ring. That keeps the tick clear of
/// the contour instead of crossing it where the depression pinches, which is where a
/// fixed position lands on an elongated ring.
fn decorate_depression(
    el_x: &[f64],
    el_y: &[f64],
    h: f64,
    minima: &IsomMinima,
) -> Option<(Vec<Point3>, Classification)> {
    let length_m = minima.slope_line;
    /// Positions tried around the ring; the best-clearance one wins.
    const CANDIDATES: usize = 12;

    let n = el_x.len();
    if n < 2 {
        return None;
    }
    let (mut xmin, mut xmax) = (f64::MAX, f64::MIN);
    let (mut ymin, mut ymax) = (f64::MAX, f64::MIN);
    for (&x, &y) in el_x.iter().zip(el_y.iter()) {
        xmin = xmin.min(x);
        xmax = xmax.max(x);
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    let (w, hgt) = (xmax - xmin, ymax - ymin);
    if w.max(hgt) < minima.ring_length || w.min(hgt) < minima.ring_width {
        // draw a small depression
        let center = ((xmax + xmin) / 2.0, (ymax + ymin + length_m) / 2.0);
        let steps = 8;
        let mut points = Vec::with_capacity(steps);
        let radius = length_m;
        for i in 0..steps {
            // Angle from 0 to PI (semi-circle)
            let angle = std::f64::consts::PI * ((i as f64) / ((steps - 1) as f64) + 1.0);
            let x = center.0 + radius * angle.cos();
            let y = center.1 + radius * angle.sin();
            points.push(Point3::new(x, y, h));
        }
        return Some((points, Classification::SmallDepression));
    }

    let ring = Ring::from_xy(el_x, el_y);
    let mut best: Option<(f64, [f64; 4])> = None;
    for k in 0..CANDIDATES {
        let i = k * n / CANDIDATES;
        let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
        let (tx, ty) = (el_x[next] - el_x[prev], el_y[next] - el_y[prev]);
        let len = (tx * tx + ty * ty).sqrt();
        if len == 0.0 {
            continue;
        }
        // Both perpendiculars; keep whichever ends up inside the ring.
        for (nx, ny) in [(-ty / len, tx / len), (ty / len, -tx / len)] {
            let (ex, ey) = (el_x[i] + nx * length_m, el_y[i] + ny * length_m);
            if !ring.contains(Point2::new(ex, ey)) {
                continue;
            }
            let clearance = ring.distance_to_point(Point2::new(ex, ey));
            if best.is_none_or(|(b, _)| clearance > b) {
                best = Some((clearance, [el_x[i], el_y[i], ex, ey]));
            }
        }
    }
    let (_, [sx, sy, ex, ey]) = best?;
    Some((
        vec![Point3::new(sx, sy, h), Point3::new(ex, ey, h)],
        Classification::SlopeLine,
    ))
}

/// Whether the map has form lines (ini `form_lines`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormLineMode {
    /// Contours only: lines are traced at the contour interval and every one is a contour.
    None,
    /// Lines are traced at half the contour interval; the half-interval lines the
    /// form-line selection keeps are drawn as form lines (ISOM 103).
    Selective,
}

impl FormLineMode {
    /// The trace interval of a map at `contour_interval`: the vertical distance between
    /// traced lines, in metres.
    pub fn trace_interval(self, contour_interval: f64) -> f64 {
        match self {
            Self::None => contour_interval,
            Self::Selective => contour_interval / 2.0,
        }
    }
}

/// Parameters of [`smoothjoin`], which smooths the traced lines, gives each its contour
/// kind, and picks out depressions, knoll heads and dot knolls.
#[derive(Debug, Clone, PartialEq)]
pub struct SmoothJoinParams {
    /// The map's contour interval in metres (ini `contour_interval`); index contours are
    /// every fifth contour.
    pub contour_interval: f64,
    /// Whether half-interval lines are traced between the contours (ini `form_lines`).
    pub form_lines: FormLineMode,
    /// Weight of the neighbours in the smoothing; bigger smooths more (ini `smoothing`).
    pub smoothing: f64,
    /// How much of the smoothing is added back, exaggerating re-entrants and spurs
    /// (ini `curviness`).
    pub curviness: f64,
    /// Closed lines of fewer vertices than this are tested for depressions and dot knolls
    /// (ini `depression_length`).
    pub depression_length: usize,
    /// Mark each depression's downhill side with a slope line (ini `decorate_depressions`).
    pub decorate_depressions: bool,
    /// How distinct a small closed line must be to stay a contour rather than become a
    /// dot knoll: the share of its vertices that must be steep, and, times 0.45-0.9 m,
    /// its relief (ini `knolls`).
    pub inidotknolls: f64,
    /// The ISOM minimum depression size and slope line length, in ground metres
    /// ([`crate::mapframe::MapFrame::isom_minima`], ini `mapscale`).
    pub isom_minima: IsomMinima,
}

impl SmoothJoinParams {
    /// How the traced lines map to contour kinds: traced every trace interval, index
    /// contours every fifth contour.
    pub fn levels(&self) -> ContourLevels {
        ContourLevels {
            trace_interval: self.form_lines.trace_interval(self.contour_interval),
            index_interval: 5.0 * self.contour_interval,
            half_interval_lines: self.form_lines == FormLineMode::Selective,
        }
    }
}

/// Smooths and joins the contours in `out.dxf.bin`, classes them on `lifted`, the
/// lifted ground model, and picks the dot knolls; writes `out2.dxf.bin` and
/// `dotknolls.bin`.
pub fn smoothjoin(
    fs: &impl FileSystem,
    params: &SmoothJoinParams,
    output_dxf: bool,
    tmpfolder: &Path,
    lifted: &HeightMap,
) -> Result<(), Box<dyn Error>> {
    info!("Smooth curves...");

    let &SmoothJoinParams {
        inidotknolls,
        smoothing,
        curviness,
        depression_length,
        decorate_depressions,
        ..
    } = params;
    let levels = params.levels();

    // in world coordinates
    let xstart = lifted.xoffset;
    let ystart = lifted.yoffset;
    let size = lifted.scale;
    let xmax = (lifted.grid.width() - 1) as u64;
    let ymax = (lifted.grid.height() - 1) as u64;
    let xyz = &lifted.grid;

    let mut steepness = Vec2D::new((xmax + 1) as usize, (ymax + 1) as usize, f64::NAN);

    for i in 1..xmax as usize {
        for j in 1..ymax as usize {
            let mut low: f64 = f64::MAX;
            let mut high: f64 = f64::MIN;
            for ii in i - 1..i + 2 {
                for jj in j - 1..j + 2 {
                    let tmp = xyz[(ii, jj)];
                    if tmp < low {
                        low = tmp;
                    }
                    if tmp > high {
                        high = tmp;
                    }
                }
            }
            steepness[(i, j)] = high - low;
        }
    }

    // read the binary input
    let input = tmpfolder.join("out.dxf.bin");
    let input_dxf =
        BinaryDxf::from_reader(&mut fs.open(input)?).expect("Unable to read out.dxf.bin");

    let input_bounds = input_dxf.bounds().clone(); // store the bounds for usage in the output
    let Geometry::Polylines3(input_lines) = input_dxf.take_geometry().swap_remove(0) else {
        return Err(anyhow::anyhow!(
            "out.dxf.bin holds no 3D contour lines: it is a stale temp file from another build; re-run the full pipeline"
        ).into());
    };

    let mut out2_lines = Polylines::<Point3, (Classification, f64)>::new();

    let mut dotknolls = Vec::new();

    let joined = join_contours(&contours_from_lines(&input_lines), usize::MAX);
    // TODO: this is not very efficient (collecting all x and y separately into Vecs), but it means the logic further down can stay the same
    let mut el_x: Vec<Vec<f64>> = joined
        .iter()
        .map(|c| c.line.iter().map(|p| p.x).collect())
        .collect();
    let mut el_y: Vec<Vec<f64>> = joined
        .iter()
        .map(|c| c.line.iter().map(|p| p.y).collect())
        .collect();
    for l in 0..input_lines.len() {
        let mut el_x_len = el_x[l].len();
        if el_x_len > 0 {
            let mut skip = false;
            let mut depression = 1;
            if el_x_len < 3 {
                skip = true;
                el_x[l].clear();
            }
            let h = joined[l].level_m;
            if !skip
                && el_x_len < depression_length
                && el_x[l].first() == el_x[l].last()
                && el_y[l].first() == el_y[l].last()
            {
                let mut mm: isize = (((el_x_len - 1) as f64) / 3.0).floor() as isize - 1;
                if mm < 0 {
                    mm = 0;
                }
                let mut m = mm as usize;
                let mut x_avg = el_x[l][m];
                let mut y_avg = el_y[l][m];
                while m < el_x_len {
                    let xm = (el_x[l][m] - xstart) / size;
                    let ym = (el_y[l][m] - ystart) / size;
                    if m < el_x_len - 3
                        && ym == ym.floor()
                        && (xm - xm.floor()).abs() > 0.5
                        && ym.floor() != ((el_y[l][0] - ystart) / size).floor()
                        && xm.floor() != ((el_x[l][0] - xstart) / size).floor()
                    {
                        x_avg = xm.floor() * size + xstart;
                        y_avg = el_y[l][m].floor();
                        m += el_x_len;
                    }
                    m += 1;
                }
                let foo_x = ((x_avg - xstart) / size) as usize;
                let foo_y = ((y_avg - ystart) / size) as usize;

                let h_center = xyz[(foo_x, foo_y)];

                let xtest = foo_x as f64 * size + xstart;
                let ytest = foo_y as f64 * size + ystart;

                let inside = Ring::from_xy(&el_x[l], &el_y[l]).contains(Point2::new(xtest, ytest));
                depression = 1;
                if (h_center < h && inside) || (h_center > h && !inside) {
                    depression = -1;
                }
                if !skip {
                    // Check if knoll is distinct enough
                    let mut steepcounter = 0;
                    let mut minele = f64::MAX;
                    let mut maxele = f64::MIN;
                    for k in 0..(el_x_len - 1) {
                        let xx = ((el_x[l][k] - xstart) / size + 0.5) as usize;
                        let yy = ((el_y[l][k] - ystart) / size + 0.5) as usize;
                        let ss = steepness[(xx, yy)];
                        if minele > h - 0.5 * ss {
                            minele = h - 0.5 * ss;
                        }
                        if maxele < h + 0.5 * ss {
                            maxele = h + 0.5 * ss;
                        }
                        if ss > 1.0 {
                            steepcounter += 1;
                        }
                    }

                    if (steepcounter as f64) < 0.4 * (el_x_len as f64 - 1.0)
                        && el_x_len < 41
                        && depression as f64 * h_center - 1.9 < minele
                    {
                        if maxele - 0.45 * inidotknolls < minele {
                            skip = true;
                        }
                        if el_x_len < 33 && maxele - 0.75 * inidotknolls < minele {
                            skip = true;
                        }
                        if el_x_len < 19 && maxele - 0.9 * inidotknolls < minele {
                            skip = true;
                        }
                    }
                    if (steepcounter as f64) < inidotknolls * (el_x_len - 1) as f64 && el_x_len < 15
                    {
                        skip = true;
                    }
                }
            }
            if el_x_len < 5 {
                skip = true;
            }
            if !skip && el_x_len < 15 {
                // dot knoll
                let mut x_avg = 0.0;
                let mut y_avg = 0.0;
                for k in 0..(el_x_len - 1) {
                    x_avg += el_x[l][k];
                    y_avg += el_y[l][k];
                }
                x_avg /= (el_x_len - 1) as f64;
                y_avg /= (el_x_len - 1) as f64;

                dotknolls.push(super::knolls::Dotknoll {
                    x: x_avg,
                    y: y_avg,
                    is_knoll: depression == 1,
                });

                skip = true;
            }

            if !skip {
                // adaptive generalization
                if el_x_len > 101 {
                    let mut newx: Vec<f64> = vec![];
                    let mut newy: Vec<f64> = vec![];
                    let mut xpre = el_x[l][0];
                    let mut ypre = el_y[l][0];

                    newx.push(el_x[l][0]);
                    newy.push(el_y[l][0]);

                    for k in 1..(el_x_len - 1) {
                        let xx = ((el_x[l][k] - xstart) / size + 0.5) as usize;
                        let yy = ((el_y[l][k] - ystart) / size + 0.5) as usize;
                        let ss = steepness[(xx, yy)];
                        if ss.is_nan() || ss < 0.5 {
                            if ((xpre - el_x[l][k]).powi(2) + (ypre - el_y[l][k]).powi(2)).sqrt()
                                >= 4.0
                            {
                                newx.push(el_x[l][k]);
                                newy.push(el_y[l][k]);
                                xpre = el_x[l][k];
                                ypre = el_y[l][k];
                            }
                        } else {
                            newx.push(el_x[l][k]);
                            newy.push(el_y[l][k]);
                            xpre = el_x[l][k];
                            ypre = el_y[l][k];
                        }
                    }
                    newx.push(el_x[l][el_x_len - 1]);
                    newy.push(el_y[l][el_x_len - 1]);

                    el_x[l].clear();
                    el_x[l].append(&mut newx);
                    el_y[l].clear();
                    el_y[l].append(&mut newy);
                    el_x_len = el_x[l].len();
                }
                // Smoothing
                let mut dx: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut dy: Vec<f64> = vec![f64::NAN; el_x_len];

                for k in 2..(el_x_len - 3) {
                    dx[k] = (el_x[l][k - 2]
                        + el_x[l][k - 1]
                        + el_x[l][k]
                        + el_x[l][k + 1]
                        + el_x[l][k + 2]
                        + el_x[l][k + 3])
                        / 6.0;
                    dy[k] = (el_y[l][k - 2]
                        + el_y[l][k - 1]
                        + el_y[l][k]
                        + el_y[l][k + 1]
                        + el_y[l][k + 2]
                        + el_y[l][k + 3])
                        / 6.0;
                }

                let mut xa: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut ya: Vec<f64> = vec![f64::NAN; el_x_len];
                for k in 1..(el_x_len - 1) {
                    xa[k] = (el_x[l][k - 1] + el_x[l][k] / (0.01 + smoothing) + el_x[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    ya[k] = (el_y[l][k - 1] + el_y[l][k] / (0.01 + smoothing) + el_y[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }

                if el_x[l].first() == el_x[l].last() && el_y[l].first() == el_y[l].last() {
                    let vx = (el_x[l][1] + el_x[l][0] / (0.01 + smoothing) + el_x[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (el_y[l][1] + el_y[l][0] / (0.01 + smoothing) + el_y[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    xa[0] = vx;
                    ya[0] = vy;
                    xa[el_x_len - 1] = vx;
                    ya[el_x_len - 1] = vy;
                } else {
                    xa[0] = el_x[l][0];
                    ya[0] = el_y[l][0];
                    xa[el_x_len - 1] = el_x[l][el_x_len - 1];
                    ya[el_x_len - 1] = el_y[l][el_x_len - 1];
                }
                for k in 1..(el_x_len - 1) {
                    el_x[l][k] = (xa[k - 1] + xa[k] / (0.01 + smoothing) + xa[k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    el_y[l][k] = (ya[k - 1] + ya[k] / (0.01 + smoothing) + ya[k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }
                if xa.first() == xa.last() && ya.first() == ya.last() {
                    let vx = (xa[1] + xa[0] / (0.01 + smoothing) + xa[el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (ya[1] + ya[0] / (0.01 + smoothing) + ya[el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    el_x[l][0] = vx;
                    el_y[l][0] = vy;
                    el_x[l][el_x_len - 1] = vx;
                    el_y[l][el_x_len - 1] = vy;
                } else {
                    el_x[l][0] = xa[0];
                    el_y[l][0] = ya[0];
                    el_x[l][el_x_len - 1] = xa[el_x_len - 1];
                    el_y[l][el_x_len - 1] = ya[el_x_len - 1];
                }

                for k in 1..(el_x_len - 1) {
                    xa[k] = (el_x[l][k - 1] + el_x[l][k] / (0.01 + smoothing) + el_x[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    ya[k] = (el_y[l][k - 1] + el_y[l][k] / (0.01 + smoothing) + el_y[l][k + 1])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                }

                if el_x[l].first() == el_x[l].last() && el_y[l].first() == el_y[l].last() {
                    let vx = (el_x[l][1] + el_x[l][0] / (0.01 + smoothing) + el_x[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    let vy = (el_y[l][1] + el_y[l][0] / (0.01 + smoothing) + el_y[l][el_x_len - 2])
                        / (2.0 + 1.0 / (0.01 + smoothing));
                    xa[0] = vx;
                    ya[0] = vy;
                    xa[el_x_len - 1] = vx;
                    ya[el_x_len - 1] = vy;
                } else {
                    xa[0] = el_x[l][0];
                    ya[0] = el_y[l][0];
                    xa[el_x_len - 1] = el_x[l][el_x_len - 1];
                    ya[el_x_len - 1] = el_y[l][el_x_len - 1];
                }

                #[allow(clippy::manual_memcpy)]
                for k in 0..el_x_len {
                    el_x[l][k] = xa[k];
                    el_y[l][k] = ya[k];
                }

                let mut dx2: Vec<f64> = vec![f64::NAN; el_x_len];
                let mut dy2: Vec<f64> = vec![f64::NAN; el_x_len];
                for k in 2..(el_x_len - 3) {
                    dx2[k] = (el_x[l][k - 2]
                        + el_x[l][k - 1]
                        + el_x[l][k]
                        + el_x[l][k + 1]
                        + el_x[l][k + 2]
                        + el_x[l][k + 3])
                        / 6.0;
                    dy2[k] = (el_y[l][k - 2]
                        + el_y[l][k - 1]
                        + el_y[l][k]
                        + el_y[l][k + 1]
                        + el_y[l][k + 2]
                        + el_y[l][k + 3])
                        / 6.0;
                }
                for k in 3..(el_x_len - 3) {
                    let vx = el_x[l][k] + (dx[k] - dx2[k]) * curviness;
                    let vy = el_y[l][k] + (dy[k] - dy2[k]) * curviness;
                    el_x[l][k] = vx;
                    el_y[l][k] = vy;
                }

                let layer =
                    Classification::Contour(levels.kind_at(h).with_depression(depression == -1));

                out2_lines.push(
                    el_x[l]
                        .iter()
                        .zip(el_y[l].iter())
                        .map(|(&x, &y)| Point3::new(x, y, h))
                        .collect(),
                    (layer, h),
                );

                // ISOM 2017-2, symbol 101: "a depression has to have at least one slope
                // line". Without one a depression ring is indistinguishable from a knoll
                // — the reader cannot tell which way the ground goes. KP classified
                // depressions but never drew the tick, and the vector output then folded
                // `depression` into plain 101, so the distinction was lost for good.
                // if the return element happens to be a small depression, lets remove the
                // original countour
                if decorate_depressions
                    && layer.is_depression()
                    && let Some((form, class)) =
                        decorate_depression(&el_x[l], &el_y[l], h, &params.isom_minima)
                {
                    if class == Classification::SmallDepression {
                        out2_lines.pop();
                    }
                    out2_lines.push(form, (class, h));
                }
            } // -- if not dotkoll
        }
    }

    crate::util::write_object(
        &mut fs.create(tmpfolder.join("dotknolls.bin"))?,
        &super::knolls::Dotknolls { dotknolls },
    )?;

    let out2_dxf = BinaryDxf::new(input_bounds, vec![out2_lines.into()]);

    let output = tmpfolder.join("out2.dxf.bin");
    let mut fp = fs.create(output).expect("Unable to create file");
    out2_dxf.to_writer(&mut fp)?;

    if output_dxf {
        out2_dxf.to_dxf(&mut fs.create(tmpfolder.join("out2.dxf"))?)?;
    }

    info!("Done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Classification;
    use crate::geometry::{Point2, Point3, Ring};
    use crate::mapframe::MapFrame;

    /// decorate_depression at the default map scale.
    fn decorate_depression(x: &[f64], y: &[f64], h: f64) -> Option<(Vec<Point3>, Classification)> {
        super::decorate_depression(x, y, h, &MapFrame::default().isom_minima())
    }
    // A closed ring approximating a circle of the given ground radius, in metres.
    fn ring(radius: f64) -> (Vec<f64>, Vec<f64>) {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..=24 {
            let a = i as f64 / 24.0 * std::f64::consts::TAU;
            x.push(100.0 + radius * a.cos());
            y.push(200.0 + radius * a.sin());
        }
        (x, y)
    }

    #[test]
    fn the_tick_points_into_the_depression() {
        let (x, y) = ring(20.0);
        let (tick, class) =
            decorate_depression(&x, &y, 12.5).expect("a 40 m depression carries a slope line");
        assert!(class == Classification::SlopeLine);
        let (start, end) = (&tick[0], &tick[1]);
        let d_start = ((start.x - 100.0).powi(2) + (start.y - 200.0).powi(2)).sqrt();
        let d_end = ((end.x - 100.0).powi(2) + (end.y - 200.0).powi(2)).sqrt();
        assert!(
            d_end < d_start,
            "tick must point inward: {d_start} -> {d_end}"
        );
        assert_eq!(start.z, 12.5);
    }

    #[test]
    fn the_tick_is_the_isom_length() {
        let (x, y) = ring(20.0);
        let (tick, _class) = decorate_depression(&x, &y, 0.0).unwrap();
        // 0.4 mm at 1:15,000 (0.6 mm at 1:10,000) -> 6 m on the ground.
        let len = ((tick[1].x - tick[0].x).powi(2) + (tick[1].y - tick[0].y).powi(2)).sqrt();
        assert!((len - 6.0).abs() < 1e-9, "{len}");
    }

    #[test]
    fn a_ring_below_the_isom_minimum_gets_no_tick() {
        // Under 16.5 x 10.5 m a contour depression may not be drawn at all.
        let (x, y) = ring(4.0);
        let (_tick, class) = decorate_depression(&x, &y, 0.0).unwrap();
        assert!(class == Classification::SmallDepression);
    }

    /// At 1:4 000 (symbols at 100 %) the minimum is 4.4 x 2.8 m and the slope line 1.6 m:
    /// the 8 m ring that is a small depression at 1:10 000 carries a short tick.
    #[test]
    fn the_isom_sizes_follow_the_map_scale() {
        let (x, y) = ring(4.0);
        let minima = MapFrame {
            scale_denominator: 4_000.0,
            ..MapFrame::default()
        }
        .isom_minima();
        let (tick, class) = super::decorate_depression(&x, &y, 0.0, &minima).unwrap();
        assert!(class == Classification::SlopeLine);
        let len = ((tick[1].x - tick[0].x).powi(2) + (tick[1].y - tick[0].y).powi(2)).sqrt();
        assert!((len - 1.6).abs() < 1e-9, "{len}");
    }

    #[test]
    fn winding_does_not_flip_the_tick_outward() {
        let (x, y) = ring(20.0);
        let (rx, ry): (Vec<f64>, Vec<f64>) = (
            x.iter().rev().copied().collect(),
            y.iter().rev().copied().collect(),
        );
        let (tick, _class) = decorate_depression(&rx, &ry, 0.0).unwrap();
        let d_start = ((tick[0].x - 100.0).powi(2) + (tick[0].y - 200.0).powi(2)).sqrt();
        let d_end = ((tick[1].x - 100.0).powi(2) + (tick[1].y - 200.0).powi(2)).sqrt();
        assert!(d_end < d_start, "reversed winding must still point inward");
    }

    /// A crescent: its centroid falls OUTSIDE the ring, which is what sent the tick to
    /// the wrong side on Nyrup Hegn. Shaped like a C opening to the right.
    fn crescent() -> (Vec<f64>, Vec<f64>) {
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..=40 {
            // outer arc, 270 degrees
            let a = i as f64 / 40.0 * (1.5 * std::f64::consts::PI) + 0.25 * std::f64::consts::PI;
            x.push(100.0 + 30.0 * a.cos());
            y.push(200.0 + 30.0 * a.sin());
        }
        for i in (0..=40).rev() {
            // inner arc back
            let a = i as f64 / 40.0 * (1.5 * std::f64::consts::PI) + 0.25 * std::f64::consts::PI;
            x.push(100.0 + 20.0 * a.cos());
            y.push(200.0 + 20.0 * a.sin());
        }
        x.push(x[0]);
        y.push(y[0]);
        (x, y)
    }

    #[test]
    fn a_crescent_ring_still_gets_an_inward_tick() {
        let (x, y) = crescent();
        let (tick, _class) = decorate_depression(&x, &y, 0.0).expect("crescent is big enough");
        assert!(
            Ring::from_xy(&x, &y).contains(Point2::new(tick[1].x, tick[1].y)),
            "tick end must be inside the ring, not outside it"
        );
    }

    #[test]
    fn the_tick_keeps_clear_of_the_contour() {
        let (x, y) = crescent();
        let (tick, _class) = decorate_depression(&x, &y, 0.0).unwrap();
        // The crescent is 10 m wide, so a 6 m tick placed well has room to spare; the
        // failure this guards is a tick laid along or across the ring itself.
        assert!(Ring::from_xy(&x, &y).distance_to_point(Point2::new(tick[1].x, tick[1].y)) > 0.5);
    }
}
