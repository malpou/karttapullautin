use image::{Rgb, RgbImage};
use log::info;
use rand::prelude::*;
use std::borrow::Cow;
use std::error::Error;
use std::path::Path;

use anyhow::Context;

use crate::geometry::{BinaryDxf, Bounds, Classification, Geometry, Point2, Polylines};
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::{LasClass, XyzRecord};
use crate::util::cliff_thinning_rng;
use crate::vec2d::Vec2D;

/// Parameters of [`makecliffs`].
///
/// The first pass compares each ground return in a bin with the returns of the 3x3 bins
/// around it: a drop above `c1_limit` (raised with the local relief) is a cliff dash
/// (ISOM 202), above `c2_limit` (raised on steep ground) an impassable one (ISOM 201).
/// The second pass compares ground model cells the same way against the fixed
/// `ground_model_drop_m` and adds impassable cliff dashes. Local relief is the height range, in
/// metres, of the 7x7 ground model cells around a return, less `flat_place`.
#[derive(Debug, Clone, PartialEq)]
pub struct CliffParams {
    /// Least drop of a cliff on flat ground (ini `cliff1`). Metres.
    pub c1_limit: f64,
    /// Least drop of an impassable cliff on flat ground (ini `cliff2`). Metres.
    pub c2_limit: f64,
    /// Least drop between ground model cells of a second-pass impassable cliff, whatever
    /// the local relief (ini `cliff_ground_drop`, default 7.15). Metres. Was the hidden
    /// constant `2.6 * 2.75`, the same f64.
    pub ground_model_drop_m: f64,
    /// Share of the returns and cells sampled, 0 to 1 (ini `cliffthin`).
    pub cliff_thin: f64,
    /// Raises the impassable cliff limit by this times `c2_limit` per metre of local
    /// relief above `no_small_cliffs` (ini `cliffsteepfactor`).
    pub steep_factor: f64,
    /// Height range below which ground counts as flat (ini `cliffflatplace`). Metres.
    pub flat_place: f64,
    /// Height range from which no cliffs are drawn, only impassable ones, and the
    /// impassable cliff limit starts to rise (ini `cliffnosmallcliffs`, 0 for None).
    /// Metres. None means 6.0 above `flat_place`.
    pub no_small_cliffs: Option<f64>,
    /// Side of the square bins the returns and cells are grouped in. Metres.
    pub bin_m: f64,
    /// A bin of more points than this keeps every n-th, n = floor((len - 1) /
    /// (`bin_max_points` - 1)) + 1, before it is compared.
    pub bin_max_points: usize,
    /// The same for the points of a bin and its eight neighbours together.
    pub neighbourhood_max_points: usize,
    /// A drop over horizontal distance `dist` is a cliff when it exceeds the limit and
    /// `limit + (dist - limit) * drop_slope`.
    pub drop_slope: f64,
    /// Half length of a cliff dash, across the drop. Metres.
    pub dash_half_length_m: f64,
}

/// The debug intermediate of the passable cliffs, [`CliffSet::passable`]; `c2g.dxf`, its
/// text DXF, is a DXF-family product.
pub const PASSABLE_DUMP: &str = "c2g.dxf.bin";
/// The debug intermediate of the impassable cliffs, [`CliffSet::impassable`]; `c3g.dxf`,
/// its text DXF, is a DXF-family product.
pub const IMPASSABLE_DUMP: &str = "c3g.dxf.bin";
/// The debug raster of the pixels the first pass marked with a passable cliff dash.
pub const PASSABLE_RASTER_DUMP: &str = "c2.png";

/// The cliff dashes [`makecliffs`] found: short lines across each drop, in world
/// coordinates, in the order the passes found them.
#[derive(Debug, Clone)]
pub struct CliffSet {
    /// The passable cliff dashes ([`Classification::Cliff2`], ISOM 202) of the first pass.
    pub passable: Polylines<Point2, Classification>,
    /// The impassable cliff dashes (ISOM 201): the first pass's, between returns
    /// ([`Classification::Cliff3`]), then the second pass's, between ground model cells
    /// ([`Classification::Cliff4`]).
    pub impassable: Polylines<Point2, Classification>,
    /// The ground model's extent, its minimum floored to the 3 m bins.
    pub bounds: Bounds,
}

