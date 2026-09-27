//! Vector metrics over GeoJSON FeatureCollections, grouped by symbol code.
//!
//! A feature's symbol code is its `isom_code` property ("NNN.NNN", see
//! [`crate::isom`]). A feature without one, or with a code the symbol table
//! does not list, is counted under the value it holds and reported, not
//! measured.
//! Per code the module reports totals for each side (features, point count,
//! line length, polygon area, crossings between lines of that code), how many
//! features' other properties have no counterpart on the other side, and,
//! when both sides hold that geometry kind, how well the candidate agrees with
//! the baseline:
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
//! a quarter of the tolerance. Every measure is rounded to six decimals, and
//! sums run over sorted values and segments, so the order features are written
//! in does not reach the report.
//! Coordinates are taken to be projected metres, as the pipeline writes them;
//! lengths and areas of WGS84 (RFC 7946) GeoJSON would be in degrees.
//!
//! Parsing is strict: a feature with a geometry type the reader does not take
//! (Point, LineString, Polygon, and the MultiLineString and MultiPolygon a
//! reference map from another tool may hold) or a malformed position is an
//! error naming the feature, never silently empty.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail, ensure};
use serde::Serialize;
use serde_json::Value;

use super::round6;
use crate::geometry::{Bounds, Point2, signed_area};
use crate::isom::IsomCode;

type Segment = (Point2, Point2);

/// The key a feature without an `isom_code` is counted under.
const NO_CODE: &str = "(none)";

/// The most distance samples one line comparison may take; a smaller
/// tolerance over a large map is refused rather than left to run for hours.
pub const MAX_SAMPLES: f64 = 20_000_000.0;

/// All geometry of one symbol code in one map.
#[derive(Debug, Default)]
pub struct CodeGeometry {
    features: usize,
    points: Vec<Point2>,
    lines: Vec<Vec<Point2>>,
    /// Polygon rings, outer and holes alike.
    rings: Vec<Vec<Point2>>,
    polygons: usize,
    /// Signed area contributions: outer rings positive, holes negative.
    ring_areas: Vec<f64>,
    /// Each feature's properties other than `isom_code`, as sorted-key JSON,
    /// with the number of features holding them.
    properties: BTreeMap<String, usize>,
}

impl CodeGeometry {
    fn append(&mut self, other: CodeGeometry) {
        self.features += other.features;
        self.points.extend(other.points);
        self.lines.extend(other.lines);
        self.rings.extend(other.rings);
        self.polygons += other.polygons;
        self.ring_areas.extend(other.ring_areas);
        for (p, n) in other.properties {
            *self.properties.entry(p).or_default() += n;
        }
    }
}

/// The geometry of a map, one GeoJSON file or several read as one, by symbol
/// code.
#[derive(Debug, Default)]
pub struct MapGeometry {
    pub codes: BTreeMap<IsomCode, CodeGeometry>,
    /// Features whose `isom_code` is missing or not in the symbol table,
    /// counted by the value found.
    pub unknown: BTreeMap<String, usize>,
    /// The collection members other than `type` and `features` (such as
    /// `crs`), as sorted-key JSON, one entry per distinct value.
    pub collection: BTreeSet<String>,
}

