use anyhow::Context;
use image::{GrayImage, Luma};
use imageproc::drawing::draw_line_segment_mut;
use log::info;
use rustc_hash::FxHashMap as HashMap;
use rustc_hash::FxHashSet;
use std::error::Error;
use std::path::Path;

use crate::contours::join_contours;
use crate::geometry::{
    BinaryDxf, Bounds, Classification, Geometry, Point2, Points, Polylines, Ring,
};
use crate::io::bytes::FromToBytes;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::vec2d::Vec2D;

/// Parameters of the knoll stage: [`knolldetector`] picks the closed contours that become
/// knolls, [`xyzknolls`] lifts the ground model under them (the knoll lift), and
/// [`dotknolls`] sorts the dot knolls into clean and ugly ones.
///
/// Provenance: every default is the constant of the original Perl `knolldetector`,
/// `xyzknolls` and `dotknolls`, kept as it was (ADR 0001). Nothing records what data they
/// were tuned on. Fields named `_m` are metres compared directly with elevations or
/// ground distances; `scalefactor` does not scale them. `settled_max_lift`,
/// `settled_top_height`, `high_lift_ratio`, `low_lift_ratio` and `shrink_min_lift` are
/// shares of the level spacing, which includes `scalefactor`.
#[derive(Debug, Clone, PartialEq)]
pub struct KnollParams {
    /// Scales the map: pixel size of the dot knoll image and part of the level spacing
    /// (ini `scalefactor`). Stays here until the scalefactor split.
    pub scalefactor: f64,
    /// Contour interval in metres (ini `contour_interval`). The knoll levels step by half
    /// of it times `scalefactor`, and the candidate thresholds tuned at 5 m scale with it.
    pub contour_interval: f64,

    // knolldetector: which closed contours are knoll candidates
    /// Lines of this many vertices or more are dropped before the end-to-end join.
    pub join_max_vertices: usize,
    /// Joined lines of more vertices than this are dropped (terrain, not knolls). Checked
    /// on every joined line, before the test that it is closed.
    pub max_ring_vertices: usize,
    /// Joined lines of fewer vertices than this must pass `min_short_ring_length_m` and
    /// `min_ring_vertices`.
    pub short_ring_vertices: usize,
    /// A line below `short_ring_vertices` whose length is under this is dropped. Metres.
    pub min_short_ring_length_m: f64,
    /// A line below `short_ring_vertices` with fewer vertices than this is dropped.
    pub min_ring_vertices: usize,
    /// A candidate lies at least this far below its top contour. Metres.
    pub min_drop_below_top_m: f64,
    /// A candidate lies less than this far below its top contour, else it belongs to a
    /// larger hill. Metres.
    pub max_drop_below_top_m: f64,
    /// A top keeps its current best candidate when that candidate's lift to the next knoll
    /// level is under this many fifths of the contour interval (times `scalefactor`)…
    pub settled_max_lift: f64,
    /// … and the top stands this many fifths of the interval above it…
    pub settled_top_height: f64,
    /// … give or take this much. Metres.
    pub settled_top_tolerance_m: f64,
    /// A best candidate of fewer vertices than this is always kept.
    pub small_ring_vertices: usize,
    /// A larger best candidate is kept when its top rises more than this above it. Metres.
    pub min_top_rise_m: f64,
    /// … or when it sits more than this above the half-interval level below it. Metres.
    pub min_level_offset_m: f64,

    // xyzknolls: the knoll lift
    /// Radius in cells of the square window that measures local relief before the lift.
    pub flatten_radius_cells: usize,
    /// Cells whose window relief is under this are pulled towards the window mean, the
    /// more the flatter. Metres.
    pub flatten_max_relief_m: f64,
    /// A knoll within this of a knoll level counts as on it: the lift aims at the level
    /// above. Metres.
    pub level_tolerance_m: f64,
    /// Added to the lift that brings the knoll to the next level. Metres.
    pub lift_margin_m: f64,
    /// Share of the lift given to the smoothing around the knoll.
    pub surround_share: f64,
    /// That share when the lift exceeds `high_lift_ratio` of the level spacing.
    pub surround_share_high: f64,
    /// Lifts above this share of the level spacing are high.
    pub high_lift_ratio: f64,
    /// Lifts below this share of the level spacing are low: no smoothing around, and
    /// `low_lift_extra_m` more lift.
    pub low_lift_ratio: f64,
    /// Extra lift of a low lift. Metres.
    pub low_lift_extra_m: f64,
    /// Extra lift of every knoll. Metres.
    pub lift_extra_m: f64,
    /// Taken off the lift when it would carry the top contour past the level above the
    /// next one. Metres.
    pub overshoot_cut_m: f64,
    /// A knoll whose lift exceeds this many 2.5ths of the level spacing (times
    /// `scalefactor`) and has more than `shrink_min_vertices` vertices is shrunk first.
    pub shrink_min_lift: f64,
    /// See `shrink_min_lift`.
    pub shrink_min_vertices: usize,
    /// Shrinks the knoll's ring towards its centre by this factor.
    pub shrink_factor: f64,
    /// The smoothing radius around a knoll is this share of the distance to the nearest
    /// other knoll, in cells…
    pub surround_range_share: f64,
    /// … less this many cells…
    pub surround_range_trim_cells: f64,
    /// … clamped to at least this many cells…
    pub min_surround_range_cells: f64,
    /// … and at most this many.
    pub max_surround_range_cells: f64,
    /// Ground model heights this close to a knoll level move this far off it, so contours
    /// at the level neither cross nor touch. Metres.
    pub level_clearance_m: f64,

