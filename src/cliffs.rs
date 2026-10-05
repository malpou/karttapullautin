use image::{Rgb, RgbImage};
use log::info;
use rand::prelude::*;
use std::borrow::Cow;
use std::error::Error;
use std::path::Path;

use crate::geometry::{BinaryDxf, Bounds, Classification, Point2, Polylines};
use crate::io::bytes::FromToBytes;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::{LasClass, XyzInternalReader};
use crate::util::cliff_thinning_rng;
use crate::vec2d::Vec2D;

/// Parameters of [`makecliffs`].
///
/// The first pass compares each ground return in a bin with the returns of the 3x3 bins
/// around it: a drop above `c1_limit` (raised with the local relief) is a cliff dash
/// (ISOM 202), above `c2_limit` (raised on steep ground) an impassable one (ISOM 201).
/// The second pass compares ground model cells the same way against the fixed
/// `cliff4_limit` and adds impassable cliff dashes. Local relief is the height range, in
/// metres, of the 7x7 ground model cells around a return, less `flat_place`.
#[derive(Debug, Clone, PartialEq)]
pub struct CliffParams {
    /// Least drop of a cliff on flat ground (ini `cliff1`). Metres.
    pub c1_limit: f64,
    /// Least drop of an impassable cliff on flat ground (ini `cliff2`). Metres.
    pub c2_limit: f64,
    /// Least drop of a second-pass impassable cliff, whatever the local relief (ini
    /// `cliff4_limit`, default 7.15). Metres. Was the hidden constant `2.6 * 2.75`,
    /// the same f64.
    pub cliff4_limit: f64,
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

/// `tile` (the tile name) seeds the `cliffthin` sampling; `output_dxf` also writes the
/// `.dxf` next to each `.dxf.bin`.
pub fn makecliffs(
    fs: &impl FileSystem,
    params: &CliffParams,
    output_dxf: bool,
    tmpfolder: &Path,
    tile: &str,
) -> Result<(), Box<dyn Error>> {
    info!("Identifying cliffs...");

    let &CliffParams {
        c1_limit,
        c2_limit,
        cliff4_limit,
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

    let heightmap_in = tmpfolder.join("xyz2.hmap");
    let hmap = HeightMap::from_bytes(&mut fs.open(&heightmap_in)?)?;

    // in world coordinates
    let xmax = hmap.maxx();
    let ymax = hmap.maxy();
    let xmin = hmap.minx();
    let ymin = hmap.miny();

    let xstart = hmap.xoffset;
    let ystart = hmap.yoffset;
    let size = hmap.scale;

    let sxmax = hmap.grid.width() - 1;
    let symax = hmap.grid.height() - 1;

    let mut steepness = Vec2D::new(sxmax + 1, symax + 1, f64::NAN);

    for i in 3..sxmax - 4 {
        for j in 3..symax - 4 {
            let mut low: f64 = f64::MAX;
            let mut high: f64 = f64::MIN;
            for ii in i - 3..i + 4 {
                for jj in j - 3..j + 4 {
                    let value = hmap.grid[(ii, jj)];

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

    let xyz_file_in = tmpfolder.join("xyztemp.xyz.bin");

    let mut rng = cliff_thinning_rng(tile);
    let randdist = rand::distr::Bernoulli::new(cliff_thin).unwrap();

    let mut reader = XyzInternalReader::new(fs.open(&xyz_file_in)?)?;
    while let Some(chunk) = reader.next_chunk()? {
        for r in chunk {
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

    let f2_dxf = BinaryDxf::new(Bounds::new(xmin, xmax, ymin, ymax), vec![f2_lines.into()]);

    // save binary file
    let mut f2 = fs
        .create(tmpfolder.join("c2g.dxf.bin"))
        .expect("Unable to create file");
    f2_dxf.to_writer(&mut f2).expect("Cannot write c2g.dxf.bin");

    if output_dxf {
        f2_dxf.to_dxf(&mut fs.create(tmpfolder.join("c2g.dxf"))?)?;
    }

    drop(f2_dxf);

    // if we drop this already here, we can reuse the memory for the second list_alt
    drop(list_alt);

    let mut list_alt = Vec2D::new(
        (((xmax - xmin) / bin_m).ceil() + 1.0) as usize,
        (((ymax - ymin) / bin_m).ceil() + 1.0) as usize,
        Vec::<(f64, f64, f64)>::new(),
    );

    for (x, y, h) in hmap.iter() {
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
                    let limit = cliff4_limit;
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

    let f3_dxf = BinaryDxf::new(Bounds::new(xmin, xmax, ymin, ymax), vec![f3_lines.into()]);

    // save binary file
    let mut f3 = fs
        .create(tmpfolder.join("c3g.dxf.bin"))
        .expect("Unable to create file");
    f3_dxf.to_writer(&mut f3).expect("Cannot write c3g.dxf.bin");

    if output_dxf {
        f3_dxf.to_dxf(&mut fs.create(tmpfolder.join("c3g.dxf"))?)?;
    }

    img.write_to(
        &mut fs
            .create(tmpfolder.join("c2.png"))
            .expect("could not save output png"),
        image::ImageFormat::Png,
    )
    .expect("could not save output png");

    info!("Done");
    Ok(())
}
