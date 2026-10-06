use log::info;
use rustc_hash::FxHashMap as HashMap;
use std::error::Error;
use std::path::Path;

use crate::geometry::{
    BinaryDxf, Bounds, Classification, Contour, Point2, Point3, Polylines, join_polylines,
};
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::{LasClass, XyzRecord};
use crate::vec2d::Vec2D;

/// Parameters of [`xyz2heightmap`], which grids the ground returns into the ground model.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundParams {
    /// Cell size of the ground model in metres (2; no ini key).
    pub cell_size_m: f64,
}

/// Create the ground model from the tile's returns.
///
/// The grid covers every return; the ground returns and those of `water_class` give
/// each cell the mean of their elevations, and the empty cells are interpolated.
pub fn xyz2heightmap(returns: &[XyzRecord], ground: &GroundParams, water_class: u8) -> HeightMap {
    info!("Generating heightmap...");

    // read all points to find the bounding box
    let mut xmin: f64 = f64::MAX;
    let mut xmax: f64 = f64::MIN;

    let mut ymin: f64 = f64::MAX;
    let mut ymax: f64 = f64::MIN;

    let mut hmin: f64 = f64::MAX;
    let mut hmax: f64 = f64::MIN;

    for r in returns {
        let x: f64 = r.x;
        let y: f64 = r.y;
        let h: f64 = r.z as f64;

        if xmin > x {
            xmin = x;
        }

        if xmax < x {
            xmax = x;
        }

        if ymin > y {
            ymin = y;
        }

        if ymax < y {
            ymax = y;
        }

        if hmin > h {
            hmin = h;
        }

        if hmax < h {
            hmax = h;
        }
    }

    let scale = ground.cell_size_m;

    // align bounding box to a grid with the required scale
    let xmin = (xmin / scale).floor() * scale;
    let ymin = (ymin / scale).floor() * scale;
    let xmax = (xmax / scale).ceil() * scale;
    let ymax = (ymax / scale).ceil() * scale;

    let w: usize = ((xmax - xmin) / scale) as usize + 1;
    let h: usize = ((ymax - ymin) / scale) as usize + 1;

    // a two-dimensional vector of (sum, count) pairs for computing averages
    let mut list_alt = Vec2D::new(w, h, (0f64, 0usize));

    for r in returns {
        if r.class() == LasClass::Ground || r.classification == water_class {
            let x: f64 = r.x;
            let y: f64 = r.y;
            let h: f64 = r.z as f64;

            // +0.5 rounding can push a point on xmax/ymax to idx == w/h.
            // Note: local h is elevation (f64) and shadows grid height — use list_alt dims.
            let idx_x = heightmap_grid_index(x, xmin, scale, list_alt.width());
            let idx_y = heightmap_grid_index(y, ymin, scale, list_alt.height());

            let (sum, count) = &mut list_alt[(idx_x, idx_y)];
            *sum += h;
            *count += 1;
        }
    }

    let mut avg_alt = Vec2D::new(w, h, f64::NAN);

    for x in 0..list_alt.width() {
        for y in 0..list_alt.height() {
            let (sum, count) = &list_alt[(x, y)];

            if *count > 0 {
                avg_alt[(x, y)] = *sum / *count as f64;
            }
        }
    }

    for x in 0..avg_alt.width() {
        for y in 0..avg_alt.height() {
            if avg_alt[(x, y)].is_nan() {
                // interpolate altitude of pixel
                // TODO: optimize to first clasify area then assign values
                let mut i1 = x;
                let mut i2 = x;
                let mut j1 = y;
                let mut j2 = y;

                while i1 > 0 && avg_alt[(i1, y)].is_nan() {
                    i1 -= 1;
                }

                while i2 < w - 1 && avg_alt[(i2, y)].is_nan() {
                    i2 += 1;
                }

                while j1 > 0 && avg_alt[(x, j1)].is_nan() {
                    j1 -= 1;
                }

                while j2 < h - 1 && avg_alt[(x, j2)].is_nan() {
                    j2 += 1;
                }

                let mut val1 = f64::NAN;
                let mut val2 = f64::NAN;

                if !avg_alt[(i1, y)].is_nan() && !avg_alt[(i2, y)].is_nan() {
                    val1 = ((i2 - x) as f64 * avg_alt[(i1, y)]
                        + (x - i1) as f64 * avg_alt[(i2, y)])
                        / ((i2 - i1) as f64);
                }

                if !avg_alt[(x, j1)].is_nan() && !avg_alt[(x, j2)].is_nan() {
                    val2 = ((j2 - y) as f64 * avg_alt[(x, j1)]
                        + (y - j1) as f64 * avg_alt[(x, j2)])
                        / ((j2 - j1) as f64);
                }

                if !val1.is_nan() && !val2.is_nan() {
                    avg_alt[(x, y)] = (val1 + val2) / 2.0;
                } else if !val1.is_nan() {
                    avg_alt[(x, y)] = val1;
                } else if !val2.is_nan() {
                    avg_alt[(x, y)] = val2;
                }
            }
        }
    }

    for x in 0..avg_alt.width() {
        for y in 0..avg_alt.height() {
            if avg_alt[(x, y)].is_nan() {
                // second round of interpolation of altitude of pixel
                let mut val: f64 = 0.0;
                let mut c = 0;

                // iterate 3x3 cell area around the pixel if possible
                for x_idx in x.saturating_sub(1)..=(x + 1).min(avg_alt.width() - 1) {
                    for y_idx in y.saturating_sub(1)..=(y + 1).min(avg_alt.height() - 1) {
                        if !avg_alt[(x_idx, y_idx)].is_nan() {
                            c += 1;
                            val += avg_alt[(x_idx, y_idx)];
                        }
                    }
                }

                if c > 0 {
                    avg_alt[(x, y)] = val / c as f64;
                }
            }
        }
    }

    for x in 0..avg_alt.width() {
        for y in 1..avg_alt.height() {
            if avg_alt[(x, y)].is_nan() {
                avg_alt[(x, y)] = avg_alt[(x, y - 1)];
            }
        }
        for yy in 1..avg_alt.height() {
            let y = avg_alt.height() - 1 - yy;
            if avg_alt[(x, y)].is_nan() {
                avg_alt[(x, y)] = avg_alt[(x, y + 1)];
            }
        }
    }

    // make sure we do not have any NaNs
    for x in 0..avg_alt.width() {
        for y in 0..avg_alt.height() {
            if avg_alt[(x, y)].is_nan() {
                panic!("heightmap should not have any nans, found NaN at ({x}, {y})");
            }
        }
    }

    HeightMap {
        xoffset: xmin,
        yoffset: ymin,
        scale,
        grid: avg_alt,
    }
}

