//! Vector metrics over GeoJSON FeatureCollections, grouped by symbol code.
//!
//! A feature's symbol code is its `isom_code` property ("NNN.NNN", see
//! [`crate::isom`]). A feature without one, or with a code the symbol table
//! does not list, is counted under the value it holds and reported, not
//! measured.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::round6;
use crate::geometry::{Bounds, Point2, signed_area};
use crate::isom::IsomCode;

type Segment = (Point2, Point2);

/// The key a feature without an `isom_code` is counted under.
const NO_CODE: &str = "(none)";

/// All geometry of one symbol code in one file.
#[derive(Debug, Default)]
pub struct CodeGeometry {
    features: usize,
    points: Vec<Point2>,
    lines: Vec<Vec<Point2>>,
    /// Polygon rings, outer and holes alike.
    rings: Vec<Vec<Point2>>,
    polygons: usize,
    area_m2: f64,
}

impl CodeGeometry {
    fn append(&mut self, other: CodeGeometry) {
        self.features += other.features;
        self.points.extend(other.points);
        self.lines.extend(other.lines);
        self.rings.extend(other.rings);
        self.polygons += other.polygons;
        self.area_m2 += other.area_m2;
    }
}

/// The geometry of one or more GeoJSON files, by symbol code.
#[derive(Debug, Default)]
pub struct FileGeometry {
    pub codes: BTreeMap<IsomCode, CodeGeometry>,
    /// Features whose `isom_code` is missing or not in the symbol table,
    /// counted by the value found.
    pub unknown: BTreeMap<String, usize>,
}

impl FileGeometry {
    /// Add another file's geometry, as when a run's tables are read as one map.
    pub fn append(&mut self, other: FileGeometry) {
        for (code, g) in other.codes {
            self.codes.entry(code).or_default().append(g);
        }
        for (code, n) in other.unknown {
            *self.unknown.entry(code).or_default() += n;
        }
    }
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

/// Features of one unknown `isom_code` value on each side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub struct UnknownCount {
    pub baseline: usize,
    pub candidate: usize,
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

/// One GeoJSON pair compared code by code.
#[derive(Debug, Serialize)]
pub struct VectorComparison {
    pub codes: BTreeMap<IsomCode, CodeComparison>,
    /// Features no symbol code could be read for, by the value found.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unknown_codes: BTreeMap<String, UnknownCount>,
}

impl VectorComparison {
    /// True when any code changed or the unknown codes differ in count.
    pub fn has_change(&self) -> bool {
        self.codes.values().any(CodeComparison::has_change)
            || self
                .unknown_codes
                .values()
                .any(|u| u.baseline != u.candidate)
    }
}

/// Read and parse a JSON file.
pub(crate) fn read_json(path: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Read a GeoJSON FeatureCollection and group its geometry by symbol code.
pub fn load(path: &Path) -> anyhow::Result<FileGeometry> {
    Ok(group_by_code(&read_json(path)?))
}

/// Group the features of a parsed FeatureCollection by symbol code.
pub fn group_by_code(collection: &Value) -> FileGeometry {
    let mut file = FileGeometry::default();
    let features = collection["features"].as_array().map_or(&[][..], |f| f);
    for feature in features {
        let code = &feature["properties"]["isom_code"];
        let Some(code) = code.as_str().and_then(|c| c.parse::<IsomCode>().ok()) else {
            let key = match code {
                Value::Null => NO_CODE.to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            *file.unknown.entry(key).or_default() += 1;
            continue;
        };
        let entry = file.codes.entry(code).or_default();
        entry.features += 1;
        add_geometry(entry, &feature["geometry"]);
    }
    file
}

fn point(v: &Value) -> Option<Point2> {
    Some(Point2::new(v.get(0)?.as_f64()?, v.get(1)?.as_f64()?))
}

fn line(v: &Value) -> Vec<Point2> {
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
        let a = signed_area(&ring).abs();
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

fn segments(lines: &[Vec<Point2>]) -> Vec<Segment> {
    lines
        .iter()
        .flat_map(|l| l.windows(2).map(|w| (w[0], w[1])))
        .collect()
}

fn length(lines: &[Vec<Point2>]) -> f64 {
    segments(lines).iter().map(|&(a, b)| a.distance(b)).sum()
}

/// Bounding box of every segment end in `sets`; `None` when there are none.
fn bounds<'a>(sets: impl IntoIterator<Item = &'a [Segment]>) -> Option<Bounds> {
    Bounds::around(sets.into_iter().flatten().flat_map(|&(a, b)| [a, b]))
}

/// A uniform grid over segments for nearest-distance and overlap queries.
/// Points are stored as zero-length segments.
struct SegmentGrid<'a> {
    segs: &'a [Segment],
    origin: Point2,
    cell: f64,
    nx: usize,
    ny: usize,
    cells: Vec<Vec<u32>>,
}

impl<'a> SegmentGrid<'a> {
    /// `bb` must cover every point the grid will be queried with.
    fn new(segs: &'a [Segment], bb: &Bounds, min_cell: f64) -> Self {
        let (w, h) = ((bb.xmax - bb.xmin).max(0.0), (bb.ymax - bb.ymin).max(0.0));
        // about one segment per cell (at most n cells along a thin box),
        // never finer than min_cell
        let n = segs.len().max(1) as f64;
        let cell = (w * h / n).sqrt().max(w.max(h) / n).max(min_cell);
        let cell = if cell > 0.0 { cell } else { 1.0 };
        let nx = (w / cell) as usize + 1;
        let ny = (h / cell) as usize + 1;
        let mut grid = Self {
            segs,
            origin: Point2::new(bb.xmin, bb.ymin),
            cell,
            nx,
            ny,
            cells: vec![Vec::new(); nx * ny],
        };
        for (i, &(a, b)) in segs.iter().enumerate() {
            let ((x0, y0), (x1, y1)) = grid.cells_of(a, b);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    grid.cells[y * nx + x].push(i as u32);
                }
            }
        }
        grid
    }

