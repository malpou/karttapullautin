//! Vector metrics over GeoJSON FeatureCollections, grouped by symbol code.
//!
//! A feature's symbol code is its `symbol` property, falling back to `isom`
//! and then `layer` (the names earlier GeoJSON writers used).
//! Per code the module reports totals for each side (features, point count,
//! line length, polygon area, line crossings) and, when both sides hold that
//! geometry kind, how well the candidate agrees with the baseline:
//!
//! - Lines: length-weighted precision and recall within a tolerance, the
//!   symmetric Hausdorff distance, and the mean distance in each direction.
//! - Polygons: the same line agreement over their rings, so a moved or
//!   reshaped polygon shows even when its area is unchanged.
//! - Points: the share of points with a counterpart within the tolerance, and
//!   the symmetric Hausdorff distance.
//!
//! Line distances are measured at sample points no more than half a tolerance
//! apart (plus every vertex for the Hausdorff maximum), so they are accurate to
//! a quarter of the tolerance. Every measure is rounded to six decimals.
//! Coordinates are taken to be projected metres, as the pipeline writes them;
//! lengths and areas of WGS84 (RFC 7946) GeoJSON would be in degrees.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::round6;

type Pt = [f64; 2];

/// The code used when a feature has none of `symbol`, `isom` or `layer`.
const UNKNOWN_CODE: &str = "unknown";

/// All geometry of one symbol code in one file.
#[derive(Debug, Default)]
pub struct CodeGeometry {
    features: usize,
    points: Vec<Pt>,
    lines: Vec<Vec<Pt>>,
    /// Polygon rings, outer and holes alike.
    rings: Vec<Vec<Pt>>,
    polygons: usize,
    area_m2: f64,
}

/// Totals for one symbol code on one side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub struct CodeStats {
    pub features: usize,
    pub points: usize,
    pub lines: usize,
    pub length_m: f64,
    pub polygons: usize,
    pub area_m2: f64,
    /// Proper crossings between any two line segments of this code.
    pub crossings: usize,
}

/// How closely the candidate's lines follow the baseline's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LineAgreement {
    /// Share of candidate length within the tolerance of a baseline line.
    pub precision: f64,
    /// Share of baseline length within the tolerance of a candidate line.
    pub recall: f64,
    pub hausdorff_m: f64,
    /// Length-weighted mean distance from candidate to baseline.
    pub mean_distance_m: f64,
    /// Length-weighted mean distance from baseline to candidate.
    pub mean_distance_back_m: f64,
}

/// How many points find a counterpart within the tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PointAgreement {
    /// Share of candidate points near a baseline point.
    pub precision: f64,
    /// Share of baseline points near a candidate point.
    pub recall: f64,
    pub hausdorff_m: f64,
}

/// One symbol code compared across the two sides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodeComparison {
    pub baseline: CodeStats,
    pub candidate: CodeStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineAgreement>,
    /// Line agreement over polygon rings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<LineAgreement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<PointAgreement>,
}

impl CodeComparison {
    /// True unless both sides have the same totals and the same geometry
    /// (to the rounding of the report).
    pub fn has_change(&self) -> bool {
        self.baseline != self.candidate
            || self.lines.is_some_and(|l| l.hausdorff_m > 0.0)
            || self.boundaries.is_some_and(|l| l.hausdorff_m > 0.0)
            || self.points.is_some_and(|p| p.hausdorff_m > 0.0)
    }
}