/// Map a world coordinate to a heightmap cell index.
///
/// Karttapullautin uses nearest-cell rounding (+ 0.5). For points exactly on
/// (or within floating-point noise of) the aligned xmax/ymax edge this can
/// produce idx == width/height, which panics in [Vec2D] indexing. Clamp to
/// the last valid cell instead.
fn heightmap_grid_index(v: f64, vmin: f64, scale: f64, n: usize) -> usize {
    let idx = ((v - vmin) / scale + 0.5) as usize;
    idx.min(n.saturating_sub(1))
}

/// Snap `h` to the nearest multiple of `interval`. The tracer steps its level by adding
/// `interval`, which drifts in the last digits; this gives the exact level it stands for.
fn snap_level(h: f64, interval: f64) -> f64 {
    (h / interval + 0.5).floor() * interval
}

/// Join the lines of a contour file written by [`heightmap2contours`] end to end (see
/// [`join_polylines`]). Returns one [`Contour`] per input line, in input order: a joined
/// line keeps the level of the slot it grew from, and absorbed and dropped lines come
/// back with an empty `line`.
pub fn join_contours(
    lines: &Polylines<Point3, (Classification, f64)>,
    max_vertices: usize,
) -> Vec<Contour> {
    let mut flat = Polylines::<Point2, f64>::with_capacity(lines.len());
    for (line, &(_, level_m)) in lines.iter() {
        flat.push(
            line.iter().map(|p| Point2::new(p.x, p.y)).collect(),
            level_m,
        );
    }
    join_polylines(&flat, max_vertices)
        .into_iter()
        .zip(flat.iter())
        .map(|(line, (_, &level_m))| Contour { level_m, line })
        .collect()
}