impl CliffSet {
    /// The [`PASSABLE_DUMP`] debug intermediate.
    pub fn passable_bindxf(&self) -> BinaryDxf {
        BinaryDxf::new(self.bounds.clone(), vec![self.passable.clone().into()])
    }

    /// The [`IMPASSABLE_DUMP`] debug intermediate.
    pub fn impassable_bindxf(&self) -> BinaryDxf {
        BinaryDxf::new(self.bounds.clone(), vec![self.impassable.clone().into()])
    }

    /// [`CliffSet::passable_bindxf`] and [`CliffSet::impassable_bindxf`] without copying
    /// the lines.
    pub fn into_bindxf(self) -> (BinaryDxf, BinaryDxf) {
        (
            BinaryDxf::new(self.bounds.clone(), vec![self.passable.into()]),
            BinaryDxf::new(self.bounds, vec![self.impassable.into()]),
        )
    }

    /// The cliffs in the [`PASSABLE_DUMP`] and [`IMPASSABLE_DUMP`] dumps, as
    /// [`CliffSet::passable_bindxf`] and [`CliffSet::impassable_bindxf`] write them; the
    /// bounds are the passable dump's (both dumps carry the same).
    pub fn from_bindxf(passable: BinaryDxf, impassable: BinaryDxf) -> anyhow::Result<Self> {
        let bounds = passable.bounds().clone();
        let lines = |dxf: BinaryDxf, what| match dxf.take_geometry().swap_remove(0) {
            Geometry::Polylines2(lines) => Ok(lines),
            _ => anyhow::bail!("the {what} cliffs hold no 2D lines"),
        };
        Ok(Self {
            passable: lines(passable, "passable")?,
            impassable: lines(impassable, "impassable")?,
            bounds,
        })
    }
}