/// Read and parse a JSON file.
pub(crate) fn read_json(path: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Read a GeoJSON FeatureCollection and group its geometry by symbol code.
pub fn load(path: &Path) -> anyhow::Result<BTreeMap<String, CodeGeometry>> {
    Ok(group_by_code(&read_json(path)?))
}

/// Group the features of a parsed FeatureCollection by symbol code.
pub fn group_by_code(collection: &Value) -> BTreeMap<String, CodeGeometry> {
    let mut codes: BTreeMap<String, CodeGeometry> = BTreeMap::new();
    let features = collection["features"].as_array().map_or(&[][..], |f| f);
    for feature in features {
        let props = &feature["properties"];
        let code = code_of(&props["symbol"])
            .or_else(|| code_of(&props["isom"]))
            .or_else(|| code_of(&props["layer"]))
            .unwrap_or_else(|| UNKNOWN_CODE.to_string());
        let entry = codes.entry(code).or_default();
        entry.features += 1;
        add_geometry(entry, &feature["geometry"]);
    }
    codes
}

fn code_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn point(v: &Value) -> Option<Pt> {
    Some([v.get(0)?.as_f64()?, v.get(1)?.as_f64()?])
}

fn line(v: &Value) -> Vec<Pt> {
    v.as_array()
        .map(|a| a.iter().filter_map(point).collect())
        .unwrap_or_default()
}

fn add_polygon(entry: &mut CodeGeometry, rings: &Value) {
    let Some(rings) = rings.as_array() else {
        return;
    };
    entry.polygons += 1;
    for (i, ring) in rings.iter().enumerate() {
        let ring = line(ring);
        let a = ring_area(&ring);
        entry.rings.push(ring);
        // the first ring is the outer boundary, the rest are holes
        entry.area_m2 += if i == 0 { a } else { -a };
    }
}

fn add_geometry(entry: &mut CodeGeometry, geometry: &Value) {
    let coords = &geometry["coordinates"];
    let list = || coords.as_array().map_or(&[][..], |a| a);
    match geometry["type"].as_str() {
        Some("Point") => entry.points.extend(point(coords)),
        Some("MultiPoint") => entry.points.extend(list().iter().filter_map(point)),
        Some("LineString") => entry.lines.push(line(coords)),
        Some("MultiLineString") => entry.lines.extend(list().iter().map(line)),
        Some("Polygon") => add_polygon(entry, coords),
        Some("MultiPolygon") => list().iter().for_each(|p| add_polygon(entry, p)),
        Some("GeometryCollection") => {
            if let Some(gs) = geometry["geometries"].as_array() {
                gs.iter().for_each(|g| add_geometry(entry, g));
            }
        }
        _ => {}
    }
}

/// Unsigned shoelace area of a ring (closed or not).
fn ring_area(ring: &[Pt]) -> f64 {
    let n = ring.len();
    let twice: f64 = (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    twice.abs() / 2.0
}

fn dist(a: Pt, b: Pt) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn seg_dist(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    dist(p, [a[0] + t * dx, a[1] + t * dy])
}

fn segments(lines: &[Vec<Pt>]) -> Vec<(Pt, Pt)> {
    lines
        .iter()
        .flat_map(|l| l.windows(2).map(|w| (w[0], w[1])))
        .collect()
}

fn length(lines: &[Vec<Pt>]) -> f64 {
    segments(lines).iter().map(|&(a, b)| dist(a, b)).sum()
}

/// Bounding box of every coordinate in `sets`, as `[minx, miny, maxx, maxy]`.
fn bounds<'a>(sets: impl IntoIterator<Item = &'a [(Pt, Pt)]>) -> [f64; 4] {
    let mut bb = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for &(a, b) in sets.into_iter().flatten() {
        for p in [a, b] {
            bb = [
                bb[0].min(p[0]),
                bb[1].min(p[1]),
                bb[2].max(p[0]),
                bb[3].max(p[1]),
            ];
        }
    }
    bb
}

/// A uniform grid over segments for nearest-distance and overlap queries.
/// Points are stored as zero-length segments.
struct SegmentGrid<'a> {
    segs: &'a [(Pt, Pt)],
    origin: Pt,
    cell: f64,
    nx: usize,
    ny: usize,
    cells: Vec<Vec<u32>>,
}