/// Creates contour lines from a heightmap and writes them as [`Polylines3`](crate::geometry::Geometry::Polylines3)
/// with each line's traced level as its height and every vertex's z.
pub fn heightmap2contours(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    cinterval: f64,
    heightmap: &HeightMap,
    dxffile: &str,
    output_dxf: bool,
) -> Result<(), Box<dyn Error>> {
    info!("Generating curves...");
    let contours = grid2contours(&heightmap.grid, cinterval);

    let xmin = heightmap.xoffset;
    let ymin = heightmap.yoffset;
    let xmax = heightmap.maxx();
    let ymax = heightmap.maxy();
    let size = heightmap.scale;

    // convert the contours to our internal binary dxf format,
    // including some thinning of the lines
    let mut lines = Polylines::new();
    for Contour { level_m, line } in contours.into_iter() {
        lines.push(
            line.iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    // original logic for some kind of "thinning" of the lines
                    let ii = i + 1;
                    let ldata = line.len() - 1;
                    if ii > 5 && ii < ldata - 5 && ldata > 12 && ii % 2 == 0 {
                        return None; // skip this point
                    }

                    // scale the points to world coordinates
                    let x: f64 = p.x * size + xmin;
                    let y: f64 = p.y * size + ymin;

                    Some(Point3::new(x, y, level_m))
                })
                .collect::<Vec<_>>(),
            (Classification::ContourSimple, level_m),
        );
    }
    let dxf = BinaryDxf::new(Bounds::new(xmin, xmax, ymin, ymax), vec![lines.into()]);

    // write to disk
    let mut f = fs
        .create(tmpfolder.join(dxffile))
        .expect("Unable to create file");
    dxf.to_writer(&mut f).expect("Cannot write binary dxf file");

    if output_dxf {
        dxf.to_dxf(&mut fs.create(tmpfolder.join(dxffile.strip_suffix(".bin").unwrap()))?)?;
    }

    info!("Done");

    Ok(())
}

