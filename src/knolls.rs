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
    BinaryDxf, Bounds, Classification, Contour, Geometry, Point2, Points, Polylines, Ring,
};
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
/// ground distances; the map scale does not scale them. `settled_max_lift`,
/// `settled_top_height`, `high_lift_ratio`, `low_lift_ratio` and `shrink_min_lift` are
/// shares of the level spacing, the trace interval.
#[derive(Debug, Clone, PartialEq)]
pub struct KnollParams {
    /// Trace interval in metres: the contour interval, or half of it with form lines
    /// (ini `contour_interval` and `form_lines`). The knoll levels step by it, and the
    /// candidate thresholds tuned at 2.5 m scale with it.
    pub trace_interval: f64,
    /// Interval of the fine contours knolldetector picks its candidates from
    /// (`contours03.dxf.bin`). Metres.
    pub candidate_interval_m: f64,

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
    /// level is under this times the trace interval / 2.5 m…
    pub settled_max_lift: f64,
    /// … and the top stands this times the trace interval / 2.5 m above it…
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
    /// A knoll whose lift exceeds this many 2.5ths of the level spacing and has more than
    /// `shrink_min_vertices` vertices is shrunk first.
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
    /// Pixel size of the image the contours are drawn into to test dot knoll clearance.
    /// Metres.
    pub dot_pixel_m: f64,
    /// Half-width in those pixels of the square around a dot knoll that must hold no
    /// contour, else the dot knoll is ugly.
    pub dot_clearance_px: f64,
}