    fn cell_of(&self, x: f64, y: f64) -> (usize, usize) {
        let x = ((x - self.origin.x) / self.cell).max(0.0) as usize;
        let y = ((y - self.origin.y) / self.cell).max(0.0) as usize;
        (x.min(self.nx - 1), y.min(self.ny - 1))
    }

    /// The lowest and highest cell under the box of segment `(a, b)`.
    fn cells_of(&self, a: Point2, b: Point2) -> ((usize, usize), (usize, usize)) {
        (
            self.cell_of(a.x.min(b.x), a.y.min(b.y)),
            self.cell_of(a.x.max(b.x), a.y.max(b.y)),
        )
    }

    /// Distance from `p` to the nearest segment, searching outward ring by ring.
    fn nearest(&self, p: Point2) -> f64 {
        let (cx, cy) = self.cell_of(p.x, p.y);
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
                        best = best.min(p.distance_to_segment(a, b));
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
    fn near_box(&self, a: Point2, b: Point2) -> Vec<u32> {
        let ((x0, y0), (x1, y1)) = self.cells_of(a, b);
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

fn directed(from: &[Segment], to: &SegmentGrid, tolerance: f64) -> Directed {
    let step = tolerance / 2.0;
    let mut d = Directed {
        total: 0.0,
        matched: 0.0,
        weighted_distance: 0.0,
        max: 0.0,
    };
    for &(a, b) in from {
        let len = a.distance(b);
        let pieces = (len / step).ceil().max(1.0);
        let w = len / pieces;
        for k in 0..pieces as usize {
            let t = (k as f64 + 0.5) / pieces;
            let near = to.nearest(Point2::new(a.x + t * (b.x - a.x), a.y + t * (b.y - a.y)));
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
    baseline: &[Segment],
    candidate: &[Segment],
    tolerance: f64,
) -> Option<LineAgreement> {
    if baseline.is_empty() || candidate.is_empty() {
        return None;
    }
    let bb = bounds([baseline, candidate])?;
    let base_grid = SegmentGrid::new(baseline, &bb, tolerance);
    let cand_grid = SegmentGrid::new(candidate, &bb, tolerance);
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

fn point_agreement(
    baseline: &[Point2],
    candidate: &[Point2],
    tolerance: f64,
) -> Option<PointAgreement> {
    let as_segs = |pts: &[Point2]| pts.iter().map(|&p| (p, p)).collect::<Vec<_>>();
    let (b, c) = (as_segs(baseline), as_segs(candidate));
    if b.is_empty() || c.is_empty() {
        return None;
    }
    let bb = bounds([&b[..], &c[..]])?;
    let (bg, cg) = (
        SegmentGrid::new(&b, &bb, tolerance),
        SegmentGrid::new(&c, &bb, tolerance),
    );
    // (share within tolerance, largest distance)
    let directed = |pts: &[Point2], grid: &SegmentGrid| {
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
fn crossings(segs: &[Segment]) -> usize {
    let Some(bb) = bounds([segs]) else {
        return 0;
    };
    let grid = SegmentGrid::new(segs, &bb, 0.0);
    // signed area only; f64::signum(0.0) is 1.0, which would count touches
    let orient =
        |a: Point2, b: Point2, c: Point2| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
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

fn stats(g: &CodeGeometry, segs: &[Segment]) -> CodeStats {
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
/// as empty there; unknown codes are counted on each side.
pub fn compare(
    baseline: &FileGeometry,
    candidate: &FileGeometry,
    tolerance: f64,
) -> VectorComparison {
    let empty = CodeGeometry::default();
    let codes: BTreeSet<IsomCode> = baseline
        .codes
        .keys()
        .chain(candidate.codes.keys())
        .copied()
        .collect();
    let codes = codes
        .into_iter()
        .map(|code| {
            let b = baseline.codes.get(&code).unwrap_or(&empty);
            let c = candidate.codes.get(&code).unwrap_or(&empty);
            let (bs, cs) = (segments(&b.lines), segments(&c.lines));
            let comparison = CodeComparison {
                baseline: stats(b, &bs),
                candidate: stats(c, &cs),
                lines: line_agreement(&bs, &cs, tolerance),
                boundaries: line_agreement(&segments(&b.rings), &segments(&c.rings), tolerance),
                points: point_agreement(&b.points, &c.points, tolerance),
            };
            (code, comparison)
        })
        .collect();
    let mut unknown_codes: BTreeMap<String, UnknownCount> = BTreeMap::new();
    for (code, &n) in &baseline.unknown {
        unknown_codes.entry(code.clone()).or_default().baseline = n;
    }
    for (code, &n) in &candidate.unknown {
        unknown_codes.entry(code.clone()).or_default().candidate = n;
    }
    VectorComparison {
        codes,
        unknown_codes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use IsomCode::*;

    fn line_feature(code: &str, coords: Value) -> Value {
        json!({"type": "Feature", "properties": {"isom_code": code},
               "geometry": {"type": "LineString", "coordinates": coords}})
    }

    fn collection(features: Vec<Value>) -> FileGeometry {
        group_by_code(&json!({"type": "FeatureCollection", "features": features}))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn groups_by_isom_code_and_counts_unknown_codes() {
        let fc = collection(vec![
            line_feature("201.000", json!([[0, 0], [1, 0]])),
            line_feature("101.000", json!([[0, 0], [3, 4]])),
            json!({"type": "Feature", "properties": {"isom_code": "109.000"},
                   "geometry": {"type": "MultiPoint", "coordinates": [[1, 1], [2, 2]]}}),
            // the property names earlier writers used are not read
            json!({"type": "Feature", "properties": {"symbol": "201.000", "layer": "cliff2"},
                   "geometry": {"type": "Point", "coordinates": [1, 1]}}),
            line_feature("101", json!([[0, 0], [1, 0]])),
            line_feature("999.000", json!([[0, 0], [1, 0]])),
            json!({"type": "Feature", "properties": {"isom_code": 109},
                   "geometry": {"type": "Point", "coordinates": [0, 0]}}),
        ]);
        let keys: Vec<IsomCode> = fc.codes.keys().copied().collect();
        assert_eq!(keys, [C101_000, C109_000, C201_000]);
        assert_eq!(fc.codes[&C109_000].points.len(), 2);
        assert!(close(length(&fc.codes[&C101_000].lines), 5.0));
        let unknown: Vec<(&str, usize)> =
            fc.unknown.iter().map(|(k, &n)| (k.as_str(), n)).collect();
        assert_eq!(
            unknown,
            [("(none)", 1), ("101", 1), ("109", 1), ("999.000", 1)]
        );
    }

    #[test]
    fn unknown_codes_are_reported_and_count_as_a_change_when_they_differ() {
        let a = collection(vec![line_feature("101", json!([[0, 0], [1, 0]]))]);
        let b = collection(vec![]);
        let same = compare(&a, &a, 1.0);
        assert!(same.codes.is_empty());
        assert_eq!(
            same.unknown_codes["101"],
            UnknownCount {
                baseline: 1,
                candidate: 1
            }
        );
        assert!(!same.has_change());
        let gone = compare(&a, &b, 1.0);
        assert_eq!(
            gone.unknown_codes["101"],
            UnknownCount {
                baseline: 1,
                candidate: 0
            }
        );
        assert!(gone.has_change());
        let json = serde_json::to_value(compare(&b, &b, 1.0)).unwrap();
        assert!(json.get("unknown_codes").is_none(), "{json}");
    }

    #[test]
    fn appending_merges_codes_across_files() {
        let mut a = collection(vec![line_feature("101.000", json!([[0, 0], [1, 0]]))]);
        a.append(collection(vec![
            line_feature("101.000", json!([[0, 1], [2, 1]])),
            line_feature("201.000", json!([[0, 0], [1, 0]])),
            line_feature("x", json!([[0, 0], [1, 0]])),
        ]));
        assert_eq!(a.codes[&C101_000].features, 2);
        assert!(close(length(&a.codes[&C101_000].lines), 3.0));
        assert_eq!((a.codes.len(), a.unknown["x"]), (2, 1));
    }

    #[test]
    fn polygon_area_subtracts_holes() {
        let fc = collection(vec![
            json!({"type": "Feature", "properties": {"isom_code": "406.000"},
            "geometry": {"type": "MultiPolygon", "coordinates": [
                [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]],
                 [[2, 2], [4, 2], [4, 4], [2, 4], [2, 2]]],
                [[[20, 0], [21, 0], [21, 1], [20, 0]]]
            ]}}),
        ]);
        let g = &fc.codes[&C406_000];
        assert_eq!(g.polygons, 2);
        assert!(close(g.area_m2, 100.0 - 4.0 + 0.5));
    }

    #[test]
    fn moved_polygon_with_same_area_is_a_change() {
        let square = |x0: i32| {
            collection(vec![
                json!({"type": "Feature", "properties": {"isom_code": "406.000"},
                "geometry": {"type": "Polygon", "coordinates":
                    [[[x0, 0], [x0 + 10, 0], [x0 + 10, 10], [x0, 10], [x0, 0]]]}}),
            ])
        };
        let same = &compare(&square(0), &square(0), 1.0).codes[&C406_000];
        assert!(!same.has_change());
        let moved = &compare(&square(0), &square(3), 1.0).codes[&C406_000];
        assert_eq!(moved.baseline, moved.candidate);
        assert_eq!(moved.boundaries.unwrap().hausdorff_m, 3.0);
        assert!(moved.has_change());
    }

    #[test]
    fn identical_lines_agree_fully() {
        let a = collection(vec![line_feature(
            "101.000",
            json!([[0, 0], [100, 0], [100, 50]]),
        )]);
        let b = collection(vec![line_feature(
            "101.000",
            json!([[0, 0], [100, 0], [100, 50]]),
        )]);
        let c = &compare(&a, &b, 1.0).codes[&C101_000];
        assert_eq!(c.baseline, c.candidate);
        let l = c.lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
        assert_eq!((l.hausdorff_m, l.mean_distance_m), (0.0, 0.0));
    }

    #[test]
    fn offset_line_has_known_distance() {
        let a = collection(vec![line_feature("101.000", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("101.000", json!([[0, 3], [100, 3]]))]);
        let l = compare(&a, &b, 1.0).codes[&C101_000].lines.unwrap();
        assert!(close(l.hausdorff_m, 3.0));
        assert!(close(l.mean_distance_m, 3.0));
        assert!(close(l.mean_distance_back_m, 3.0));
        assert_eq!((l.precision, l.recall), (0.0, 0.0));
        // a tolerance above the offset matches everything
        let l = compare(&a, &b, 5.0).codes[&C101_000].lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
    }

    #[test]
    fn partial_overlap_gives_length_fractions() {
        // reference cliff 0..100, emitted cliff 50..250: 50 m overlap
        let a = collection(vec![line_feature("201.000", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("201.000", json!([[50, 0], [250, 0]]))]);
        let l = compare(&a, &b, 1.0).codes[&C201_000].lines.unwrap();
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
        let a = collection(vec![line_feature("201.000", json!([[0, 0], [10, 0]]))]);
        let b = collection(vec![line_feature("202.000", json!([[0, 0], [10, 0]]))]);
        let cmp = compare(&a, &b, 1.0);
        assert_eq!(cmp.codes[&C201_000].candidate, CodeStats::default());
        assert!(cmp.codes[&C201_000].lines.is_none());
        assert!(close(cmp.codes[&C202_000].candidate.length_m, 10.0));
    }

    #[test]
    fn points_match_within_tolerance() {
        let pts = |c: Value| {
            collection(vec![
                json!({"type": "Feature", "properties": {"isom_code": "109.000"},
                "geometry": {"type": "MultiPoint", "coordinates": c}}),
            ])
        };
        let a = pts(json!([[0, 0], [100, 0], [200, 0], [300, 0]]));
        let b = pts(json!([[0.5, 0], [100, 0.5], [250, 0]]));
        let p = compare(&a, &b, 1.0).codes[&C109_000].points.unwrap();
        assert_eq!((p.precision, p.recall), (0.666667, 0.5));
        assert_eq!(p.hausdorff_m, 50.0);
    }

    #[test]
    fn counts_crossings_between_and_within_lines() {
        let fc = collection(vec![
            line_feature("101.000", json!([[0, 0], [10, 10]])),
            line_feature("101.000", json!([[0, 10], [10, 0]])),
            // a bow tie: one self-crossing
            line_feature("101.000", json!([[20, 0], [30, 10], [30, 0], [20, 10]])),
            // touching at an endpoint is not a crossing
            line_feature("101.000", json!([[10, 10], [15, 20]])),
        ]);
        let c = &compare(&fc, &FileGeometry::default(), 1.0).codes[&C101_000];
        assert_eq!(c.baseline.crossings, 2);
    }

    #[test]
    fn nearest_searches_beyond_the_first_ring() {
        let p = Point2::new;
        let segs = [
            (p(0.0, 0.0), p(1.0, 0.0)),
            (p(500.0, 500.0), p(501.0, 500.0)),
        ];
        let grid = SegmentGrid::new(&segs[..1], &bounds([&segs[..]]).unwrap(), 1.0);
        assert!(close(grid.nearest(p(500.0, 500.0)), 500.0f64.hypot(499.0)));
    }
}