    // dotknolls
    /// Half-width in dot knoll image pixels (`scalefactor` metres) of the square around a
    /// dot knoll that must hold no contour, else the dot knoll is ugly.
    pub dot_clearance_px: f64,
}

impl Default for KnollParams {
    fn default() -> Self {
        Self {
            scalefactor: 1.0,
            contour_interval: 5.0,
            join_max_vertices: 201,
            max_ring_vertices: 121,
            short_ring_vertices: 9,
            min_short_ring_length_m: 5.0,
            min_ring_vertices: 3,
            min_drop_below_top_m: 0.1,
            max_drop_below_top_m: 4.6,
            settled_max_lift: 1.75,
            settled_top_height: 0.6,
            settled_top_tolerance_m: 0.2,
            small_ring_vertices: 13,
            min_top_rise_m: 0.45,
            min_level_offset_m: 0.45,
            flatten_radius_cells: 2,
            flatten_max_relief_m: 1.25,
            level_tolerance_m: 0.09,
            lift_margin_m: 0.15,
            surround_share: 0.4,
            surround_share_high: 0.6,
            high_lift_ratio: 0.66,
            low_lift_ratio: 0.25,
            low_lift_extra_m: 0.3,
            lift_extra_m: 0.5,
            overshoot_cut_m: 0.4,
            shrink_min_lift: 1.5,
            shrink_min_vertices: 21,
            shrink_factor: 0.8,
            surround_range_share: 0.8,
            surround_range_trim_cells: 1.0,
            min_surround_range_cells: 1.0,
            max_surround_range_cells: 12.0,
            level_clearance_m: 0.02,
            dot_clearance_px: 3.0,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Dotknolls {
    pub dotknolls: Vec<Dotknoll>,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Dotknoll {
    pub x: f64,
    pub y: f64,
    pub is_knoll: bool,
}

pub fn dotknolls(
    fs: &impl FileSystem,
    params: &KnollParams,
    output_dxf: bool,
    tmpfolder: &Path,
) -> Result<(), Box<dyn Error>> {
    info!("Identifying dotknolls...");

    let scalefactor = params.scalefactor;
    let clearance = params.dot_clearance_px;

    let heightmap_in = tmpfolder.join("xyz_knolls.hmap");
    let hmap = HeightMap::from_bytes(&mut fs.open(heightmap_in)?)?;

    // in world coordinates
    let xstart = hmap.xoffset;
    let ystart = hmap.yoffset;

    // in grid coordinates
    let xmax = (hmap.grid.width() - 1) as f64;
    let ymax = (hmap.grid.height() - 1) as f64;
    let size = hmap.scale;

    let mut im = GrayImage::from_pixel(
        (xmax * size / scalefactor) as u32,
        (ymax * size / scalefactor) as u32,
        Luma([0xff]),
    );

    let data = BinaryDxf::from_reader(&mut fs.open(tmpfolder.join("out2.dxf.bin"))?)?;
    let Geometry::Polylines3(lines) = data.take_geometry().swap_remove(0) else {
        return Err(anyhow::anyhow!("out2.dxf.bin should contain polylines").into());
    };

    for (line, _) in lines.iter() {
        for i in 1..line.len() {
            draw_line_segment_mut(
                &mut im,
                (
                    ((line[i - 1].x - xstart) / scalefactor).floor() as f32,
                    ((line[i - 1].y - ystart) / scalefactor).floor() as f32,
                ),
                (
                    ((line[i].x - xstart) / scalefactor).floor() as f32,
                    ((line[i].y - ystart) / scalefactor).floor() as f32,
                ),
                Luma([0x0]),
            )
        }
    }

    let mut dotknoll_points = Points::new();

    let dotknolls: Dotknolls =
        crate::util::read_object(&mut fs.open(tmpfolder.join("dotknolls.bin"))?)?;

    for dot in dotknolls.dotknolls {
        let Dotknoll { x, y, is_knoll } = dot;

        let mut ok = true;
        let mut i = (x - xstart) / scalefactor - clearance;
        while i < (x - xstart) / scalefactor + (clearance + 1.0) && ok {
            if (i as u32) >= im.width() {
                ok = false;
                break;
            }
            let mut j = (y - ystart) / scalefactor - clearance;
            while j < (y - ystart) / scalefactor + (clearance + 1.0) && ok {
                if (j as u32) >= im.height() {
                    ok = false;
                    break;
                }
                let pix = im.get_pixel(i as u32, j as u32);
                if pix[0] == 0 {
                    ok = false;
                    break;
                }
                j += 1.0;
            }
            i += 1.0;
        }

        let layer2 = match (ok, is_knoll) {
            (true, true) => Classification::Dotknoll,
            (true, false) => Classification::Udepression,
            (false, true) => Classification::UglyDotknoll,
            (false, false) => Classification::UglyUdepression,
        };

        dotknoll_points.push(Point2::new(x, y), layer2);
    }

    let dxf = BinaryDxf::new(
        Bounds::new(xstart, xmax * size + xstart, ystart, ymax * size + ystart),
        vec![dotknoll_points.into()],
    );

    // write binary
    let mut f = fs
        .create(tmpfolder.join("dotknolls.dxf.bin"))
        .expect("Unable to create file");
    dxf.to_writer(&mut f)
        .expect("could not write dotknolls.dxf.bin");

    if output_dxf {
        dxf.to_dxf(&mut fs.create(tmpfolder.join("dotknolls.dxf"))?)?;
    }

    info!("Done");
    Ok(())
}
pub fn knolldetector(
    fs: &impl FileSystem,
    params: &KnollParams,
    output_dxf: bool,
    tmpfolder: &Path,
) -> anyhow::Result<()> {
    info!("Detecting knolls...");
    let scalefactor = params.scalefactor;
    let contour_interval = params.contour_interval;

    let halfinterval = contour_interval / 2.0 * scalefactor;

    // the thresholds were tuned at a 5 m contour interval; this scales them to the map's
    let contours_ratio = contour_interval / 5.0 * scalefactor;

    let hmap = read_heightmap(fs, &tmpfolder.join("xyz_03.hmap"))?;

    // in world coordinates
    let xstart = hmap.xoffset;
    let ystart = hmap.yoffset;
    let size = hmap.scale;

    // in grid coordinates
    let (xmin, ymin) = (0, 0);
    let xmax = (hmap.grid.width() - 1) as u64;
    let ymax = (hmap.grid.height() - 1) as u64;

    let contours_in = tmpfolder.join("contours03.dxf.bin");
    let data = fs
        .open(&contours_in)
        .map_err(anyhow::Error::from)
        .and_then(|mut f| BinaryDxf::from_reader(&mut f))
        .with_context(|| format!("reading {}", contours_in.display()))?;
    let Geometry::Polylines3(lines) = data.take_geometry().swap_remove(0) else {
        anyhow::bail!(
            "contours03.dxf.bin holds no 3D contour lines: it is a stale temp file from another build; re-run the full pipeline"
        );
    };

    let detected_bounds = Bounds::new(xmin as f64, xmax as f64, ymin as f64, ymax as f64);
    let mut detected_lines = Polylines::<Point2, Classification>::new();

    // TODO; might need to lower to 200
    let joined = join_contours(&lines, params.join_max_vertices);
    // TODO: this is not very efficient (collecting all x and y separately into Vecs), but it means the logic further down can stay the same
    let mut el_x: Vec<Vec<f64>> = joined
        .iter()
        .map(|c| c.line.iter().map(|p| p.x).collect())
        .collect();
    let mut el_y: Vec<Vec<f64>> = joined
        .iter()
        .map(|c| c.line.iter().map(|p| p.y).collect())
        .collect();

    let mut elevation: HashMap<u64, f64> = HashMap::default();
    for l in 0..lines.len() {
        let mut skip = false;
        let el_x_len = el_x[l].len();
        if el_x_len > 0 {
            if el_x_len > params.max_ring_vertices {
                skip = true;
                el_x[l].clear();
                el_y[l].clear();
            }
            if el_x_len < params.short_ring_vertices {
                let mut p = 0;
                let mut dist = 0.0;
                while p < el_x_len - 1 {
                    dist += ((el_x[l][p] - el_x[l][p + 1]).powi(2)
                        + (el_y[l][p] - el_y[l][p + 1]).powi(2))
                    .sqrt();
                    p += 1;
                }
                if dist < params.min_short_ring_length_m || el_x_len < params.min_ring_vertices {
                    skip = true;
                    el_x[l].clear();
                    el_y[l].clear();
                }
            }
            if el_x[l].first() != el_x[l].last() || el_y[l].first() != el_y[l].last() {
                skip = true;
                el_x[l].clear();
                el_y[l].clear();
            }
            if !skip
                && el_x_len <= params.max_ring_vertices
                && el_x[l].first() == el_x[l].last()
                && el_y[l].first() == el_y[l].last()
            {
                let tailx = *el_x[l].first().unwrap();
                let mut xl = el_x[l].to_vec();
                xl.push(tailx);
                let taily = *el_y[l].first().unwrap();
                let mut yl = el_y[l].to_vec();
                yl.push(taily);
                let h = joined[l].level_m;
                elevation.insert(l as u64, h);

                let mut mm = ((el_x_len as f64 / 3.0).floor() - 1.0) as i32;
                if mm < 0 {
                    mm = 0;
                }
                let mut m = mm as usize;
                let mut xa = xl[m];
                let mut ya = yl[m];
                while m < xl.len() {
                    let xm = xl[m];
                    let ym = yl[m];
                    let xo = (xm - xstart) / size;
                    let yo = (ym - ystart) / size;
                    if m < xl.len() - 3 && yo == yo.floor() && xo != xo.floor() {
                        xa = xo.floor() * size + xstart;
                        ya = ym.floor();
                        break;
                    }
                    m += 1;
                }
                let h_center = hmap
                    .grid
                    .get((
                        ((xa - xstart) / size).floor() as usize,
                        ((ya - ystart) / size).floor() as usize,
                    ))
                    .copied()
                    .unwrap_or(0.0);
                let xtest = ((xa - xstart) / size).floor() * size + xstart + 0.000000001;
                let ytest = ((ya - ystart) / size).floor() * size + ystart + 0.000000001;
                let inside = Ring::from_xy(&el_x[l], &el_y[l]).contains(Point2::new(xtest, ytest));

                if (h_center < h) && inside || (h_center > h) && !inside {
                    skip = true;
                    el_x[l].clear();
                    el_y[l].clear();
                }
            }
        }
        if skip {
            el_x[l].clear();
            el_y[l].clear();
        }
    }

    struct Head {
        id: u64,
        xtest: f64,
        ytest: f64,
    }
    let mut heads = Vec::<Head>::new();
    for l in 0..lines.len() {
        if !el_x[l].is_empty() {
            if el_x[l].first() == el_x[l].last() && el_y[l].first() == el_y[l].last() {
                heads.push(Head {
                    id: l as u64,
                    xtest: el_x[l][0],
                    ytest: el_y[l][0],
                });
            } else {
                el_x[l].clear();
                el_y[l].clear();
            }
        }
    }
    struct Top {
        id: u64,
        xtest: f64,
        ytest: f64,
    }
    let mut tops = Vec::<Top>::new();
    struct BoundingBox {
        minx: f64,
        maxx: f64,
        miny: f64,
        maxy: f64,
    }
    let mut bb: HashMap<usize, BoundingBox> = HashMap::default();
    for l in 0..lines.len() {
        let mut skip = false;
        if !el_x[l].is_empty() {
            let mut x = el_x[l].to_vec();
            let tailx = *el_x[l].first().unwrap();
            x.push(tailx);

            let mut y = el_y[l].to_vec();
            let taily = *el_y[l].first().unwrap();
            y.push(taily);
            let ring = Ring::from_xy(&x, &y);

            let mut minx = f64::MAX;
            let mut miny = f64::MAX;
            let mut maxx = f64::MIN;
            let mut maxy = f64::MIN;

            for k in 0..x.len() {
                if x[k] > maxx {
                    maxx = x[k]
                }
                if x[k] < minx {
                    minx = x[k]
                }
                if y[k] > maxy {
                    maxy = y[k]
                }
                if y[k] < miny {
                    miny = y[k]
                }
            }
            bb.insert(
                l,
                BoundingBox {
                    minx,
                    maxx,
                    miny,
                    maxy,
                },
            );

            for head in heads.iter() {
                let &Head { id, xtest, ytest } = head;

                if !skip
                    && *elevation.get(&id).unwrap() > *elevation.get(&(l as u64)).unwrap()
                    && id != (l as u64)
                    && xtest < maxx
                    && xtest > minx
                    && ytest < maxy
                    && ytest > miny
                    && ring.contains(Point2::new(xtest, ytest))
                {
                    skip = true;
                }
            }
            if !skip {
                tops.push(Top {
                    id: l as u64,
                    xtest: x[0],
                    ytest: y[0],
                });
            }
        }
    }
    struct Candidate {
        id: u64,
        xtest: f64,
        ytest: f64,
        topid: u64,
    }
    let mut canditates = Vec::<Candidate>::new();

    for l in 0..lines.len() {
        let mut skip = true;
        if !el_x[l].is_empty() {
            let mut x = el_x[l].to_vec();
            let tailx = *el_x[l].first().unwrap();
            x.push(tailx);

            let mut y = el_y[l].to_vec();
            let taily = *el_y[l].first().unwrap();
            y.push(taily);
            let ring = Ring::from_xy(&x, &y);

            let &BoundingBox {
                minx,
                maxx,
                miny,
                maxy,
            } = bb.get(&l).unwrap();

            let mut topid = 0;
            for head in tops.iter() {
                let &Top { id, xtest, ytest } = head;
                let ll = l as u64;

                if *elevation.get(&ll).unwrap()
                    < (*elevation.get(&id).unwrap() - params.min_drop_below_top_m)
                    && *elevation.get(&ll).unwrap()
                        > (*elevation.get(&id).unwrap() - params.max_drop_below_top_m)
                    && skip
                    && xtest < maxx
                    && xtest > minx
                    && ytest < maxy
                    && ytest > miny
                    && ring.contains(Point2::new(xtest, ytest))
                {
                    skip = false;
                    topid = id;
                }
            }
            if !skip {
                canditates.push(Candidate {
                    id: l as u64,
                    xtest: x[0],
                    ytest: y[0],
                    topid,
                });
            } else {
                el_x[l].clear();
                el_y[l].clear();
            }
        }
    }

    let mut best: HashMap<u64, u64> = HashMap::default();
    let mut mov: HashMap<u64, f64> = HashMap::default();

    for head in canditates.iter() {
        let &Candidate { id, topid, .. } = head;
        let el = *elevation.get(&id).unwrap();
        let test = (el / halfinterval + 1.0).floor() * halfinterval - el;

        if !best.contains_key(&topid) {
            best.insert(topid, id);
            mov.insert(id, test);
        } else {
            let tid = *best.get(&topid).unwrap();
            if *mov.get(&tid).unwrap() < params.settled_max_lift * contours_ratio
                && (*elevation.get(&topid).unwrap()
                    - *elevation.get(&tid).unwrap()
                    - params.settled_top_height * contours_ratio)
                    .abs()
                    < params.settled_top_tolerance_m
            {
                // no action
            } else if *mov.get(&tid).unwrap() > test {
                best.insert(topid, id);
                mov.insert(id, test);
            }
        }
    }
    let mut new_candidates = Vec::<Candidate>::new();
    for head in canditates.iter() {
        let &Candidate {
            id,
            xtest,
            ytest,
            topid,
        } = head;

        let x = el_x[id as usize].to_vec();
        if *best.get(&topid).unwrap() == id
            && (x.len() < params.small_ring_vertices
                || (*elevation.get(&topid).unwrap() > (*elevation.get(&id).unwrap() + params.min_top_rise_m)
                    // 2.5 * contours_ratio is the half interval
                    || (*elevation.get(&id).unwrap()
                        - 2.5
                            * contours_ratio
                            * (*elevation.get(&id).unwrap() / (2.5 * contours_ratio)).floor())
                        > params.min_level_offset_m))
        {
            new_candidates.push(Candidate {
                id,
                xtest,
                ytest,
                topid,
            });
        } else {
            el_x[id as usize].clear();
            el_y[id as usize].clear();
        }
    }

    let canditates = new_candidates;

    let mut pins = Vec::new();

    for l in 0..lines.len() {
        let mut skip = false;
        let ll = l as u64;
        let mut ltopid = 0;
        if !el_x[l].is_empty() {
            let mut x = el_x[l].to_vec();
            let tailx = *el_x[l].first().unwrap();
            x.push(tailx);

            let mut y = el_y[l].to_vec();
            let taily = *el_y[l].first().unwrap();
            y.push(taily);
            let ring = Ring::from_xy(&x, &y);

            let &BoundingBox {
                minx,
                maxx,
                miny,
                maxy,
            } = bb.get(&l).unwrap();

            for head in canditates.iter() {
                let &Candidate {
                    id,
                    xtest,
                    ytest,
                    topid,
                } = head;

                ltopid = topid;
                if id != ll
                    && !skip
                    && xtest < maxx
                    && xtest > minx
                    && ytest < maxy
                    && ytest > miny
                    && ring.contains(Point2::new(xtest, ytest))
                {
                    skip = true;
                }
            }

            if !skip {
                let line = x
                    .iter()
                    .zip(y.iter())
                    .map(|(x, y)| Point2::new(*x, *y))
                    .collect::<Vec<_>>();
                detected_lines.push(line, Classification::Knoll1010);

                let mut xa = 0.0;
                let mut ya = 0.0;
                for k in 0..x.len() {
                    xa += x[k];
                    ya += y[k];
                }
                let xlen = x.len() as f64;
                xa /= xlen;
                ya /= xlen;

                x.push(x[0]);
                y.push(y[0]);
                pins.push(Pin {
                    xx: xa,
                    yy: ya,
                    ele: *elevation.get(&ll).unwrap(),
                    ele2: *elevation.get(&ltopid).unwrap(),
                    xlist: x,
                    ylist: y,
                });
            } else {
                el_x[l].clear();
                el_y[l].clear();
            }
        }
    }

    let detected_dxf = BinaryDxf::new(detected_bounds, vec![detected_lines.into()]);
    let detected_out = tmpfolder.join("detected.dxf.bin");
    fs.create(&detected_out)
        .map_err(anyhow::Error::from)
        .and_then(|mut f| detected_dxf.to_writer(&mut f))
        .with_context(|| format!("writing {}", detected_out.display()))?;

    if output_dxf {
        let detected_out = tmpfolder.join("detected.dxf");
        fs.create(&detected_out)
            .map_err(anyhow::Error::from)
            .and_then(|mut f| detected_dxf.to_dxf(&mut f))
            .with_context(|| format!("writing {}", detected_out.display()))?;
    }

    // write pins to file
    let pins_out = tmpfolder.join("pins.bin");
    fs.create(&pins_out)
        .map_err(anyhow::Error::from)
        .and_then(|f| crate::util::write_object(f, &pins))
        .with_context(|| format!("writing {}", pins_out.display()))?;

    info!("Done");
    Ok(())
}

/// Struct used to store temporary data about pins on disk
#[derive(serde::Serialize, serde::Deserialize)]
struct Pin {
    xx: f64,
    yy: f64,
    ele: f64,
    ele2: f64,
    xlist: Vec<f64>,
    ylist: Vec<f64>,
}

pub fn xyzknolls(
    fs: &impl FileSystem,
    params: &KnollParams,
    tmpfolder: &Path,
) -> anyhow::Result<()> {
    info!("Identifying knolls...");
    let scalefactor = params.scalefactor;
    let contour_interval = params.contour_interval;

    let interval = contour_interval / 2.0 * scalefactor;

    // load the binary file
    let hmap = read_heightmap(fs, &tmpfolder.join("xyz_03.hmap"))?;

    let xmax = hmap.grid.width() - 1;
    let ymax = hmap.grid.height() - 1;
    let size = hmap.scale;
    let xstart = hmap.xoffset;
    let ystart = hmap.yoffset;

    let mut xyz2 = hmap.clone();

    let r = params.flatten_radius_cells;
    let flat = params.flatten_max_relief_m;
    for i in r..=(xmax - r) {
        for j in r..=(ymax - r) {
            let mut low = f64::MAX;
            let mut high = f64::MIN;
            let mut val = 0.0;
            let mut count = 0;
            for ii in (i - r)..=(i + r) {
                for jj in (j - r)..=(j + r) {
                    let tmp = hmap.grid[(ii, jj)];
                    if tmp < low {
                        low = tmp;
                    }
                    if tmp > high {
                        high = tmp;
                    }
                    count += 1;
                    val += tmp;
                }
            }
            let steepness = high - low;
            if steepness < flat {
                let tmp = (flat - steepness) * (val - low - high) / (count as f64 - 2.0) / flat
                    + steepness * xyz2.grid[(i, j)] / flat;
                xyz2.grid[(i, j)] = tmp;
            }
        }
    }

    // read pins from file if it exists
    let pins_file_in = tmpfolder.join("pins.bin");
    let pins: Vec<Pin> = if fs.exists(&pins_file_in) {
        fs.open(&pins_file_in)
            .map_err(anyhow::Error::from)
            .and_then(crate::util::read_object)
            .with_context(|| format!("reading {}", pins_file_in.display()))?
    } else {
        Vec::new()
    };

    // compute closest distance from each pin to another pin
    let mut dist: HashMap<usize, f64> = HashMap::default();
    for (l, pin) in pins.iter().enumerate() {
        let mut min = f64::MAX;
        let xx = ((pin.xx - xstart) / size).floor();
        let yy = ((pin.yy - ystart) / size).floor();
        for (k, pin2) in pins.iter().enumerate() {
            if k == l {
                continue;
            }

            let xx2 = ((pin2.xx - xstart) / size).floor();
            let yy2 = ((pin2.yy - ystart) / size).floor();
            let mut dis = (xx2 - xx).abs();
            let disy = (yy2 - yy).abs();
            if disy > dis {
                dis = disy;
            }
            if dis < min {
                min = dis;
            }
        }
        dist.insert(l, min);
    }

    for (l, line) in pins.into_iter().enumerate() {
        let Pin {
            xx,
            yy,
            ele,
            ele2,
            xlist: mut x,
            ylist: mut y,
        } = line;

        let elenew = ((ele - params.level_tolerance_m) / interval + 1.0).floor() * interval;
        let mut move1 = elenew - ele + params.lift_margin_m;
        let mut move2 = move1 * params.surround_share;
        if move1 > params.high_lift_ratio * interval {
            move2 = move1 * params.surround_share_high;
        }
        if move1 < params.low_lift_ratio * interval {
            move2 = 0.0;
            move1 += params.low_lift_extra_m;
        }
        move1 += params.lift_extra_m;
        if ele2 + move1 > ((ele - params.level_tolerance_m) / interval + 2.0).floor() * interval {
            move1 -= params.overshoot_cut_m;
        }
        if elenew - ele > params.shrink_min_lift * interval / 2.5 * scalefactor
            && x.len() > params.shrink_min_vertices
        {
            for k in 0..x.len() {
                x[k] = xx + (x[k] - xx) * params.shrink_factor;
                y[k] = yy + (y[k] - yy) * params.shrink_factor;
            }
        }
        let mut touched: FxHashSet<(usize, usize)> = Default::default();
        let mut minx = u64::MAX;
        let mut miny = u64::MAX;
        let mut maxx = u64::MIN;
        let mut maxy = u64::MIN;
        for k in 0..x.len() {
            x[k] = ((x[k] - xstart) / size + 0.5).floor();
            y[k] = ((y[k] - ystart) / size + 0.5).floor();
            let xk = x[k] as u64;
            let yk = y[k] as u64;
            if xk > maxx {
                maxx = xk;
            }
            if yk > maxy {
                maxy = yk;
            }
            if xk < minx {
                minx = xk;
            }
            if yk < miny {
                miny = yk;
            }
        }

        let xx = ((xx - xstart) / size).floor();
        let yy = ((yy - ystart) / size).floor();

        let ring = Ring::from_xy(&x, &y);
        for ii in minx as usize..(maxx as usize + 1) {
            for jj in miny as usize..(maxy as usize + 1) {
                if ring.contains(Point2::new(ii as f64, jj as f64)) {
                    xyz2.grid[(ii, jj)] += move1;
                    touched.insert((ii, jj));
                }
            }
        }
        let mut range = *dist.get(&l).unwrap_or(&0.0) * params.surround_range_share
            - params.surround_range_trim_cells;
        range = range.clamp(
            params.min_surround_range_cells,
            params.max_surround_range_cells,
        );
        smooth_around_pin(&mut xyz2.grid, &touched, (xx, yy), range, move2);
    }

    // As per https://github.com/karttapullautin/karttapullautin/discussions/154#discussioncomment-11393907
    // If elevation grid point elavion equals with contour interval steps you will get contour topology issues
    // (crossing/touching contours). This was implemented to avoid that. 0.02 (two centimeters) is just a random
    // small number to avoid that issue, insignificant enough to matter, but big buffer enough to hopefully make
    // it not get back to "bad value" for it getting rounded somewhere. Sure, it could be some fraction of
    // contour interval, but in real world 2 cm is insignificant enough.
    // Here the 2 cm is `level_clearance_m`; the tracer in contours.rs keeps its own nudge.
    for (_, _, h) in xyz2.grid.iter_mut() {
        let tmp = (*h / interval + 0.5).floor() * interval;
        if (tmp - *h).abs() < params.level_clearance_m {
            if *h - tmp < 0.0 {
                *h = tmp - params.level_clearance_m;
            } else {
                *h = tmp + params.level_clearance_m;
            }
        }
    }

    // write the updated heightmap
    let heightmap_out = tmpfolder.join("xyz_knolls.hmap");
    fs.create(&heightmap_out)
        .and_then(|mut f| xyz2.to_bytes(&mut f))
        .with_context(|| format!("writing {}", heightmap_out.display()))?;

    info!("Done");
    Ok(())
}

/// Read a heightmap temp file, naming the file in the error.
fn read_heightmap(fs: &impl FileSystem, path: &Path) -> anyhow::Result<HeightMap> {
    fs.open(path)
        .and_then(|mut f| HeightMap::from_bytes(&mut f))
        .with_context(|| format!("reading {}", path.display()))
}

/// Knoll-lift smoothing around one pin: adds `move2` to the (2 * range + 1)² cells centred on
/// `centre` (cell coordinates), tapering linearly to zero at `range`. Cells the lift already
/// raised (`touched`) and the grid border are left alone. `range` may be fractional, so the
/// visited coordinates may be too; each one writes the cell it truncates to.
fn smooth_around_pin(
    grid: &mut Vec2D<f64>,
    touched: &FxHashSet<(usize, usize)>,
    centre: (f64, f64),
    range: f64,
    move2: f64,
) {
    let (xx, yy) = centre;
    let xmax = (grid.width() - 1) as f64;
    let ymax = (grid.height() - 1) as f64;
    let steps = (range * 2.0 + 1.0) as usize;
    for iii in 0..steps {
        for jjj in 0..steps {
            let ii = xx - range + iii as f64;
            let jj = yy - range + jjj as f64;
            if ii > 0.0 && ii < xmax && jj > 0.0 && jj < ymax {
                let cell = (ii as usize, jj as usize);
                if !touched.contains(&cell) {
                    grid[cell] += (range - (xx - ii).abs()) / range * (range - (yy - jj).abs())
                        / range
                        * move2;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::fs::memory::MemoryFileSystem;

    /// The defaults are the original (Perl-port) constants.
    #[test]
    fn knoll_params_default_to_the_perl_constants() {
        let p = KnollParams::default();
        assert_eq!((p.scalefactor, p.contour_interval), (1.0, 5.0));
        assert_eq!(p.join_max_vertices, 201);
        assert_eq!((p.short_ring_vertices, p.max_ring_vertices), (9, 121));
        assert_eq!((p.min_ring_vertices, p.min_short_ring_length_m), (3, 5.0));
        assert_eq!((p.min_drop_below_top_m, p.max_drop_below_top_m), (0.1, 4.6));
        assert_eq!(
            (
                p.settled_max_lift,
                p.settled_top_height,
                p.settled_top_tolerance_m
            ),
            (1.75, 0.6, 0.2)
        );
        assert_eq!(p.small_ring_vertices, 13);
        assert_eq!((p.min_top_rise_m, p.min_level_offset_m), (0.45, 0.45));
        assert_eq!((p.flatten_radius_cells, p.flatten_max_relief_m), (2, 1.25));
        assert_eq!((p.level_tolerance_m, p.lift_margin_m), (0.09, 0.15));
        assert_eq!((p.surround_share, p.surround_share_high), (0.4, 0.6));
        assert_eq!((p.high_lift_ratio, p.low_lift_ratio), (0.66, 0.25));
        assert_eq!((p.low_lift_extra_m, p.lift_extra_m), (0.3, 0.5));
        assert_eq!(p.overshoot_cut_m, 0.4);
        assert_eq!(
            (p.shrink_min_lift, p.shrink_min_vertices, p.shrink_factor),
            (1.5, 21, 0.8)
        );
        assert_eq!(
            (p.surround_range_share, p.surround_range_trim_cells),
            (0.8, 1.0)
        );
        assert_eq!(
            (p.min_surround_range_cells, p.max_surround_range_cells),
            (1.0, 12.0)
        );
        assert_eq!((p.level_clearance_m, p.dot_clearance_px), (0.02, 3.0));
    }

    /// Run `knolldetector` on a 2 m ground model of `f(cell x, cell y)`, contoured at
    /// 0.3 m as the pipeline does, and return its pins.
    fn detect(f: impl Fn(f64, f64) -> f64) -> Vec<Pin> {
        let fs = MemoryFileSystem::new();
        let tmp = Path::new("tmp");
        fs.create_dir_all(tmp).unwrap();
        let (w, h) = (41, 41);
        let mut grid = Vec2D::new(w, h, 0.0);
        for i in 0..w {
            for j in 0..h {
                grid[(i, j)] = f(i as f64, j as f64);
            }
        }
        let hmap = HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 2.0,
            grid,
        };
        hmap.to_file(&fs, tmp.join("xyz_03.hmap")).unwrap();
        crate::contours::heightmap2contours(&fs, tmp, 0.3, &hmap, "contours03.dxf.bin", false)
            .unwrap();
        knolldetector(&fs, &KnollParams::default(), false, tmp).unwrap();
        crate::util::read_object(fs.open(tmp.join("pins.bin")).unwrap()).unwrap()
    }

    /// A 1 m cone (radius 5 cells) on flat ground at 100 m is one knoll: one pin, at the
    /// apex (cell 20, 20 = 1040 m, 2040 m), its ring below the top ring.
    #[test]
    fn knolldetector_finds_one_knoll_on_a_cone() {
        let cone = |x: f64, y: f64| {
            100.0 + (1.0 - ((x - 20.0).powi(2) + (y - 20.0).powi(2)).sqrt() / 5.0).max(0.0)
        };
        let pins = detect(cone);
        assert_eq!(pins.len(), 1);
        let pin = &pins[0];
        assert!((pin.xx - 1040.0).abs() < 2.0 && (pin.yy - 2040.0).abs() < 2.0);
        assert!(pin.ele < pin.ele2 && pin.ele2 < 101.0 && pin.ele > 100.0);
    }

    /// The same cone upside down is a depression: no knoll.
    #[test]
    fn knolldetector_finds_no_knoll_in_a_pit() {
        let pit = |x: f64, y: f64| {
            100.0 - (1.0 - ((x - 20.0).powi(2) + (y - 20.0).powi(2)).sqrt() / 5.0).max(0.0)
        };
        assert!(detect(pit).is_empty());
    }

    /// Range 1.5 visits x, y in {3.5, 4.5, 5.5, 6.5} around (5, 5): fractional coordinates.
    /// (5.5, 5.5) truncates to the lifted cell (5, 5), which must be skipped; the neighbour
    /// (4.5, 5.5) writes (4, 5) with weight (1 / 1.5)².
    #[test]
    fn smooth_around_pin_skips_lifted_cell_at_fractional_coordinates() {
        let mut grid = Vec2D::new(12, 12, 0.0);
        let touched: FxHashSet<(usize, usize)> = [(5, 5)].into_iter().collect();
        smooth_around_pin(&mut grid, &touched, (5.0, 5.0), 1.5, 1.0);
        assert_eq!(grid[(5, 5)], 0.0);
        assert!((grid[(4, 5)] - 4.0 / 9.0).abs() < 1e-12);
    }
}