impl MapGeometry {
    /// Add another file's geometry, as when a run's tables are read as one map.
    pub fn append(&mut self, other: MapGeometry) {
        for (code, g) in other.codes {
            self.codes.entry(code).or_default().append(g);
        }
        for (code, n) in other.unknown {
            *self.unknown.entry(code).or_default() += n;
        }
        self.collection.extend(other.collection);
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
    /// Proper crossings between any two line segments of this code; lines of
    /// different codes are not tested against each other.
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
    /// Features, on either side, whose properties other than `isom_code`
    /// match no feature of this code on the other side.
    pub properties_unmatched: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineAgreement>,
    /// Line agreement over polygon rings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<LineAgreement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<PointAgreement>,
}

impl CodeComparison {
    /// True unless both sides have the same totals, properties and geometry
    /// (to the rounding of the report).
    pub fn has_change(&self) -> bool {
        self.baseline != self.candidate
            || self.properties_unmatched > 0
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
    /// The collection members other than the features (such as `crs`)
    /// differ.
    pub collection_changed: bool,
}

impl VectorComparison {
    /// True when any code changed, the unknown codes differ in count, or the
    /// collection members differ.
    pub fn has_change(&self) -> bool {
        self.collection_changed
            || self.codes.values().any(CodeComparison::has_change)
            || self
                .unknown_codes
                .values()
                .any(|u| u.baseline != u.candidate)
    }
}

/// Group the features of a parsed FeatureCollection by symbol code. A
/// malformed collection or feature is an error naming the feature.
pub fn group_by_code(collection: &Value) -> anyhow::Result<MapGeometry> {
    let Some(members) = collection.as_object() else {
        bail!("not a JSON object");
    };
    ensure!(
        collection["type"] == "FeatureCollection",
        "not a FeatureCollection"
    );
    let Some(features) = collection["features"].as_array() else {
        bail!("`features` is not an array");
    };
    let mut map = MapGeometry::default();
    let mut rest = members.clone();
    rest.remove("type");
    rest.remove("features");
    if !rest.is_empty() {
        map.collection.insert(Value::Object(rest).to_string());
    }
    for (i, feature) in features.iter().enumerate() {
        add_feature(&mut map, feature).with_context(|| format!("feature {i}"))?;
    }
    Ok(map)
}

fn add_feature(map: &mut MapGeometry, feature: &Value) -> anyhow::Result<()> {
    ensure!(feature["type"] == "Feature", "not a Feature");
    let mut geometry = CodeGeometry::default();
    add_geometry(&mut geometry, &feature["geometry"])?;
    let code = &feature["properties"]["isom_code"];
    let Some(code) = code.as_str().and_then(|c| c.parse::<IsomCode>().ok()) else {
        // checked above, but not measured
        let key = match code {
            Value::Null => NO_CODE.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        *map.unknown.entry(key).or_default() += 1;
        return Ok(());
    };
    let mut properties = feature["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    properties.remove("isom_code");
    geometry.features = 1;
    geometry
        .properties
        .insert(Value::Object(properties).to_string(), 1);
    map.codes.entry(code).or_default().append(geometry);
    Ok(())
}

fn point(v: &Value) -> anyhow::Result<Point2> {
    match v.as_array().map(Vec::as_slice) {
        Some([x, y, ..]) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => Ok(Point2::new(x, y)),
            _ => bail!("a position holds a non-number: {v}"),
        },
        _ => bail!("a position is not an array of two or more numbers: {v}"),
    }
}

fn positions(v: &Value, min: usize, what: &str) -> anyhow::Result<Vec<Point2>> {
    let Some(list) = v.as_array() else {
        bail!("{what} coordinates are not an array");
    };
    ensure!(
        list.len() >= min,
        "{what} has {} positions, fewer than {min}",
        list.len()
    );
    list.iter().map(point).collect()
}

fn list<'a>(v: &'a Value, what: &str) -> anyhow::Result<&'a [Value]> {
    v.as_array()
        .map(Vec::as_slice)
        .with_context(|| format!("{what} coordinates are not an array"))
}

fn add_polygon(entry: &mut CodeGeometry, rings: &Value) -> anyhow::Result<()> {
    let rings = list(rings, "a Polygon")?;
    ensure!(!rings.is_empty(), "a Polygon has no rings");
    entry.polygons += 1;
    for (i, ring) in rings.iter().enumerate() {
        let ring = positions(ring, 4, "a Polygon ring")?;
        let a = signed_area(&ring).abs();
        entry.rings.push(ring);
        // the first ring is the outer boundary, the rest are holes
        entry.ring_areas.push(if i == 0 { a } else { -a });
    }
    Ok(())
}

fn add_geometry(entry: &mut CodeGeometry, geometry: &Value) -> anyhow::Result<()> {
    let coords = &geometry["coordinates"];
    match geometry["type"].as_str() {
        Some("Point") => entry.points.push(point(coords)?),
        // RFC 7946 wants two positions, but the pipeline writes one-position
        // contours into temp/contours.geojson: read them as zero-length lines
        Some("LineString") => entry.lines.push(positions(coords, 1, "a LineString")?),
        Some("MultiLineString") => {
            for line in list(coords, "a MultiLineString")? {
                entry.lines.push(positions(line, 1, "a LineString")?);
            }
        }
        Some("Polygon") => add_polygon(entry, coords)?,
        Some("MultiPolygon") => {
            for polygon in list(coords, "a MultiPolygon")? {
                add_polygon(entry, polygon)?;
            }
        }
        Some(other) => bail!("geometry type {other} is not read"),
        None => bail!("no geometry type"),
    }
    Ok(())
}

/// Every segment of `lines`, in a canonical order so sums over them do not
/// depend on the order features were written in.
fn segments(lines: &[Vec<Point2>]) -> Vec<Segment> {
    let mut segs: Vec<Segment> = lines
        .iter()
        .flat_map(|l| l.windows(2).map(|w| (w[0], w[1])))
        .collect();
    segs.sort_by(|(a, b), (c, d)| {
        (a.x.total_cmp(&c.x))
            .then(a.y.total_cmp(&c.y))
            .then(b.x.total_cmp(&d.x))
            .then(b.y.total_cmp(&d.y))
    });
    segs
}

/// A sum over sorted values, so it does not depend on their order.
fn sorted_sum(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.into_iter().sum()
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
struct DirectedDistances {
    total: f64,
    matched: f64,
    weighted_distance: f64,
    max: f64,
}

/// The number of samples `from` takes at a step of `step`.
fn samples(from: &[Segment], step: f64) -> f64 {
    from.iter()
        .map(|&(a, b)| (a.distance(b) / step).ceil().max(1.0))
        .sum()
}

fn directed(from: &[Segment], to: &SegmentGrid, tolerance: f64) -> DirectedDistances {
    let step = tolerance / 2.0;
    let mut d = DirectedDistances {
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
) -> anyhow::Result<Option<LineAgreement>> {
    let Some(bb) = bounds([baseline, candidate]) else {
        return Ok(None);
    };
    if baseline.is_empty() || candidate.is_empty() {
        return Ok(None);
    }
    let n = samples(baseline, tolerance / 2.0) + samples(candidate, tolerance / 2.0);
    ensure!(
        n <= MAX_SAMPLES,
        "a tolerance of {tolerance} m takes {n:.0} samples, more than {MAX_SAMPLES:.0}; use a larger --tolerance"
    );
    let base_grid = SegmentGrid::new(baseline, &bb, tolerance);
    let cand_grid = SegmentGrid::new(candidate, &bb, tolerance);
    let forward = directed(candidate, &base_grid, tolerance);
    let back = directed(baseline, &cand_grid, tolerance);
    if forward.total == 0.0 || back.total == 0.0 {
        return Ok(None);
    }
    Ok(Some(LineAgreement {
        precision: round6(forward.matched / forward.total),
        recall: round6(back.matched / back.total),
        hausdorff_m: round6(forward.max.max(back.max)),
        mean_distance_m: round6(forward.weighted_distance / forward.total),
        mean_distance_back_m: round6(back.weighted_distance / back.total),
    }))
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
    // (share within tolerance, largest distance); counts and a maximum, so
    // the order of the points does not matter
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

fn code_stats(g: &CodeGeometry, segs: &[Segment]) -> CodeStats {
    CodeStats {
        features: g.features,
        points: g.points.len(),
        lines: g.lines.len(),
        length_m: round6(sorted_sum(
            segs.iter().map(|&(a, b)| a.distance(b)).collect(),
        )),
        polygons: g.polygons,
        area_m2: round6(sorted_sum(g.ring_areas.clone())),
        crossings: crossings(segs),
    }
}

/// Features on either side whose properties have no counterpart on the other.
fn properties_unmatched(b: &BTreeMap<String, usize>, c: &BTreeMap<String, usize>) -> usize {
    let keys: BTreeSet<&String> = b.keys().chain(c.keys()).collect();
    keys.into_iter()
        .map(|k| {
            let (nb, nc) = (b.get(k).copied(), c.get(k).copied());
            nb.unwrap_or(0).abs_diff(nc.unwrap_or(0))
        })
        .sum()
}

/// Compare two maps' geometry code by code. Codes missing on one side count
/// as empty there; unknown codes are counted on each side. Fails when the
/// tolerance would take more than [`MAX_SAMPLES`].
pub fn compare(
    baseline: &MapGeometry,
    candidate: &MapGeometry,
    tolerance: f64,
) -> anyhow::Result<VectorComparison> {
    let empty = CodeGeometry::default();
    let codes: BTreeSet<IsomCode> = baseline
        .codes
        .keys()
        .chain(candidate.codes.keys())
        .copied()
        .collect();
    let mut compared = BTreeMap::new();
    for code in codes {
        let b = baseline.codes.get(&code).unwrap_or(&empty);
        let c = candidate.codes.get(&code).unwrap_or(&empty);
        let (bs, cs) = (segments(&b.lines), segments(&c.lines));
        let comparison = CodeComparison {
            baseline: code_stats(b, &bs),
            candidate: code_stats(c, &cs),
            properties_unmatched: properties_unmatched(&b.properties, &c.properties),
            lines: line_agreement(&bs, &cs, tolerance).with_context(|| code.to_string())?,
            boundaries: line_agreement(&segments(&b.rings), &segments(&c.rings), tolerance)
                .with_context(|| code.to_string())?,
            points: point_agreement(&b.points, &c.points, tolerance),
        };
        compared.insert(code, comparison);
    }
    let mut unknown_codes: BTreeMap<String, UnknownCount> = BTreeMap::new();
    for (code, &n) in &baseline.unknown {
        unknown_codes.entry(code.clone()).or_default().baseline = n;
    }
    for (code, &n) in &candidate.unknown {
        unknown_codes.entry(code.clone()).or_default().candidate = n;
    }
    Ok(VectorComparison {
        codes: compared,
        unknown_codes,
        collection_changed: baseline.collection != candidate.collection,
    })
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

    fn collection(features: Vec<Value>) -> MapGeometry {
        group_by_code(&json!({"type": "FeatureCollection", "features": features})).unwrap()
    }

    fn compare_ok(b: &MapGeometry, c: &MapGeometry, tolerance: f64) -> VectorComparison {
        compare(b, c, tolerance).unwrap()
    }

    fn length(lines: &[Vec<Point2>]) -> f64 {
        sorted_sum(
            segments(lines)
                .iter()
                .map(|&(a, b)| a.distance(b))
                .collect(),
        )
    }

    fn point_feature(code: &str, x: f64, y: f64) -> Value {
        json!({"type": "Feature", "properties": {"isom_code": code},
               "geometry": {"type": "Point", "coordinates": [x, y]}})
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn groups_by_isom_code_and_counts_unknown_codes() {
        let fc = collection(vec![
            line_feature("201.000", json!([[0, 0], [1, 0]])),
            line_feature("101.000", json!([[0, 0], [3, 4]])),
            point_feature("109.000", 1.0, 1.0),
            point_feature("109.000", 2.0, 2.0),
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
        let same = compare_ok(&a, &a, 1.0);
        assert!(same.codes.is_empty());
        assert_eq!(
            same.unknown_codes["101"],
            UnknownCount {
                baseline: 1,
                candidate: 1
            }
        );
        assert!(!same.has_change());
        let gone = compare_ok(&a, &b, 1.0);
        assert_eq!(
            gone.unknown_codes["101"],
            UnknownCount {
                baseline: 1,
                candidate: 0
            }
        );
        assert!(gone.has_change());
        let json = serde_json::to_value(compare_ok(&b, &b, 1.0)).unwrap();
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
        assert!(close(sorted_sum(g.ring_areas.clone()), 100.0 - 4.0 + 0.5));
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
        let same = &compare_ok(&square(0), &square(0), 1.0).codes[&C406_000];
        assert!(!same.has_change());
        let moved = &compare_ok(&square(0), &square(3), 1.0).codes[&C406_000];
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
        let c = &compare_ok(&a, &b, 1.0).codes[&C101_000];
        assert_eq!(c.baseline, c.candidate);
        let l = c.lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
        assert_eq!((l.hausdorff_m, l.mean_distance_m), (0.0, 0.0));
    }

    #[test]
    fn offset_line_has_known_distance() {
        let a = collection(vec![line_feature("101.000", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("101.000", json!([[0, 3], [100, 3]]))]);
        let l = compare_ok(&a, &b, 1.0).codes[&C101_000].lines.unwrap();
        assert!(close(l.hausdorff_m, 3.0));
        assert!(close(l.mean_distance_m, 3.0));
        assert!(close(l.mean_distance_back_m, 3.0));
        assert_eq!((l.precision, l.recall), (0.0, 0.0));
        // a tolerance above the offset matches everything
        let l = compare_ok(&a, &b, 5.0).codes[&C101_000].lines.unwrap();
        assert_eq!((l.precision, l.recall), (1.0, 1.0));
    }

    #[test]
    fn partial_overlap_gives_length_fractions() {
        // reference cliff 0..100, emitted cliff 50..250: 50 m overlap
        let a = collection(vec![line_feature("201.000", json!([[0, 0], [100, 0]]))]);
        let b = collection(vec![line_feature("201.000", json!([[50, 0], [250, 0]]))]);
        let l = compare_ok(&a, &b, 1.0).codes[&C201_000].lines.unwrap();
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
        let cmp = compare_ok(&a, &b, 1.0);
        assert_eq!(cmp.codes[&C201_000].candidate, CodeStats::default());
        assert!(cmp.codes[&C201_000].lines.is_none());
        assert!(close(cmp.codes[&C202_000].candidate.length_m, 10.0));
    }

    #[test]
    fn points_match_within_tolerance() {
        let pts = |c: &[(f64, f64)]| {
            collection(
                c.iter()
                    .map(|&(x, y)| point_feature("109.000", x, y))
                    .collect(),
            )
        };
        let a = pts(&[(0.0, 0.0), (100.0, 0.0), (200.0, 0.0), (300.0, 0.0)]);
        let b = pts(&[(0.5, 0.0), (100.0, 0.5), (250.0, 0.0)]);
        let p = compare_ok(&a, &b, 1.0).codes[&C109_000].points.unwrap();
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
        let c = &compare_ok(&fc, &MapGeometry::default(), 1.0).codes[&C101_000];
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

    #[test]
    fn malformed_input_is_an_error_naming_the_feature() {
        let parse = |v: Value| group_by_code(&v).map_err(|e| format!("{e:#}"));
        let fc = |f: Value| json!({"type": "FeatureCollection", "features": [line_feature("101.000", json!([[0, 0], [1, 0]])), f]});
        assert!(
            parse(json!({"type": "FeatureCollection"}))
                .unwrap_err()
                .contains("features")
        );
        assert!(parse(json!([])).is_err());
        let bad = [
            (
                json!({"type": "Feature", "properties": {"isom_code": "109.000"},
                    "geometry": {"type": "MultiPoint", "coordinates": [[0, 0]]}}),
                "MultiPoint",
            ),
            (
                json!({"type": "Feature", "properties": {"isom_code": "101.000"},
                    "geometry": {"type": "GeometryCollection", "geometries": []}}),
                "GeometryCollection",
            ),
            (
                json!({"type": "Feature", "properties": {"isom_code": "101.000"}, "geometry": null}),
                "no geometry type",
            ),
            (
                line_feature("101.000", json!([[0, 0], ["x", 1]])),
                "non-number",
            ),
            (line_feature("101.000", json!([])), "fewer than 1"),
            (line_feature("101", json!([[0], [1, 1]])), "position"),
            (
                json!({"type": "Feature", "properties": {"isom_code": "406.000"},
                    "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [0, 0]]]}}),
                "fewer than 4",
            ),
        ];
        for (feature, expected) in bad {
            let e = parse(fc(feature)).unwrap_err();
            assert!(e.starts_with("feature 1: ") && e.contains(expected), "{e}");
        }
    }

    #[test]
    fn changed_properties_are_a_change() {
        let contour = |level: f64, extra: Value| {
            let mut f = line_feature("101.000", json!([[0, 0], [10, 0]]));
            f["properties"]["level_m"] = json!(level);
            if let Value::Object(m) = extra {
                f["properties"].as_object_mut().unwrap().extend(m);
            }
            f
        };
        let a = collection(vec![contour(100.0, json!({})), contour(105.0, json!({}))]);
        // the same features in the other order: no change
        let swapped = collection(vec![contour(105.0, json!({})), contour(100.0, json!({}))]);
        assert!(!compare_ok(&a, &swapped, 1.0).has_change());
        let relevelled = collection(vec![contour(100.0, json!({})), contour(110.0, json!({}))]);
        let c = &compare_ok(&a, &relevelled, 1.0).codes[&C101_000];
        assert_eq!(c.properties_unmatched, 2);
        assert!(c.has_change());
        let flagged = collection(vec![
            contour(100.0, json!({"ugly": true})),
            contour(105.0, json!({})),
        ]);
        assert_eq!(
            compare_ok(&a, &flagged, 1.0).codes[&C101_000].properties_unmatched,
            2
        );
    }

    #[test]
    fn collection_members_such_as_crs_are_compared() {
        let with_crs = |name: &str| {
            let fc = json!({"type": "FeatureCollection", "features": [],
                            "crs": {"type": "name", "properties": {"name": name}}});
            group_by_code(&fc).unwrap()
        };
        let (a, b) = (with_crs("EPSG:3067"), with_crs("EPSG:25832"));
        assert!(!compare_ok(&a, &a, 1.0).has_change());
        let c = compare_ok(&a, &b, 1.0);
        assert!(c.collection_changed && c.has_change());
        assert!(compare_ok(&a, &MapGeometry::default(), 1.0).has_change());
    }

    #[test]
    fn sums_do_not_depend_on_feature_order() {
        let lines: Vec<Value> = (0..50)
            .map(|i| {
                let x = f64::from(i) * 0.1;
                line_feature("101.000", json!([[x, 0.3], [x + 0.7, 1.1 * x]]))
            })
            .collect();
        let mut reversed = lines.clone();
        reversed.reverse();
        let c = compare_ok(&collection(lines), &collection(reversed), 1.0);
        assert!(!c.has_change());
    }

    #[test]
    fn too_many_samples_is_an_error() {
        let a = collection(vec![line_feature("101.000", json!([[0, 0], [1e9, 0]]))]);
        let e = compare(&a, &a, 1.0).unwrap_err();
        assert!(format!("{e:#}").contains("--tolerance"), "{e:#}");
    }
}