impl Default for KnollParams {
    fn default() -> Self {
        Self {
            trace_interval: 2.5,
            candidate_interval_m: 0.3,
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
            dot_pixel_m: 1.0,
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

/// Sorts smoothjoin's dot knolls (`dotknolls.bin`) into clean and ugly ones by their
/// clearance from the contours in `out2.dxf.bin`, on the frame of `lifted`, the lifted
/// ground model; writes `dotknolls.dxf.bin`.
pub fn dotknolls(
    fs: &impl FileSystem,
    params: &KnollParams,
    output_dxf: bool,
    tmpfolder: &Path,
    lifted: &HeightMap,
) -> Result<(), Box<dyn Error>> {
    info!("Identifying dotknolls...");

    let pixel = params.dot_pixel_m;
    let clearance = params.dot_clearance_px;

    // in world coordinates
    let xstart = lifted.xoffset;
    let ystart = lifted.yoffset;

    // in grid coordinates
    let xmax = (lifted.grid.width() - 1) as f64;
    let ymax = (lifted.grid.height() - 1) as f64;
    let size = lifted.scale;

    let mut im = GrayImage::from_pixel(
        (xmax * size / pixel) as u32,
        (ymax * size / pixel) as u32,
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
                    ((line[i - 1].x - xstart) / pixel).floor() as f32,
                    ((line[i - 1].y - ystart) / pixel).floor() as f32,
                ),
                (
                    ((line[i].x - xstart) / pixel).floor() as f32,
                    ((line[i].y - ystart) / pixel).floor() as f32,
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
        let mut i = (x - xstart) / pixel - clearance;
        while i < (x - xstart) / pixel + (clearance + 1.0) && ok {
            if (i as u32) >= im.width() {
                ok = false;
                break;
            }
            let mut j = (y - ystart) / pixel - clearance;
            while j < (y - ystart) / pixel + (clearance + 1.0) && ok {
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

/// The ground model's debug intermediate that the knoll stage commands read
/// (`knolldetector`, `xyzknolls`); `xyz2.hmap` ([`crate::render::GROUND_DUMP`]) holds
/// the same bytes.
pub const KNOLL_GROUND_DUMP: &str = "xyz_03.hmap";

/// The debug intermediate of the fine contours knolldetector picks its candidates from.
pub const CANDIDATES_DUMP: &str = "contours03.dxf.bin";
/// The debug intermediate of the knoll rings knolldetector found ([`DetectedKnolls`]).
pub const DETECTED_DUMP: &str = "detected.dxf.bin";
/// The debug intermediate of the knoll pins knolldetector found ([`Pin`]).
pub const PINS_DUMP: &str = "pins.bin";
/// The debug intermediate of the lifted ground model ([`xyzknolls`]).
pub const LIFTED_GROUND_DUMP: &str = "xyz_knolls.hmap";

/// Detects knolls on `ground` from `contours`, the fine contours traced every
/// `params.candidate_interval_m` ([`trace`](crate::contours::trace)). Returns the
/// detected knoll rings and one [`Pin`] per knoll, for [`xyzknolls`].
pub fn knolldetector(
    ground: &HeightMap,
    contours: &[Contour],
    params: &KnollParams,
) -> (DetectedKnolls, Vec<Pin>) {
    info!("Detecting knolls...");
    let halfinterval = params.trace_interval;

    // the thresholds were tuned at a 2.5 m trace interval; this scales them to the map's
    let contours_ratio = params.trace_interval / 2.5;

    // in world coordinates
    let xstart = ground.xoffset;
    let ystart = ground.yoffset;
    let size = ground.scale;

    // in grid coordinates
    let (xmin, ymin) = (0, 0);
    let xmax = (ground.grid.width() - 1) as u64;
    let ymax = (ground.grid.height() - 1) as u64;

    let detected_bounds = Bounds::new(xmin as f64, xmax as f64, ymin as f64, ymax as f64);
    let mut detected_lines = Polylines::<Point2, Classification>::new();

    // TODO; might need to lower to 200
    let joined = join_contours(contours, params.join_max_vertices);
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
    for l in 0..contours.len() {
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
                let h_center = ground
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
    for l in 0..contours.len() {
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
    for l in 0..contours.len() {
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

    for l in 0..contours.len() {
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

    for l in 0..contours.len() {
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

    info!("Done");
    (
        DetectedKnolls {
            lines: detected_lines,
            bounds: detected_bounds,
        },
        pins,
    )
}

/// Write knolldetector's debug intermediates to `tmpfolder`: `detected` to [`DETECTED_DUMP`]
/// (and as text DXF with `output_dxf`) and `pins` to [`PINS_DUMP`].
pub fn write_detected(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    detected: &DetectedKnolls,
    pins: &[Pin],
    output_dxf: bool,
) -> Result<(), Box<dyn Error>> {
    crate::contours::write_bindxf(
        fs,
        tmpfolder,
        DETECTED_DUMP,
        &detected.to_bindxf(),
        output_dxf,
    )?;
    let pins_out = tmpfolder.join(PINS_DUMP);
    fs.create(&pins_out)
        .map_err(anyhow::Error::from)
        .and_then(|f| crate::util::write_object(f, &pins))
        .with_context(|| format!("writing {}", pins_out.display()))?;
    Ok(())
}

/// The knoll rings [`knolldetector`] found, for the `detected.dxf.bin` debug intermediate.
#[derive(Debug, Clone)]
pub struct DetectedKnolls {
    /// Each knoll's ring, in world coordinates, classed [`Classification::Knoll1010`].
    pub lines: Polylines<Point2, Classification>,
    /// The ground model's extent in grid cells (0 to width - 1, 0 to height - 1), not in
    /// world coordinates as the lines are: kept as the original wrote it.
    pub bounds: Bounds,
}

impl DetectedKnolls {
    /// The `detected.dxf.bin` debug intermediate.
    pub fn to_bindxf(&self) -> BinaryDxf {
        BinaryDxf::new(self.bounds.clone(), vec![self.lines.clone().into()])
    }
}

/// A knoll pin: one knoll that knoll detection kept, which [`xyzknolls`] lifts the
/// ground model under. The `pins.bin` debug intermediate holds a `Vec<Pin>`.
///
/// The fields keep three quirks of the original, which the knoll lift depends on: the
/// centre counts the first vertex more than once, `ele2` is not this knoll's own top,
/// and the ring ends with its first vertex repeated more than once.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pin {
    /// The x of the knoll's centre, in world coordinates: the mean x of the closed ring
    /// with its first vertex appended once more, so the first vertex counts three times.
    pub xx: f64,
    /// The y of the knoll's centre, the same mean as `xx` over the y coordinates.
    pub yy: f64,
    /// The level of the knoll's ring, in metres.
    pub ele: f64,
    /// The level of the top of the last knoll candidate knolldetector looked at, in
    /// metres, not of this knoll's own top: the loop that tests the ring against every
    /// candidate assigns the top of each in turn and keeps the last.
    pub ele2: f64,
    /// The x of the ring's vertices, in world coordinates: the closed ring (first vertex
    /// repeated last) with its first vertex appended twice more.
    pub xlist: Vec<f64>,
    /// The y of the ring's vertices, in the same order and with the same repeats as
    /// `xlist`.
    pub ylist: Vec<f64>,
}

/// Flattens a copy of `ground` and lifts it under `pins` (the knoll lift). Returns the
/// lifted ground model, which smoothjoin and dotknolls read. With no pins the copy is
/// only flattened.
pub fn xyzknolls(ground: &HeightMap, pins: &[Pin], params: &KnollParams) -> HeightMap {
    info!("Identifying knolls...");
    let interval = params.trace_interval;

    let xmax = ground.grid.width() - 1;
    let ymax = ground.grid.height() - 1;
    let size = ground.scale;
    let xstart = ground.xoffset;
    let ystart = ground.yoffset;

    let mut xyz2 = ground.clone();

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
                    let tmp = ground.grid[(ii, jj)];
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

    for (l, line) in pins.iter().cloned().enumerate() {
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
        if elenew - ele > params.shrink_min_lift * interval / 2.5
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

    info!("Done");
    xyz2
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

    /// The defaults are the original (Perl-port) constants.
    #[test]
    fn knoll_params_default_to_the_perl_constants() {
        let p = KnollParams::default();
        assert_eq!((p.trace_interval, p.candidate_interval_m), (2.5, 0.3));
        assert_eq!(p.dot_pixel_m, 1.0);
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

    /// A 41 x 41 cell, 2 m ground model of `f(cell x, cell y)` at (1000 m, 2000 m).
    fn ground(f: impl Fn(f64, f64) -> f64) -> HeightMap {
        let (w, h) = (41, 41);
        let mut grid = Vec2D::new(w, h, 0.0);
        for i in 0..w {
            for j in 0..h {
                grid[(i, j)] = f(i as f64, j as f64);
            }
        }
        HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 2.0,
            grid,
        }
    }

    /// Run `knolldetector` on `ground(f)`, contoured at 0.3 m as the pipeline does, and
    /// return its pins.
    fn detect(f: impl Fn(f64, f64) -> f64) -> Vec<Pin> {
        let hmap = ground(f);
        let candidates = crate::contours::trace(&hmap, 0.3);
        knolldetector(&hmap, &candidates, &KnollParams::default()).1
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

    /// With no pins `xyzknolls` only flattens: steep ground (3 m a cell) is left as it
    /// is, and gently rolling ground is smoothed away from the border.
    #[test]
    fn xyzknolls_without_pins_only_flattens() {
        let params = KnollParams::default();
        let steep = ground(|x, _| 100.3 + 3.0 * x);
        assert_eq!(xyzknolls(&steep, &[], &params).grid, steep.grid);

        let rolling = ground(|x, y| 100.3 + if (x + y) % 2.0 == 0.0 { 0.1 } else { -0.1 });
        let flat = xyzknolls(&rolling, &[], &params);
        for (i, j, z) in flat.grid.iter() {
            let border = i < 2 || j < 2 || i > 38 || j > 38;
            if border {
                assert_eq!(z, rolling.grid[(i, j)]);
            } else {
                assert!((z - 100.3).abs() < 0.05, "cell ({i}, {j}) at {z}");
            }
        }
    }

    /// A pin just below the next knoll level (102.2 m, levels every 2.5 m) gets a small
    /// lift with no surround: the cells inside its ring rise by 1.25 m and no other
    /// cell changes.
    ///
    /// The lift with the default params: the next level is 102.5 m, so it starts at
    /// 0.3 m; + 0.15 m margin = 0.45 m. That is below the low-lift ratio (0.25 x 2.5 m =
    /// 0.625 m), so the surround lift is 0 and 0.3 m low-lift extra is added; + 0.5 m
    /// lift extra = 1.25 m. 102.4 m + 1.25 m stays below 105 m, so there is no overshoot
    /// cut. Cells outside the ring are unchanged only because the surround lift is 0.
    #[test]
    fn xyzknolls_lifts_only_the_cells_inside_the_pin_ring() {
        let params = KnollParams::default();
        let flat = ground(|_, _| 102.2);
        // a square ring around cell (20, 20), cells 16 to 24
        let (xlist, ylist) = [
            (1032.0, 2032.0),
            (1048.0, 2032.0),
            (1048.0, 2048.0),
            (1032.0, 2048.0),
        ]
        .iter()
        .chain(&[(1032.0, 2032.0)])
        .copied()
        .unzip();
        let pin = Pin {
            xx: 1040.0,
            yy: 2040.0,
            ele: 102.2,
            ele2: 102.4,
            xlist,
            ylist,
        };
        let unlifted = xyzknolls(&flat, &[], &params);
        let lifted = xyzknolls(&flat, &[pin], &params);
        for (i, j, z) in lifted.grid.iter() {
            let rise = z - unlifted.grid[(i, j)];
            if (17..=23).contains(&i) && (17..=23).contains(&j) {
                assert!((rise - 1.25).abs() < 1e-9, "cell ({i}, {j}) rose {rise}");
            } else if !(16..=24).contains(&i) || !(16..=24).contains(&j) {
                assert_eq!(rise, 0.0, "cell ({i}, {j}) outside the ring");
            }
        }
    }

    /// The pins of a cone lift its apex: the knoll stage end to end, without files.
    #[test]
    fn the_cone_pin_lifts_the_apex() {
        let cone = |x: f64, y: f64| {
            100.0 + (1.0 - ((x - 20.0).powi(2) + (y - 20.0).powi(2)).sqrt() / 5.0).max(0.0)
        };
        let params = KnollParams::default();
        let hmap = ground(cone);
        let pins = detect(cone);
        let lifted = xyzknolls(&hmap, &pins, &params);
        let unlifted = xyzknolls(&hmap, &[], &params);
        assert!(lifted.grid[(20, 20)] > unlifted.grid[(20, 20)] + 1.0);
        assert_eq!(lifted.grid[(1, 1)], unlifted.grid[(1, 1)]);
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