/// Inner function to generate contours from a heightmap.
/// Returns one [`Contour`] per traced line, in grid coordinates, carrying the level it was
/// traced at (the first multiple of `cinterval` above the minimum, plus `cinterval` per step).
/// Note: this will Clone the provided `heightmap`.
pub fn grid2contours(heightmap: &Vec2D<f64>, cinterval: f64) -> Vec<Contour> {
    // clone the heightmap so that we can perform the correction below
    let mut avg_alt = heightmap.clone();

    // As per https://github.com/karttapullautin/karttapullautin/discussions/154#discussioncomment-11393907
    // If elevation grid point elavion equals with contour interval steps you will get contour topology issues
    // (crossing/touching contours). This was implemented to avoid that. 0.02 (two centimeters) is just a random
    // small number to avoid that issue, insignificant enough to matter, but big buffer enough to hopefully make
    // it not get back to "bad value" for it getting rounded somewhere. Sure, it could be some fraction of
    // contour interval, but in real world 2 cm is insignificant enough.
    for (_, _, ele) in avg_alt.iter_mut() {
        let temp: f64 = (*ele / cinterval + 0.5).floor() * cinterval;
        if (*ele - temp).abs() < 0.02 {
            if *ele - temp < 0.0 {
                *ele = temp - 0.02;
            } else {
                *ele = temp + 0.02;
            }
        }
    }

    // compute hmin and hmax
    let mut hmin: f64 = f64::MAX;
    let mut hmax: f64 = f64::MIN;
    for (_, _, h) in avg_alt.iter() {
        if h < hmin {
            hmin = h;
        }
        if h > hmax {
            hmax = h;
        }
    }

    let v = cinterval;

    // we start at the first level that is above hmin (anything below that will just have empty contours)
    let mut level: f64 = (hmin / v).ceil() * v;

    let mut contours = Vec::<Contour>::new();

    loop {
        if level >= hmax {
            break;
        }

        let mut obj = Vec::<(i64, i64, u8)>::new();
        let mut curves: HashMap<(i64, i64, u8), (i64, i64)> = HashMap::default();

        // iterate over all "corners" of the grid
        for i in 0..(avg_alt.width() - 1) {
            for j in 0..(avg_alt.height() - 1) {
                let mut a = avg_alt[(i, j)];
                let mut b = avg_alt[(i, j + 1)];
                let mut c = avg_alt[(i + 1, j)];
                let mut d = avg_alt[(i + 1, j + 1)];

                // if all corners are below or above the level, skip
                if a < level && b < level && c < level && d < level
                    || a > level && b > level && c > level && d > level
                {
                    continue;
                }

                let temp: f64 = (a / v + 0.5).floor() * v;
                if (a - temp).abs() < 0.05 {
                    if a - temp < 0.0 {
                        a = temp - 0.05;
                    } else {
                        a = temp + 0.05;
                    }
                }

                let temp: f64 = (b / v + 0.5).floor() * v;
                if (b - temp).abs() < 0.05 {
                    if b - temp < 0.0 {
                        b = temp - 0.05;
                    } else {
                        b = temp + 0.05;
                    }
                }

                let temp: f64 = (c / v + 0.5).floor() * v;
                if (c - temp).abs() < 0.05 {
                    if c - temp < 0.0 {
                        c = temp - 0.05;
                    } else {
                        c = temp + 0.05;
                    }
                }

                let temp: f64 = (d / v + 0.5).floor() * v;
                if (d - temp).abs() < 0.05 {
                    if d - temp < 0.0 {
                        d = temp - 0.05;
                    } else {
                        d = temp + 0.05;
                    }
                }

                if a < b {
                    if level < b && level > a {
                        let x1: f64 = i as f64;
                        let y1: f64 = j as f64 + (level - a) / (b - a);
                        if level > c {
                            let x2: f64 = i as f64 + (b - level) / (b - c);
                            let y2: f64 = j as f64 + (level - c) / (b - c);
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        } else if level < c {
                            let x2: f64 = i as f64 + (level - a) / (c - a);
                            let y2: f64 = j as f64;
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        }
                    }
                } else if b < a && level < a && level > b {
                    let x1: f64 = i as f64;
                    let y1: f64 = j as f64 + (a - level) / (a - b);
                    if level < c {
                        let x2: f64 = i as f64 + (level - b) / (c - b);
                        let y2: f64 = j as f64 + (c - level) / (c - b);
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    } else if level > c {
                        let x2: f64 = i as f64 + (a - level) / (a - c);
                        let y2: f64 = j as f64;
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    }
                }

                if a < c {
                    if level < c && level > a {
                        let x1: f64 = i as f64 + (level - a) / (c - a);
                        let y1: f64 = j as f64;
                        if level > b {
                            let x2: f64 = i as f64 + (level - b) / (c - b);
                            let y2: f64 = j as f64 + (c - level) / (c - b);
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        }
                    }
                } else if a > c && level < a && level > c {
                    let x1: f64 = i as f64 + (a - level) / (a - c);
                    let y1: f64 = j as f64;
                    if level < b {
                        let x2: f64 = i as f64 + (b - level) / (b - c);
                        let y2: f64 = j as f64 + (level - c) / (b - c);
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    }
                }

                if c < d {
                    if level < d && level > c {
                        let x1: f64 = i as f64 + 1.0;
                        let y1: f64 = j as f64 + (level - c) / (d - c);
                        if level < b {
                            let x2: f64 = i as f64 + (b - level) / (b - c);
                            let y2: f64 = j as f64 + (level - c) / (b - c);
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        } else if level > b {
                            let x2: f64 = i as f64 + (level - b) / (d - b);
                            let y2: f64 = j as f64 + 1.0;
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        }
                    }
                } else if c > d && level < c && level > d {
                    let x1: f64 = i as f64 + 1.0;
                    let y1: f64 = j as f64 + (c - level) / (c - d);
                    if level > b {
                        let x2: f64 = i as f64 + (level - b) / (c - b);
                        let y2: f64 = j as f64 + (c - level) / (c - b);
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    } else if level < b {
                        let x2: f64 = i as f64 + (b - level) / (b - d);
                        let y2: f64 = j as f64 + 1.0;
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    }
                }

                if d < b {
                    if level < b && level > d {
                        let x1: f64 = i as f64 + (b - level) / (b - d);
                        let y1: f64 = j as f64 + 1.0;
                        if level > c {
                            let x2: f64 = i as f64 + (b - level) / (b - c);
                            let y2: f64 = j as f64 + (level - c) / (b - c);
                            check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                        }
                    }
                } else if b < d && level < d && level > b {
                    let x1: f64 = i as f64 + (level - b) / (d - b);
                    let y1: f64 = j as f64 + 1.0;
                    if level < c {
                        let x2: f64 = i as f64 + (level - b) / (c - b);
                        let y2: f64 = j as f64 + (c - level) / (c - b);
                        check_obj_in(&mut obj, &mut curves, x1, x2, y1, y2);
                    }
                }
            }
        }

        for k in obj.iter() {
            if curves.contains_key(k) {
                let mut polyline = Vec::<Point2>::new();
                let (x, y, _) = *k;
                polyline.push(Point2::new(x as f64 / 100.0, y as f64 / 100.0));

                let mut res = (x, y);

                let (x, y) = *curves.get(k).unwrap();
                polyline.push(Point2::new(x as f64 / 100.0, y as f64 / 100.0));
                curves.remove(k);

                let mut head = (x, y);

                if curves.get(&(head.0, head.1, 1)).is_some_and(|v| *v == res) {
                    curves.remove(&(head.0, head.1, 1));
                }
                if curves.get(&(head.0, head.1, 2)).is_some_and(|v| *v == res) {
                    curves.remove(&(head.0, head.1, 2));
                }
                loop {
                    if curves.get(&(head.0, head.1, 1)).is_some_and(|v| *v != res) {
                        res = head;

                        let (x, y) = *curves.get(&(head.0, head.1, 1)).unwrap();
                        polyline.push(Point2::new(x as f64 / 100.0, y as f64 / 100.0));
                        curves.remove(&(head.0, head.1, 1));

                        head = (x, y);
                        if curves.get(&(head.0, head.1, 1)).is_some_and(|v| *v == res) {
                            curves.remove(&(head.0, head.1, 1));
                        }
                        if curves.get(&(head.0, head.1, 2)).is_some_and(|v| *v == res) {
                            curves.remove(&(head.0, head.1, 2));
                        }
                    } else if curves.get(&(head.0, head.1, 2)).is_some_and(|v| *v != res) {
                        res = head;

                        let (x, y) = *curves.get(&(head.0, head.1, 2)).unwrap();
                        polyline.push(Point2::new(x as f64 / 100.0, y as f64 / 100.0));
                        curves.remove(&(head.0, head.1, 2));

                        head = (x, y);
                        if curves.get(&(head.0, head.1, 1)).is_some_and(|v| *v == res) {
                            curves.remove(&(head.0, head.1, 1));
                        }
                        if curves.get(&(head.0, head.1, 2)).is_some_and(|v| *v == res) {
                            curves.remove(&(head.0, head.1, 2));
                        }
                    } else {
                        contours.push(Contour {
                            level_m: snap_level(level, v),
                            line: polyline,
                        });
                        break;
                    }
                }
            }
        }
        level += v;
    }

    contours
}