impl<'a> SegmentGrid<'a> {
    /// `bb` must cover every point the grid will be queried with.
    fn new(segs: &'a [(Pt, Pt)], bb: [f64; 4], min_cell: f64) -> Self {
        let (w, h) = ((bb[2] - bb[0]).max(0.0), (bb[3] - bb[1]).max(0.0));
        // about one segment per cell (at most n cells along a thin box),
        // never finer than min_cell
        let n = segs.len().max(1) as f64;
        let cell = (w * h / n).sqrt().max(w.max(h) / n).max(min_cell);
        let cell = if cell > 0.0 { cell } else { 1.0 };
        let nx = (w / cell) as usize + 1;
        let ny = (h / cell) as usize + 1;
        let mut grid = Self {
            segs,
            origin: [bb[0], bb[1]],
            cell,
            nx,
            ny,
            cells: vec![Vec::new(); nx * ny],
        };
        for (i, &(a, b)) in segs.iter().enumerate() {
            let (x0, y0) = grid.cell_of([a[0].min(b[0]), a[1].min(b[1])]);
            let (x1, y1) = grid.cell_of([a[0].max(b[0]), a[1].max(b[1])]);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    grid.cells[y * nx + x].push(i as u32);
                }
            }
        }
        grid
    }

    fn cell_of(&self, p: Pt) -> (usize, usize) {
        let x = ((p[0] - self.origin[0]) / self.cell).max(0.0) as usize;
        let y = ((p[1] - self.origin[1]) / self.cell).max(0.0) as usize;
        (x.min(self.nx - 1), y.min(self.ny - 1))
    }

    /// Distance from `p` to the nearest segment, searching outward ring by ring.
    fn nearest(&self, p: Pt) -> f64 {
        let (cx, cy) = self.cell_of(p);
        let max_ring = cx.max(self.nx - 1 - cx).max(cy).max(self.ny - 1 - cy);
        let mut best = f64::INFINITY;
        for r in 0..=max_ring {
            let (x0, x1) = (cx.saturating_sub(r), (cx + r).min(self.nx - 1));
            let (y0, y1) = (cy.saturating_sub(r), (cy + r).min(self.ny - 1));
            for y in y0..=y1 {
                for x in x0..=x1 {
                    // only the ring's outline; inner cells were visited already
                    if x.abs_diff(cx) != r && y.abs_diff(cy) != r {
                        continue;
                    }
                    for &i in &self.cells[y * self.nx + x] {
                        let (a, b) = self.segs[i as usize];
                        best = best.min(seg_dist(p, a, b));
                    }
                }
            }
            // every cell beyond ring r is at least r cells away from p
            if best <= r as f64 * self.cell {
                break;
            }
        }
        best
    }

    /// Indices of segments whose cells overlap the box of segment `(a, b)`.
    fn near_box(&self, a: Pt, b: Pt) -> Vec<u32> {
        let (x0, y0) = self.cell_of([a[0].min(b[0]), a[1].min(b[1])]);
        let (x1, y1) = self.cell_of([a[0].max(b[0]), a[1].max(b[1])]);
        let mut out: Vec<u32> = (y0..=y1)
            .flat_map(|y| (x0..=x1).map(move |x| (x, y)))
            .flat_map(|(x, y)| self.cells[y * self.nx + x].iter().copied())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Distances from one line set to a grid of the other, weighted by length.
struct Directed {
    total: f64,
    matched: f64,
    weighted_distance: f64,
    max: f64,
}

fn directed(from: &[(Pt, Pt)], to: &SegmentGrid, tolerance: f64) -> Directed {
    let step = tolerance / 2.0;
    let mut d = Directed {
        total: 0.0,
        matched: 0.0,
        weighted_distance: 0.0,
        max: 0.0,
    };
    for &(a, b) in from {
        let len = dist(a, b);
        let pieces = (len / step).ceil().max(1.0);
        let w = len / pieces;
        for k in 0..pieces as usize {
            let t = (k as f64 + 0.5) / pieces;
            let near = to.nearest([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]);
            d.total += w;
            d.weighted_distance += w * near;
            d.max = d.max.max(near);
            if near <= tolerance {
                d.matched += w;
            }
        }
        d.max = d.max.max(to.nearest(a)).max(to.nearest(b));
    }
    d
}

/// Line agreement between two segment sets; `None` when either has no length.
fn line_agreement(
    baseline: &[(Pt, Pt)],
    candidate: &[(Pt, Pt)],
    tolerance: f64,
) -> Option<LineAgreement> {
    if baseline.is_empty() || candidate.is_empty() {
        return None;
    }
    let bb = bounds([baseline, candidate]);
    let base_grid = SegmentGrid::new(baseline, bb, tolerance);
    let cand_grid = SegmentGrid::new(candidate, bb, tolerance);
    let forward = directed(candidate, &base_grid, tolerance);
    let back = directed(baseline, &cand_grid, tolerance);
    if forward.total == 0.0 || back.total == 0.0 {
        return None;
    }
    Some(LineAgreement {
        precision: round6(forward.matched / forward.total),
        recall: round6(back.matched / back.total),
        hausdorff_m: round6(forward.max.max(back.max)),
        mean_distance_m: round6(forward.weighted_distance / forward.total),
        mean_distance_back_m: round6(back.weighted_distance / back.total),
    })
}

fn point_agreement(baseline: &[Pt], candidate: &[Pt], tolerance: f64) -> Option<PointAgreement> {
    if baseline.is_empty() || candidate.is_empty() {
        return None;
    }
    let as_segs = |pts: &[Pt]| pts.iter().map(|&p| (p, p)).collect::<Vec<_>>();
    let (b, c) = (as_segs(baseline), as_segs(candidate));
    let bb = bounds([&b[..], &c[..]]);
    let (bg, cg) = (
        SegmentGrid::new(&b, bb, tolerance),
        SegmentGrid::new(&c, bb, tolerance),
    );
    // (share within tolerance, largest distance)
    let directed = |pts: &[Pt], grid: &SegmentGrid| {
        let d: Vec<f64> = pts.iter().map(|&p| grid.nearest(p)).collect();
        let near = d.iter().filter(|&&d| d <= tolerance).count();
        (
            near as f64 / pts.len() as f64,
            d.into_iter().fold(0.0, f64::max),
        )
    };
    let (precision, forward_max) = directed(candidate, &bg);
    let (recall, back_max) = directed(baseline, &cg);
    Some(PointAgreement {
        precision: round6(precision),
        recall: round6(recall),
        hausdorff_m: round6(forward_max.max(back_max)),
    })
}

/// Proper crossings between pairs of segments; touching and collinear
/// overlaps do not count, so neighbouring segments of one line never do.
fn crossings(segs: &[(Pt, Pt)]) -> usize {
    if segs.is_empty() {
        return 0;
    }
    let grid = SegmentGrid::new(segs, bounds([segs]), 0.0);
    // signed area only; f64::signum(0.0) is 1.0, which would count touches
    let orient =
        |a: Pt, b: Pt, c: Pt| (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
    let mut n = 0;
    for (i, &(a, b)) in segs.iter().enumerate() {
        for j in grid.near_box(a, b) {
            let j = j as usize;
            if j <= i {
                continue;
            }
            let (c, d) = segs[j];
            if orient(a, b, c) * orient(a, b, d) < 0.0 && orient(c, d, a) * orient(c, d, b) < 0.0 {
                n += 1;
            }
        }
    }
    n
}

fn stats(g: &CodeGeometry, segs: &[(Pt, Pt)]) -> CodeStats {
    CodeStats {
        features: g.features,
        points: g.points.len(),
        lines: g.lines.len(),
        length_m: round6(length(&g.lines)),
        polygons: g.polygons,
        area_m2: round6(g.area_m2),
        crossings: crossings(segs),
    }
}

/// Compare two files' geometry code by code. Codes missing on one side count
/// as empty there.
pub fn compare(
    baseline: &BTreeMap<String, CodeGeometry>,
    candidate: &BTreeMap<String, CodeGeometry>,
    tolerance: f64,
) -> BTreeMap<String, CodeComparison> {
    let empty = CodeGeometry::default();
    let codes: std::collections::BTreeSet<&String> =
        baseline.keys().chain(candidate.keys()).collect();
    codes
        .into_iter()
        .map(|code| {
            let b = baseline.get(code).unwrap_or(&empty);
            let c = candidate.get(code).unwrap_or(&empty);
            let (bs, cs) = (segments(&b.lines), segments(&c.lines));
            let comparison = CodeComparison {
                baseline: stats(b, &bs),
                candidate: stats(c, &cs),
                lines: line_agreement(&bs, &cs, tolerance),
                boundaries: line_agreement(&segments(&b.rings), &segments(&c.rings), tolerance),
                points: point_agreement(&b.points, &c.points, tolerance),
            };
            (code.clone(), comparison)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line_feature(code: &str, coords: Value) -> Value {
        json!({"type": "Feature", "properties": {"symbol": code},
               "geometry": {"type": "LineString", "coordinates": coords}})
    }

    fn collection(features: Vec<Value>) -> BTreeMap<String, CodeGeometry> {
        group_by_code(&json!({"type": "FeatureCollection", "features": features}))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn groups_by_symbol_then_isom_then_layer() {
        let fc = collection(vec![
            json!({"type": "Feature", "properties": {"symbol": "201", "isom": "x", "layer": "y"},
                   "geometry": {"type": "LineString", "coordinates": [[0, 0], [1, 0]]}}),
            line_feature("101", json!([[0, 0], [3, 4]])),
            json!({"type": "Feature", "properties": {"layer": "cliff2"},
                   "geometry": {"type": "Point", "coordinates": [1, 1]}}),
            json!({"type": "Feature", "properties": {"isom": 109},
                   "geometry": {"type": "MultiPoint", "coordinates": [[1, 1], [2, 2]]}}),
            json!({"type": "Feature", "properties": {},
                   "geometry": {"type": "Point", "coordinates": [0, 0]}}),
        ]);
        let keys: Vec<&str> = fc.keys().map(String::as_str).collect();
        assert_eq!(keys, ["101", "109", "201", "cliff2", "unknown"]);
        assert_eq!(fc["109"].points.len(), 2);
        assert!(close(length(&fc["101"].lines), 5.0));
    }

    #[test]
    fn polygon_area_subtracts_holes() {
        let fc = collection(vec![
            json!({"type": "Feature", "properties": {"isom": "406"},
            "geometry": {"type": "MultiPolygon", "coordinates": [
                [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]],
                 [[2, 2], [4, 2], [4, 4], [2, 4], [2, 2]]],
                [[[20, 0], [21, 0], [21, 1], [20, 0]]]
            ]}}),
        ]);
        let g = &fc["406"];
        assert_eq!(g.polygons, 2);
        assert!(close(g.area_m2, 100.0 - 4.0 + 0.5));
    }

    #[test]
    fn moved_polygon_with_same_area_is_a_change() {
        let square = |x0: i32| {
            collection(vec![
                json!({"type": "Feature", "properties": {"symbol": "406"},
                "geometry": {"type": "Polygon", "coordinates":
                    [[[x0, 0], [x0 + 10, 0], [x0 + 10, 10], [x0, 10], [x0, 0]]]}}),
            ])
        };
        let same = &compare(&square(0), &square(0), 1.0)["406"];
        assert!(!same.has_change());
        let moved = &compare(&square(0), &square(3), 1.0)["406"];
        assert_eq!(moved.baseline, moved.candidate);
        assert_eq!(moved.boundaries.unwrap().hausdorff_m, 3.0);
        assert!(moved.has_change());
    }

    #[test]
    fn identical_lines_agree_fully() {
        let a = collection(vec![line_feature(
            "101",
            json!([[0, 0], [100, 0], [100, 50]]),
        )]);
        let b = collection(vec![line_feature(
            "101",
            json!([[0, 0], [100, 0], [100, 50]]),
        )]);
        let c = &compare(&a, &b, 1.0)["101"];
        assert_eq!(c.baseline, c.candidate);
        let l = c.lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
        assert_eq!((l.hausdorff_m, l.mean_distance_m), (0.0, 0.0));
    }

    #[test]
    fn offset_line_has_known_distance() {
        let a = collection(vec![line_feature("101", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("101", json!([[0, 3], [100, 3]]))]);
        let l = compare(&a, &b, 1.0)["101"].lines.unwrap();
        assert!(close(l.hausdorff_m, 3.0));
        assert!(close(l.mean_distance_m, 3.0));
        assert!(close(l.mean_distance_back_m, 3.0));
        assert_eq!((l.precision, l.recall), (0.0, 0.0));
        // a tolerance above the offset matches everything
        let l = compare(&a, &b, 5.0)["101"].lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
    }

    #[test]
    fn partial_overlap_gives_length_fractions() {
        // reference cliff 0..100, emitted cliff 50..250: 50 m overlap
        let a = collection(vec![line_feature("201", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("201", json!([[50, 0], [250, 0]]))]);
        let l = compare(&a, &b, 1.0)["201"].lines.unwrap();
        // within the 1 m tolerance, 51 m match on each side; half-metre
        // sampling quantises the matched length to 0.5 m
        assert!((l.recall - 51.0 / 100.0).abs() <= 0.005, "{}", l.recall);
        assert!(
            (l.precision - 51.0 / 200.0).abs() <= 0.0025,
            "{}",
            l.precision
        );
        assert!(close(l.hausdorff_m, 150.0));
    }

    #[test]
    fn code_on_one_side_has_no_agreement() {
        let a = collection(vec![line_feature("201", json!([[0, 0], [10, 0]]))]);
        let b = collection(vec![line_feature("202", json!([[0, 0], [10, 0]]))]);
        let cmp = compare(&a, &b, 1.0);
        assert_eq!(cmp["201"].candidate, CodeStats::default());
        assert!(cmp["201"].lines.is_none());
        assert!(close(cmp["202"].candidate.length_m, 10.0));
    }

    #[test]
    fn points_match_within_tolerance() {
        let pts = |c: Value| {
            collection(vec![
                json!({"type": "Feature", "properties": {"isom": "109"},
                "geometry": {"type": "MultiPoint", "coordinates": c}}),
            ])
        };
        let a = pts(json!([[0, 0], [100, 0], [200, 0], [300, 0]]));
        let b = pts(json!([[0.5, 0], [100, 0.5], [250, 0]]));
        let p = compare(&a, &b, 1.0)["109"].points.unwrap();
        assert_eq!((p.precision, p.recall), (0.666667, 0.5));
        assert_eq!(p.hausdorff_m, 50.0);
    }

    #[test]
    fn counts_crossings_between_and_within_lines() {
        let fc = collection(vec![
            line_feature("101", json!([[0, 0], [10, 10]])),
            line_feature("101", json!([[0, 10], [10, 0]])),
            // a bow tie: one self-crossing
            line_feature("101", json!([[20, 0], [30, 10], [30, 0], [20, 10]])),
            // touching at an endpoint is not a crossing
            line_feature("101", json!([[10, 10], [15, 20]])),
        ]);
        let c = &compare(&fc, &BTreeMap::new(), 1.0)["101"];
        assert_eq!(c.baseline.crossings, 2);
    }

    #[test]
    fn nearest_searches_beyond_the_first_ring() {
        let segs = [([0.0, 0.0], [1.0, 0.0]), ([500.0, 500.0], [501.0, 500.0])];
        let grid = SegmentGrid::new(&segs[..1], bounds([&segs[..]]), 1.0);
        assert!(close(grid.nearest([500.0, 500.0]), 500.0f64.hypot(499.0)));
    }
}