/// Write `cliffs` to `tmpfolder`: with `debug` the debug intermediates [`PASSABLE_DUMP`],
/// [`IMPASSABLE_DUMP`] and `passable_raster` as [`PASSABLE_RASTER_DUMP`], with
/// `output_dxf` the text DXFs `c2g.dxf` and `c3g.dxf`.
pub fn write_cliffs(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    cliffs: &CliffSet,
    passable_raster: &RgbImage,
    debug: bool,
    output_dxf: bool,
) -> Result<(), Box<dyn Error>> {
    for (name, dxf) in [
        (PASSABLE_DUMP, cliffs.passable_bindxf()),
        (IMPASSABLE_DUMP, cliffs.impassable_bindxf()),
    ] {
        crate::contours::write_dxf_files(fs, tmpfolder, name, &dxf, debug, output_dxf)?;
    }
    if debug {
        let path = tmpfolder.join(PASSABLE_RASTER_DUMP);
        fs.create(&path)
            .map_err(anyhow::Error::from)
            .and_then(|mut f| Ok(passable_raster.write_to(&mut f, image::ImageFormat::Png)?))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Finds the cliff dashes in `returns`, against the local relief of the `ground` model,
/// and between the `ground` model's cells. `tile` (the tile name) seeds the `cliffthin`
/// sampling, which draws once per return in `returns` order and then once per cell.
/// Returns the cliffs and the raster of the pixels the first pass marked with a passable
/// cliff dash, 1 m per pixel from the bounds' minimum (the [`PASSABLE_RASTER_DUMP`]).
pub fn makecliffs(
    ground: &HeightMap,
    returns: &[XyzRecord],
    tile: &str,
    params: &CliffParams,
) -> (CliffSet, RgbImage) {
    info!("Identifying cliffs...");

    let &CliffParams {
        c1_limit,
        c2_limit,
        ground_model_drop_m,
        cliff_thin,
        steep_factor,
        flat_place,
        no_small_cliffs,
        bin_m,
        bin_max_points,
        neighbourhood_max_points,
        drop_slope,
        dash_half_length_m: cliff_length,
    } = params;

    let no_small_cliffs = no_small_cliffs.map_or(6.0, |steep| steep - flat_place);

    // in world coordinates
    let xmax = ground.maxx();
    let ymax = ground.maxy();
    let xmin = ground.minx();
    let ymin = ground.miny();

    let xstart = ground.xoffset;
    let ystart = ground.yoffset;
    let size = ground.scale;

    let sxmax = ground.grid.width() - 1;
    let symax = ground.grid.height() - 1;

    let mut steepness = Vec2D::new(sxmax + 1, symax + 1, f64::NAN);

    for i in 3..sxmax - 4 {
        for j in 3..symax - 4 {
            let mut low: f64 = f64::MAX;
            let mut high: f64 = f64::MIN;
            for ii in i - 3..i + 4 {
                for jj in j - 3..j + 4 {
                    let value = ground.grid[(ii, jj)];

                    if value < low {
                        low = value;
                    }
                    if value > high {
                        high = value;
                    }
                }
            }
            steepness[(i, j)] = high - low;
        }
    }

    let mut img = RgbImage::from_pixel(
        (xmax - xmin) as u32,
        (ymax - ymin) as u32,
        Rgb([255, 255, 255]),
    );

    let xmin = (xmin / bin_m).floor() * bin_m;
    let ymin = (ymin / bin_m).floor() * bin_m;

    let mut list_alt = Vec2D::new(
        (((xmax - xmin) / bin_m).ceil() + 1.0) as usize,
        (((ymax - ymin) / bin_m).ceil() + 1.0) as usize,
        Vec::<(f64, f64, f64)>::new(),
    );

    let mut rng = cliff_thinning_rng(tile);
    let randdist = rand::distr::Bernoulli::new(cliff_thin).unwrap();

    for r in returns {
        if cliff_thin == 1.0 || rng.sample(randdist) {
            let (x, y, h) = (r.x, r.y, r.z as f64);
            if r.class() == LasClass::Ground {
                list_alt[(
                    ((x - xmin).floor() / bin_m) as usize,
                    ((y - ymin).floor() / bin_m) as usize,
                )]
                    .push((x, y, h));
            }
        }
    }

    let w = ((xmax - xmin).floor() / bin_m) as usize;
    let h = ((ymax - ymin).floor() / bin_m) as usize;

    let mut f2_lines = Polylines::new();
    let mut f3_lines = Polylines::new();

    // temporary vector to reuse memory allocations
    let mut t = Vec::<(f64, f64, f64)>::new();
    for x in 0..w + 1 {
        for y in 0..h + 1 {
            if !list_alt[(x, y)].is_empty() {
                t.clear();
                if x >= 1 {
                    if y >= 1 {
                        t.extend(&list_alt[(x - 1, y - 1)]);
                    }
                    t.extend(&list_alt[(x - 1, y)]);
                    if y < h {
                        t.extend(&list_alt[(x - 1, y + 1)]);
                    }
                }
                if y >= 1 {
                    t.extend(&list_alt[(x, y - 1)]);
                }
                t.extend(&list_alt[(x, y)]);
                if y < h {
                    t.extend(&list_alt[(x, y + 1)]);
                }
                if x < w {
                    if y >= 1 {
                        t.extend(&list_alt[(x + 1, y - 1)]);
                    }
                    t.extend(&list_alt[(x + 1, y)]);
                    if y < h {
                        t.extend(&list_alt[(x + 1, y + 1)]);
                    }
                }
                // use a Cow to avoid unnecessary allocation in the case when we don't need to modify the list
                let mut d = Cow::Borrowed(&list_alt[(x, y)]);

                if d.len() > bin_max_points {
                    // since we need to modify it, we need to convert it to mutable
                    // this will actually mutate the outer `d`
                    let d = d.to_mut();

                    // if d has too many points, thin it by keeping every b point
                    let b = ((d.len() - 1) as f64 / (bin_max_points - 1) as f64) as usize + 1;
                    let mut idx = 0;
                    d.retain(|_| {
                        idx += 1;
                        idx % b == 0
                    });
                }
                if t.len() > neighbourhood_max_points {
                    // if t has too many points, thin it by keeping every b point
                    let b =
                        ((t.len() - 1) as f64 / (neighbourhood_max_points - 1) as f64) as usize + 1;
                    let mut idx = 0;
                    t.retain(|_| {
                        idx += 1;
                        idx % b == 0
                    })
                }
                let mut temp_max: f64 = f64::MIN;
                let mut temp_min: f64 = f64::MAX;
                for rec in t.iter() {
                    let h0 = rec.2;
                    if temp_max < h0 {
                        temp_max = h0;
                    }
                    if temp_min > h0 {
                        temp_min = h0;
                    }
                }
                if temp_max - temp_min < c1_limit * 0.999 {
                    // no cliffs to add, continue
                    continue;
                }

                for &(x0, y0, h0) in d.iter() {
                    let mut steep = steepness[(
                        ((x0 - xstart) / size) as usize,
                        ((y0 - ystart) / size) as usize,
                    )] - flat_place;
                    if steep.is_nan() {
                        steep = -flat_place;
                    }

                    steep = steep.clamp(0.0, 17.0);

                    let bonus =
                        (c2_limit - c1_limit) * (1.0 - (no_small_cliffs - steep) / no_small_cliffs);
                    let limit = c1_limit + bonus;
                    let mut bonus = c2_limit * steep_factor * (steep - no_small_cliffs);
                    if bonus < 0.0 {
                        bonus = 0.0;
                    }
                    let limit2 = c2_limit + bonus;
                    for &(xt, yt, ht) in t.iter() {
                        let temp = h0 - ht;
                        let dist = ((x0 - xt).powi(2) + (y0 - yt).powi(2)).sqrt();
                        if dist > 0.0 {
                            let imgx = ((x0 + xt) / 2.0 - xmin + 0.5) as u32;
                            let imgy = ((y0 + yt) / 2.0 - ymin + 0.5) as u32;
                            if steep < no_small_cliffs
                                && temp > limit
                                && temp > (limit + (dist - limit) * drop_slope)
                                && imgx < img.width()
                                && imgy < img.height()
                            {
                                let p = img.get_pixel(imgx, imgy);
                                if p[0] == 255 {
                                    img.put_pixel(imgx, imgy, Rgb([0, 0, 0]));

                                    f2_lines.push(
                                        vec![
                                            Point2::new(
                                                (x0 + xt) / 2.0 + cliff_length * (y0 - yt) / dist,
                                                (y0 + yt) / 2.0 - cliff_length * (x0 - xt) / dist,
                                            ),
                                            Point2::new(
                                                (x0 + xt) / 2.0 - cliff_length * (y0 - yt) / dist,
                                                (y0 + yt) / 2.0 + cliff_length * (x0 - xt) / dist,
                                            ),
                                        ],
                                        Classification::Cliff2,
                                    );
                                }
                            }

                            if temp > limit2 && temp > (limit2 + (dist - limit2) * drop_slope) {
                                f3_lines.push(
                                    vec![
                                        Point2::new(
                                            (x0 + xt) / 2.0 + cliff_length * (y0 - yt) / dist,
                                            (y0 + yt) / 2.0 - cliff_length * (x0 - xt) / dist,
                                        ),
                                        Point2::new(
                                            (x0 + xt) / 2.0 - cliff_length * (y0 - yt) / dist,
                                            (y0 + yt) / 2.0 + cliff_length * (x0 - xt) / dist,
                                        ),
                                    ],
                                    Classification::Cliff3,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // if we drop this already here, we can reuse the memory for the second list_alt
    drop(list_alt);

    let mut list_alt = Vec2D::new(
        (((xmax - xmin) / bin_m).ceil() + 1.0) as usize,
        (((ymax - ymin) / bin_m).ceil() + 1.0) as usize,
        Vec::<(f64, f64, f64)>::new(),
    );

    for (x, y, h) in ground.iter() {
        if cliff_thin == 1.0 || rng.sample(randdist) {
            list_alt[(
                ((x - xmin).floor() / bin_m) as usize,
                ((y - ymin).floor() / bin_m) as usize,
            )]
                .push((x, y, h));
        }
    }

    // temporary vector to reuse memory allocations
    let mut t = Vec::<(f64, f64, f64)>::new();
    for x in 0..w + 1 {
        for y in 0..h + 1 {
            let d = &list_alt[(x, y)];
            if !d.is_empty() {
                t.clear();
                if x >= 1 {
                    if y >= 1 {
                        t.extend(&list_alt[(x - 1, y - 1)]);
                    }
                    t.extend(&list_alt[(x - 1, y)]);
                    if y < h {
                        t.extend(&list_alt[(x - 1, y + 1)]);
                    }
                }
                if y >= 1 {
                    t.extend(&list_alt[(x, y - 1)]);
                }
                t.extend(&list_alt[(x, y)]);
                if y < h {
                    t.extend(&list_alt[(x, y + 1)]);
                }
                if x < w {
                    if y >= 1 {
                        t.extend(&list_alt[(x + 1, y - 1)]);
                    }
                    t.extend(&list_alt[(x + 1, y)]);
                    if y < h {
                        t.extend(&list_alt[(x + 1, y + 1)]);
                    }
                }

                for &(x0, y0, h0) in d.iter() {
                    let limit = ground_model_drop_m;
                    for &(xt, yt, ht) in t.iter() {
                        let temp = h0 - ht;
                        let dist = ((x0 - xt).powi(2) + (y0 - yt).powi(2)).sqrt();
                        if dist > 0.0
                            && temp > limit
                            && temp > (limit + (dist - limit) * drop_slope)
                        {
                            f3_lines.push(
                                vec![
                                    Point2::new(
                                        (x0 + xt) / 2.0 + cliff_length * (y0 - yt) / dist,
                                        (y0 + yt) / 2.0 - cliff_length * (x0 - xt) / dist,
                                    ),
                                    Point2::new(
                                        (x0 + xt) / 2.0 - cliff_length * (y0 - yt) / dist,
                                        (y0 + yt) / 2.0 + cliff_length * (x0 - xt) / dist,
                                    ),
                                ],
                                Classification::Cliff4,
                            );
                        }
                    }
                }
            }
        }
    }

    let cliffs = CliffSet {
        passable: f2_lines,
        impassable: f3_lines,
        bounds: Bounds::new(xmin, xmax, ymin, ymax),
    };
    info!("Done");
    (cliffs, img)
}

#[cfg(test)]
mod tests {
    use super::{
        CliffParams, CliffSet, IMPASSABLE_DUMP, PASSABLE_DUMP, PASSABLE_RASTER_DUMP, makecliffs,
        write_cliffs,
    };
    use crate::geometry::{BinaryDxf, Classification, Point2};
    use crate::io::fs::FileSystem;
    use crate::io::fs::memory::MemoryFileSystem;
    use crate::io::heightmap::HeightMap;
    use crate::io::xyz::XyzRecord;
    use crate::vec2d::Vec2D;
    use std::path::Path;

    /// The template's cliff parameters (`pullauta.default.ini`).
    fn template() -> CliffParams {
        CliffParams {
            c1_limit: 1.15,
            c2_limit: 2.0,
            ground_model_drop_m: 7.15,
            cliff_thin: 1.0,
            steep_factor: 0.38,
            flat_place: 3.5,
            no_small_cliffs: Some(5.5),
            bin_m: 3.0,
            bin_max_points: 31,
            neighbourhood_max_points: 301,
            drop_slope: 0.85,
            dash_half_length_m: 1.47,
        }
    }

    /// The cliffs `makecliffs` finds for two ground returns a metre apart across x = 310 m,
    /// 103 m and 100 m high, on a 20 x 20 m ground model of 1 m cells that is `step`
    /// metres higher west of x = 310 m.
    fn cliff_set_across_a_step(step: f64) -> CliffSet {
        let mut grid = Vec2D::new(20, 20, 100.0);
        for i in 0..10 {
            for j in 0..20 {
                grid[(i, j)] = 100.0 + step;
            }
        }
        let ground = HeightMap {
            xoffset: 300.0,
            yoffset: 600.0,
            scale: 1.0,
            grid,
        };
        let ret = |x, z| XyzRecord {
            x,
            y: 610.0,
            z,
            classification: 2,
            ..Default::default()
        };
        let returns = [ret(309.5, 103.0), ret(310.5, 100.0)];
        makecliffs(&ground, &returns, "", &template()).0
    }

    /// [`cliff_set_across_a_step`]'s (passable, impassable) dashes.
    fn cliffs_across_a_step(step: f64) -> [Vec<(Vec<Point2>, Classification)>; 2] {
        let cliffs = cliff_set_across_a_step(step);
        [cliffs.passable, cliffs.impassable].map(|lines| lines.into_iter().collect())
    }

    /// The dash across a 3 m drop over 1 m: centred between the returns, along the step,
    /// 2 x 1.47 m long.
    fn dash(class: Classification) -> (Vec<Point2>, Classification) {
        (
            vec![Point2::new(310.0, 611.47), Point2::new(310.0, 608.53)],
            class,
        )
    }

    /// A 3 m step is flat ground for the local relief (3 m < `cliffflatplace` 3.5 m), so the
    /// 3 m drop between the returns is one cliff dash: a cliff (above `cliff1` 1.15 m) and,
    /// in the first pass, an impassable one (above `cliff2` 2.0 m). The second pass finds
    /// none: no two ground model cells are `cliff_ground_drop` 7.15 m apart.
    #[test]
    fn a_3_m_step_is_one_cliff_line() {
        let [c2g, c3g] = cliffs_across_a_step(3.0);
        assert_eq!(c2g, [dash(Classification::Cliff2)]);
        assert_eq!(c3g, [dash(Classification::Cliff3)]);
        let (line, _) = &c2g[0];
        let length = ((line[0].x - line[1].x).powi(2) + (line[0].y - line[1].y).powi(2)).sqrt();
        assert!((length - 2.94).abs() < 1e-9, "{length}");
    }

    /// The local relief comes from the ground model: on a 6 m step (relief 6 m, 2.5 m above
    /// `cliffflatplace`, past the 2 m `cliffnosmallcliffs` range) the same returns draw no
    /// passable cliff, only the impassable one, its limit raised to 2.38 m.
    #[test]
    fn steep_ground_drops_the_passable_cliff() {
        let [c2g, c3g] = cliffs_across_a_step(6.0);
        assert!(c2g.is_empty(), "{c2g:?}");
        assert_eq!(c3g, [dash(Classification::Cliff3)]);
    }

    /// The second pass compares the ground model's own cells against `cliff_ground_drop`
    /// (7.15 m). An 8 m step is past it: impassable cliff dashes (Cliff4) between cells
    /// across the step, among them the one between the neighbouring cells at x = 309 m
    /// and 310 m. The returns' 3 m drop draws nothing on ground that steep.
    #[test]
    fn the_second_pass_finds_the_ground_model_step_above_cliff_ground_drop() {
        let [c2g, c3g] = cliffs_across_a_step(8.0);
        assert!(c2g.is_empty(), "{c2g:?}");
        assert!(!c3g.is_empty());
        for (line, class) in &c3g {
            assert_eq!(*class, Classification::Cliff4);
            let length = ((line[0].x - line[1].x).powi(2) + (line[0].y - line[1].y).powi(2)).sqrt();
            assert!((length - 2.94).abs() < 1e-9, "{length}");
        }
        let neighbours = (
            vec![Point2::new(309.5, 611.47), Point2::new(309.5, 608.53)],
            Classification::Cliff4,
        );
        assert!(c3g.contains(&neighbours), "{c3g:?}");

        // just under the limit, the second pass finds nothing
        let [_, c3g] = cliffs_across_a_step(7.0);
        assert!(
            c3g.iter()
                .all(|(_, class)| *class != Classification::Cliff4),
            "{c3g:?}"
        );
    }

    /// The dumps hold the set as before, the impassable ones after the first pass's in
    /// one file, and read back into the same set; the text DXFs and the raster are
    /// written only when asked for.
    #[test]
    fn write_cliffs_writes_the_dumps_under_the_flag_and_reads_them_back() {
        // both passes: the returns' 3 m drop and the 8 m step between cells
        let (cliffs, raster) = {
            let mut grid = Vec2D::new(20, 20, 100.0);
            for i in 0..10 {
                for j in 0..20 {
                    grid[(i, j)] = 108.0;
                }
            }
            let ground = HeightMap {
                xoffset: 300.0,
                yoffset: 600.0,
                scale: 1.0,
                grid,
            };
            let ret = |x, z| XyzRecord {
                x,
                y: 610.0,
                z,
                classification: 2,
                ..Default::default()
            };
            let mut params = template();
            // flat ground for the returns, so the first pass draws its dashes too
            params.flat_place = 9.0;
            params.no_small_cliffs = None;
            makecliffs(
                &ground,
                &[ret(309.5, 103.0), ret(310.5, 100.0)],
                "",
                &params,
            )
        };
        let classes = |lines: &crate::geometry::Polylines<Point2, Classification>| {
            lines.iter().map(|(_, &c)| c).collect::<Vec<_>>()
        };
        assert_eq!(classes(&cliffs.passable), [Classification::Cliff2]);
        let impassable = classes(&cliffs.impassable);
        assert_eq!(impassable[0], Classification::Cliff3);
        assert!(impassable[1..].iter().all(|&c| c == Classification::Cliff4));
        assert!(impassable.len() > 1);

        let tmp = Path::new("tmp");
        let fs = MemoryFileSystem::new();
        fs.create_dir_all(tmp).unwrap();
        write_cliffs(&fs, tmp, &cliffs, &raster, false, false).unwrap();
        assert!(fs.list(tmp).unwrap().is_empty());

        write_cliffs(&fs, tmp, &cliffs, &raster, false, true).unwrap();
        let mut names: Vec<_> = fs.list(tmp).unwrap();
        names.sort();
        assert_eq!(names, [tmp.join("c2g.dxf"), tmp.join("c3g.dxf")]);

        let fs = MemoryFileSystem::new();
        fs.create_dir_all(tmp).unwrap();
        write_cliffs(&fs, tmp, &cliffs, &raster, true, false).unwrap();
        let mut names: Vec<_> = fs.list(tmp).unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                tmp.join(PASSABLE_RASTER_DUMP),
                tmp.join(PASSABLE_DUMP),
                tmp.join(IMPASSABLE_DUMP)
            ]
        );
        let read = |name| BinaryDxf::from_reader(&mut fs.open(tmp.join(name)).unwrap()).unwrap();
        let back = CliffSet::from_bindxf(read(PASSABLE_DUMP), read(IMPASSABLE_DUMP)).unwrap();
        assert_eq!(
            back.passable.iter().collect::<Vec<_>>(),
            cliffs.passable.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            back.impassable.iter().collect::<Vec<_>>(),
            cliffs.impassable.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            (
                back.bounds.xmin,
                back.bounds.xmax,
                back.bounds.ymin,
                back.bounds.ymax
            ),
            (300.0, 320.0, 600.0, 620.0)
        );
    }
}