fn check_obj_in(
    obj: &mut Vec<(i64, i64, u8)>,
    curves: &mut HashMap<(i64, i64, u8), (i64, i64)>,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
) {
    // convert the coordinates to integers with 2 decimal places for use as keys
    let x1 = (x1 * 100.0).floor() as i64;
    let x2 = (x2 * 100.0).floor() as i64;
    let y1 = (y1 * 100.0).floor() as i64;
    let y2 = (y2 * 100.0).floor() as i64;

    if x1 != x2 || y1 != y2 {
        let key = (x1, y1, 1);
        if !curves.contains_key(&key) {
            curves.insert(key, (x2, y2));
            obj.push(key);
        } else {
            let key = (x1, y1, 2);
            curves.insert(key, (x2, y2));
            obj.push(key);
        }
        let key = (x2, y2, 1);
        if !curves.contains_key(&key) {
            curves.insert(key, (x1, y1));
            obj.push(key);
        } else {
            let key = (x2, y2, 2);
            curves.insert(key, (x1, y1));
            obj.push(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::contours;

    #[test]
    fn ground_model_is_the_mean_of_ground_and_water_returns_per_cell() {
        use crate::contours::{GroundParams, xyz2heightmap};
        use crate::io::xyz::XyzRecord;
        let ret = |x, y, z, classification| XyzRecord {
            x,
            y,
            z,
            classification,
            ..Default::default()
        };
        let returns = [
            ret(0.0, 0.0, 10.0, 2),
            ret(0.4, 0.2, 12.0, 2),
            // high vegetation: inside the grid, but its 100 m reach no cell
            ret(2.0, 0.0, 100.0, 5),
            ret(4.0, 0.0, 5.0, 9),
            ret(4.2, 0.1, 7.0, 2),
        ];
        let ground = GroundParams { cell_size_m: 2.0 };

        let hmap = xyz2heightmap(&returns, &ground, 9);
        assert_eq!((hmap.xoffset, hmap.yoffset, hmap.scale), (0.0, 0.0, 2.0));
        // x 0..4.2 snaps out to 0..6, y 0..0.2 to 0..2: 4 x 2 cells
        assert_eq!((hmap.grid.width(), hmap.grid.height()), (4, 2));
        assert_eq!(hmap.grid[(0, 0)], 11.0);
        assert_eq!(hmap.grid[(2, 0)], 6.0);
        // the empty cell between them is interpolated, not the vegetation return
        assert_eq!(hmap.grid[(1, 0)], 8.5);

        // with another water class the class-9 return is ignored too
        let hmap = xyz2heightmap(&returns, &ground, 0);
        assert_eq!(hmap.grid[(2, 0)], 7.0);
    }

    #[test]
    fn test_heightmap_grid_index_clamps_edge() {
        let scale = 0.8_f64;
        let vmin = 0.0_f64;
        let n = 10_usize;
        // v = vmin + n*scale => raw idx == n (OOB without clamp).
        let v_edge = vmin + (n as f64) * scale;
        let raw = ((v_edge - vmin) / scale + 0.5) as usize;
        assert_eq!(raw, n);
        assert_eq!(super::heightmap_grid_index(v_edge, vmin, scale, n), n - 1);
        assert_eq!(super::heightmap_grid_index(vmin, vmin, scale, n), 0);
    }

    #[test]
    fn test_grid2contours_empty() {
        let grid = crate::vec2d::Vec2D::new(5, 5, 0.0);
        let contours = contours::grid2contours(&grid, 1.0);
        assert!(
            contours.is_empty(),
            "Expected no contours for a uniform grid"
        );
    }

    #[test]
    fn test_grid2contours_single_contour() {
        let mut grid = crate::vec2d::Vec2D::new(5, 5, 0.0);
        grid[(2, 2)] = 1.1;
        let contours = contours::grid2contours(&grid, 1.0);
        println!("Contours: {contours:?}");
        assert_eq!(
            contours.len(),
            1,
            "Expected one contour for a single contour line"
        );
        assert_eq!(
            contours[0].line.len(),
            7,
            "Expected contour to have 4 points"
        );
    }

    #[test]
    fn test_grid2contours_single_contour2() {
        let mut grid = crate::vec2d::Vec2D::new(5, 5, 2.0);
        grid[(2, 2)] = 1.1;
        let contours = contours::grid2contours(&grid, 1.0);
        println!("Contours: {contours:?}");
        assert_eq!(
            contours.len(),
            1,
            "Expected one contour for a single contour line"
        );
        assert_eq!(
            contours[0].line.len(),
            7,
            "Expected contour to have 4 points"
        );
    }

    use super::*;
    use crate::io::fs::memory::MemoryFileSystem;

    fn grid(w: usize, h: usize, f: impl Fn(f64, f64) -> f64) -> Vec2D<f64> {
        let mut g = Vec2D::new(w, h, 0.0);
        for i in 0..w {
            for j in 0..h {
                g[(i, j)] = f(i as f64, j as f64);
            }
        }
        g
    }

    /// A cone: `peak - distance to (c, c)`.
    fn cone(c: f64, peak: f64) -> impl Fn(f64, f64) -> f64 {
        move |x, y| peak - ((x - c).powi(2) + (y - c).powi(2)).sqrt()
    }

    /// Trace `grid` into a contour file with [`heightmap2contours`] and read it back.
    fn contour_file(
        grid: Vec2D<f64>,
        offset: (f64, f64),
        scale: f64,
        cinterval: f64,
    ) -> Polylines<Point3, (Classification, f64)> {
        let fs = MemoryFileSystem::new();
        let hmap = HeightMap {
            xoffset: offset.0,
            yoffset: offset.1,
            scale,
            grid,
        };
        heightmap2contours(&fs, Path::new(""), cinterval, &hmap, "c.dxf.bin", false).unwrap();
        let dxf = BinaryDxf::from_reader(&mut fs.open("c.dxf.bin").unwrap()).unwrap();
        match dxf.take_geometry().swap_remove(0) {
            crate::geometry::Geometry::Polylines3(lines) => lines,
            _ => panic!("contour files hold Polylines3"),
        }
    }

    /// The smoothjoin level lookup this branch deleted: interpolate the heightmap at the
    /// first vertex (from a third of the way in) that lies exactly on a grid line. NaN when
    /// no vertex does.
    fn old_smoothjoin_level(
        line: &[Point2],
        xyz: &Vec2D<f64>,
        (xstart, ystart, size): (f64, f64, f64),
        interval: f64,
    ) -> f64 {
        let n = line.len();
        let mut m = ((((n - 1) as f64) / 3.0).floor() as isize - 1).max(0) as usize;
        while m < n {
            let (xm, ym) = (line[m].x, line[m].y);
            if (xm - xstart) / size == ((xm - xstart) / size).floor() {
                let xx = ((xm - xstart) / size) as usize;
                let yy = ((ym - ystart) / size) as usize;
                let h1 = xyz[(xx, yy)];
                if yy < xyz.height() - 1 {
                    let h2 = xyz[(xx, yy + 1)];
                    let h3 = h1 * (yy as f64 + 1.0 - (ym - ystart) / size)
                        + h2 * ((ym - ystart) / size - yy as f64);
                    return (h3 / interval + 0.5).floor() * interval;
                }
                return (h1 / interval + 0.5).floor() * interval;
            } else if m < n - 1 && (ym - ystart) / size == ((ym - ystart) / size).floor() {
                let xx = ((xm - xstart) / size) as usize;
                let yy = ((ym - ystart) / size) as usize;
                let h1 = xyz[(xx, yy)];
                if xx < xyz.width() - 1 {
                    let h2 = xyz[(xx + 1, yy)];
                    let h3 = h1 * (xx as f64 + 1.0 - (xm - xstart) / size)
                        + h2 * ((xm - xstart) / size - xx as f64);
                    return (h3 / interval + 0.5).floor() * interval;
                }
                return (h1 / interval + 0.5).floor() * interval;
            }
            m += 1;
        }
        f64::NAN
    }

    /// The knolldetector level lookup this branch deleted, on a closed ring: the first
    /// vertex on a vertical grid line wins, else the last on a horizontal one. 0.0 when
    /// no vertex is on a grid line.
    fn old_knoll_level(
        line: &[Point2],
        xyz: &Vec2D<f64>,
        (xstart, ystart, size): (f64, f64, f64),
        interval: f64,
    ) -> f64 {
        let n = line.len();
        let mut l = line.to_vec();
        l.push(line[0]);
        let mut m = ((n as f64 / 3.0).floor() - 1.0).max(0.0) as usize;
        let mut h = 0.0;
        while m < l.len() {
            let xo = (l[m].x - xstart) / size;
            let yo = (l[m].y - ystart) / size;
            let at = |x: usize, y: usize| xyz.get((x, y)).copied().unwrap_or(0.0);
            if xo == xo.floor() {
                let (x, y) = (xo.floor() as usize, yo.floor() as usize);
                h = at(x, y) * (yo.floor() + 1.0 - yo) + at(x, y + 1) * (yo - yo.floor());
                return (h / interval + 0.5).floor() * interval;
            } else if m < n - 3 && yo == yo.floor() {
                let (x, y) = (xo.floor() as usize, yo.floor() as usize);
                h = at(x, y) * (xo.floor() + 1.0 - xo) + at(x + 1, y) * (xo - xo.floor());
                h = (h / interval + 0.5).floor() * interval;
            }
            m += 1;
        }
        h
    }

    fn closed(line: &[Point2]) -> bool {
        line.len() > 1 && line.first() == line.last()
    }

    #[test]
    fn cone_gives_nested_closed_rings_at_their_levels() {
        // peak 9.5 at (10, 10); rings of level 0..=9 have radius 9.5 - level <= 9.5 and
        // stay inside the 21 x 21 grid, lower levels hit the border
        let contours = grid2contours(&grid(21, 21, cone(10.0, 9.5)), 1.0);
        for level in 0..=9 {
            let level = level as f64;
            let rings: Vec<_> = contours.iter().filter(|c| c.level_m == level).collect();
            assert_eq!(rings.len(), 1, "one ring at level {level}");
            let ring = &rings[0].line;
            assert!(closed(ring), "the ring at level {level} is closed");
            for p in ring {
                let r = ((p.x - 10.0).powi(2) + (p.y - 10.0).powi(2)).sqrt();
                assert!(
                    (r - (9.5 - level)).abs() < 0.25,
                    "vertex {p:?} of level {level} at radius {r}"
                );
            }
        }
        // every contour sits at one of the traced levels
        assert!(contours.iter().all(|c| c.level_m == c.level_m.round()));
        // levels are exact multiples even where stepping by the interval drifts
        let contours = grid2contours(&grid(21, 21, cone(10.0, 9.5)), 0.3);
        assert!(
            contours
                .iter()
                .all(|c| c.level_m == (c.level_m / 0.3).round() * 0.3)
        );
    }

    #[test]
    fn tilted_plane_gives_one_line_per_level_at_its_level() {
        let contours = grid2contours(&grid(12, 6, |x, _| 0.5 * x + 0.1), 1.0);
        let levels: Vec<f64> = contours.iter().map(|c| c.level_m).collect();
        assert_eq!(levels, [1.0, 2.0, 3.0, 4.0, 5.0]);
        for c in &contours {
            assert!(!closed(&c.line));
            for p in &c.line {
                assert!(
                    (0.5 * p.x + 0.1 - c.level_m).abs() < 0.06,
                    "vertex {p:?} of level {}",
                    c.level_m
                );
            }
        }
    }

    #[test]
    fn contour_file_carries_the_traced_level() {
        let lines = contour_file(grid(12, 6, |x, _| 0.5 * x + 0.1), (100.0, 200.0), 2.0, 1.0);
        let levels: Vec<f64> = lines.iter().map(|(_, &(_, h))| h).collect();
        assert_eq!(levels, [1.0, 2.0, 3.0, 4.0, 5.0]);
        for (line, &(class, h)) in lines.iter() {
            assert_eq!(class, Classification::ContourSimple);
            assert!(line.iter().all(|p| p.z == h));
        }
    }

    #[test]
    fn joining_keeps_the_level_of_the_slot_it_grew_from() {
        let mut lines = Polylines::new();
        let seg = |a: (f64, f64), b: (f64, f64), h| {
            vec![Point3::new(a.0, a.1, h), Point3::new(b.0, b.1, h)]
        };
        let c = Classification::ContourSimple;
        lines.push(seg((5.0, 5.0), (6.0, 5.0), 4.0), (c, 4.0));
        lines.push(seg((0.0, 0.0), (1.0, 0.0), 3.0), (c, 3.0));
        lines.push(seg((1.0, 0.0), (2.0, 0.0), 3.0), (c, 3.0));
        let joined = join_contours(&lines, usize::MAX);
        assert_eq!(joined.len(), 3);
        assert_eq!(joined[0].level_m, 4.0);
        assert_eq!(joined[0].line.len(), 2);
        assert_eq!(joined[1].level_m, 3.0);
        assert_eq!(
            joined[1].line,
            [(0.0, 0.0), (1.0, 0.0), (1.0, 0.0), (2.0, 0.0)].map(|(x, y)| Point2::new(x, y))
        );
        assert!(joined[2].line.is_empty());
    }

    /// Where the deleted lookups found a level, the snapped traced level is the same value.
    #[test]
    fn snapped_level_matches_the_deleted_lookups() {
        // a tilted cone: closed rings near the top, open lines at the border
        let surface = |x: f64, y: f64| cone(15.0, 30.0)(x, y) + 0.3 * x;
        let g = grid(31, 31, surface);
        let frame = (1000.0, 2000.0, 1.0);

        // smoothjoin: out.dxf.bin at the half interval, joined without a vertex limit
        let interval = 1.25;
        let file = contour_file(g.clone(), (frame.0, frame.1), frame.2, interval);
        let mut found = 0;
        for c in join_contours(&file, usize::MAX) {
            if c.line.len() < 3 {
                continue;
            }
            let old = old_smoothjoin_level(&c.line, &g, frame, interval);
            if !old.is_nan() {
                assert_eq!(old, c.level_m);
                found += 1;
            }
        }
        assert!(found > 10, "the old lookup found {found} levels");

        // knolldetector: contours03.dxf.bin at 0.3 m, closed rings of up to 121 vertices
        let interval = 0.3;
        let file = contour_file(g.clone(), (frame.0, frame.1), frame.2, interval);
        let mut found = 0;
        for c in join_contours(&file, 201) {
            if c.line.len() < 3 || c.line.len() > 121 || !closed(&c.line) {
                continue;
            }
            let old = old_knoll_level(&c.line, &g, frame, interval);
            if old != 0.0 {
                assert_eq!(old, c.level_m);
                found += 1;
            }
        }
        assert!(found > 10, "the old lookup found {found} levels");
    }

    /// At a 2.6 m cell and a 1.625 m trace interval (not binary fractions) the old
    /// lookup's exact on-grid test can miss every vertex and return NaN; the traced level
    /// is always there.
    #[test]
    fn level_is_known_where_the_old_lookup_gave_nan() {
        let interval = 2.5 / 2.0 * 1.3;
        let frame = (1000.1, 2000.3, 2.0 * 1.3);
        let g = grid(31, 31, |x, y| cone(15.0, 30.0)(x, y) + 0.3 * x);
        let file = contour_file(g.clone(), (frame.0, frame.1), frame.2, interval);
        let mut nan = 0;
        for c in join_contours(&file, usize::MAX) {
            if c.line.len() < 3 {
                continue;
            }
            let h = c.level_m;
            assert!(h.is_finite());
            // every vertex lies at the level (grid coordinates back from world metres)
            for p in &c.line {
                let (x, y) = ((p.x - frame.0) / frame.2, (p.y - frame.1) / frame.2);
                let z = cone(15.0, 30.0)(x, y) + 0.3 * x;
                assert!((z - h).abs() < 0.5, "vertex {p:?} at {z}, level {h}");
            }
            if old_smoothjoin_level(&c.line, &g, frame, interval).is_nan() {
                nan += 1;
            }
        }
        assert!(nan > 0, "the old lookup found every level");
    }

    /// The knolldetector counterpart: with a 0.52 m cell at map coordinates its exact
    /// on-grid test misses every vertex of some closed rings, which got level 0; the
    /// traced level is right.
    #[test]
    fn level_is_known_where_the_old_knoll_lookup_gave_zero() {
        let interval = 0.3 * 1.3;
        let frame = (385000.0, 6712000.0, 0.52);
        let surface = |x: f64, y: f64| cone(15.0, 30.0)(x, y) + 0.3 * x;
        let g = grid(31, 31, surface);
        let file = contour_file(g.clone(), (frame.0, frame.1), frame.2, interval);
        let mut zero = 0;
        for c in join_contours(&file, 201) {
            if c.line.len() < 3 || c.line.len() > 121 || !closed(&c.line) {
                continue;
            }
            assert!(c.level_m > 0.0);
            for p in &c.line {
                let (x, y) = ((p.x - frame.0) / frame.2, (p.y - frame.1) / frame.2);
                let z = surface(x, y);
                assert!(
                    (z - c.level_m).abs() < 0.5,
                    "vertex {p:?} at {z}, level {}",
                    c.level_m
                );
            }
            if old_knoll_level(&c.line, &g, frame, interval) == 0.0 {
                zero += 1;
            }
        }
        assert!(zero > 0, "the old lookup found every level");
    }
}
