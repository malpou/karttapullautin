//! GeoJSON output for vector features (contours, cliffs, knolls, vector-mapped
//! shapefile features, vegetation areas), plus the serialization contract generated from the JSON Schema.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use log::info;
use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Classification, Geometry, Point2};
use crate::io::fs::FileSystem;
use crate::plan::Rect;
use geojson_types::{FeatureGeometryType, FeatureProperties};

/// Rust types generated from `schema/geojson.schema.json` by `typify` in `build.rs`.
///
/// These types are the serialization contract for GeoJSON output properties.
/// Add new property classes to the schema file; `cargo build` regenerates this
/// module automatically.
#[allow(clippy::all)]
pub mod geojson_types {
    include!(concat!(env!("OUT_DIR"), "/geojson_types.rs"));
}

/// One GeoJSON vector output: its file-name suffix and whether it is also
/// carried by `merged.dxf.bin` (and thus skipped from the merged-GeoJSON route
/// in `export_combined` when that file exists).
///
/// Add new outputs here. Set `skip_when_merged_bin: true` if the feature also
/// rides `merged.dxf.bin`.
pub struct GeoJsonOutput {
    pub name: &'static str,
    pub skip_when_merged_bin: bool,
}

impl GeoJsonOutput {
    /// File name of this output in a temp folder: `<name>.geojson`.
    pub fn file_name(&self) -> String {
        format!("{}.geojson", self.name)
    }

    /// File name of one tile's cropped output in the batch output folder:
    /// `<tile>_<name>.geojson`.
    pub fn tile_file_name(&self, tile: &str) -> String {
        format!("{tile}_{}", self.file_name())
    }

    /// File name of the batch merge of every tile's output: `merged_<name>.geojson`.
    pub fn merged_file_name(&self) -> String {
        self.tile_file_name(MERGED_PREFIX)
    }
}

/// Prefix of the batch merge outputs in the batch output folder. Files carrying it are
/// merge outputs, never merge inputs.
pub const MERGED_PREFIX: &str = "merged";

/// The combined export's outputs in the batch output folder: every merged vector output
/// in one GeoJSON file and one DXF file, and the OCAD cross reference table that maps the
/// DXF layers (symbol codes) to OCAD symbols.
pub const COMBINED_GEOJSON: &str = "output.geojson";
pub const COMBINED_DXF: &str = "output.dxf";
pub const COMBINED_CRT: &str = "output.ocdCrt";

/// Contours (101, 102, and the depression and slope-line variants), from `out2.dxf.bin`.
pub const CONTOURS: GeoJsonOutput = GeoJsonOutput {
    name: "contours",
    skip_when_merged_bin: true,
};
/// The renderer's form lines (103), from `formlines.dxf.bin`.
pub const FORMLINES: GeoJsonOutput = GeoJsonOutput {
    name: "formlines",
    skip_when_merged_bin: true,
};
/// Knoll and small depression points (109, 111), from `dotknolls.dxf.bin`. Not skipped:
/// `merged.dxf.bin` carries them too, but its points are never read, so this file is
/// their only source.
pub const DOTKNOLLS: GeoJsonOutput = GeoJsonOutput {
    name: "dotknolls",
    skip_when_merged_bin: false,
};
/// Cliffs (201, 202), from `c2g.dxf.bin` and `c3g.dxf.bin` in one file.
pub const CLIFFS: GeoJsonOutput = GeoJsonOutput {
    name: "cliffs",
    skip_when_merged_bin: true,
};
/// Vegetation areas traced from the greenshade grid.
pub const VEGETATION: GeoJsonOutput = GeoJsonOutput {
    name: "vegetation",
    skip_when_merged_bin: false,
};
/// Open land areas (ISOM 403).
pub const OPEN_LAND: GeoJsonOutput = GeoJsonOutput {
    name: "yellow",
    skip_when_merged_bin: false,
};
/// Undergrowth areas.
pub const UNDERGROWTH: GeoJsonOutput = GeoJsonOutput {
    name: "undergrowth",
    skip_when_merged_bin: false,
};
/// Shapefile lines matched by a vector mapping rule.
pub const OSM_LINES: GeoJsonOutput = GeoJsonOutput {
    name: "osm_lines",
    skip_when_merged_bin: false,
};
/// Shapefile areas matched by a vector mapping rule.
pub const OSM_AREAS: GeoJsonOutput = GeoJsonOutput {
    name: "osm_areas",
    skip_when_merged_bin: false,
};

pub const GEOJSON_OUTPUTS: &[GeoJsonOutput] = &[
    CONTOURS,
    FORMLINES,
    DOTKNOLLS,
    CLIFFS,
    VEGETATION,
    OPEN_LAND,
    UNDERGROWTH,
    OSM_LINES,
    OSM_AREAS,
];

/// Legacy GeoJSON `crs` member for a projected EPSG code. RFC 7946 dropped `crs`, but
/// GIS tools still read it, and without it projected coordinates load misplaced.
/// None (no `epsg` config key) omits the member.
fn crs(epsg: Option<u32>) -> Option<geojson_types::Crs> {
    epsg.map(|code| geojson_types::Crs {
        properties: geojson_types::CrsProperties {
            name: format!("urn:ogc:def:crs:EPSG::{code}"),
        },
        type_: json!("name"),
    })
}

/// Round to cm to keep files small; sub-cm is noise at map scale.
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Build the coordinate array for one line/ring.
fn coords_line<I: IntoIterator<Item = [f64; 2]>>(pts: I) -> Vec<Value> {
    pts.into_iter()
        .map(|[x, y]| json!([r2(x), r2(y)]))
        .collect()
}

/// Typed GeoJSON properties for a terrain classification: the schema class (contour
/// family, knoll and small depression, or cliff) that owns its symbol code.
/// `elevation` is kept only for the contour family. None for a classification without
/// a symbol code (the knoll-detector artifact), which is left out of the output.
fn terrain_properties(
    c: Classification,
    elevation: Option<f64>,
) -> Option<geojson_types::FeatureProperties> {
    use geojson_types::{CliffProperties, ContourProperties, KnollProperties};

    let code = c.symbol_code()?;
    let symbol_name = c.symbol_name().map(String::from);
    let flag = |set: bool| set.then_some(true);
    // each arm lists exactly its schema enum, so the parse cannot fail
    Some(match code {
        "101" | "102" | "103" => ContourProperties {
            symbol: code.parse().unwrap(),
            symbol_name,
            elevation,
            depression: flag(c.is_depression_line()),
            slope_line: flag(c == Classification::SlopeLine),
        }
        .into(),
        "109" | "111" => KnollProperties {
            symbol: code.parse().unwrap(),
            symbol_name,
            ugly: flag(c.is_ugly()),
        }
        .into(),
        "201" | "202" => CliffProperties {
            symbol: code.parse().unwrap(),
            symbol_name,
        }
        .into(),
        _ => unreachable!("symbol code {code} has no terrain schema class"),
    })
}

fn feature(
    geometry: FeatureGeometryType,
    coordinates: Vec<Value>,
    properties: geojson_types::FeatureProperties,
) -> geojson_types::Feature {
    geojson_types::Feature {
        geometry: geojson_types::FeatureGeometry {
            coordinates,
            type_: geometry,
        },
        properties,
        type_: json!("Feature"),
    }
}

fn terrain_feature(
    geometry: FeatureGeometryType,
    coordinates: Vec<Value>,
    c: Classification,
    elevation: Option<f64>,
) -> Option<geojson_types::Feature> {
    Some(feature(
        geometry,
        coordinates,
        terrain_properties(c, elevation)?,
    ))
}

/// Typed GeoJSON properties of a shapefile record matched by a vector mapping rule:
/// its symbol code, its category (the mapping's name), and `upper_level` only when set.
fn osm_properties(
    symbol: &str,
    category: &str,
    upper_level: bool,
) -> geojson_types::FeatureProperties {
    geojson_types::OsmProperties {
        symbol: symbol.to_string(),
        category: category.to_string(),
        upper_level: upper_level.then_some(true),
    }
    .into()
}

/// LineString feature for one part of a shapefile polyline matched by a vector mapping rule.
pub fn osm_line(
    symbol: &str,
    category: &str,
    upper_level: bool,
    line: &[[f64; 2]],
) -> geojson_types::Feature {
    feature(
        FeatureGeometryType::LineString,
        coords_line(line.iter().copied()),
        osm_properties(symbol, category, upper_level),
    )
}

/// Polygon feature (exterior ring, then holes) for a shapefile polygon matched by a
/// vector mapping rule.
pub fn osm_area(
    symbol: &str,
    category: &str,
    upper_level: bool,
    rings: &[Vec<[f64; 2]>],
) -> geojson_types::Feature {
    feature(
        FeatureGeometryType::Polygon,
        rings
            .iter()
            .map(|ring| Value::Array(coords_line(ring.iter().copied())))
            .collect(),
        osm_properties(symbol, category, upper_level),
    )
}

/// Polygon feature (exterior ring, then holes) for one vegetation area. Rings are open
/// (first vertex not repeated); GeoJSON rings are closed here.
pub fn vegetation_area(
    symbol: geojson_types::VegetationPropertiesSymbol,
    rings: &[Vec<Point2>],
) -> geojson_types::Feature {
    feature(
        FeatureGeometryType::Polygon,
        rings
            .iter()
            .map(|ring| {
                let closed = ring.iter().chain(ring.first());
                Value::Array(coords_line(closed.map(|p| [p.x, p.y])))
            })
            .collect(),
        geojson_types::VegetationProperties {
            symbol,
            shade: None,
        }
        .into(),
    )
}

/// Write features as one GeoJSON FeatureCollection (with the legacy `crs` member when
/// an EPSG code is given).
pub fn write_feature_collection(
    fs: &impl FileSystem,
    output: &std::path::Path,
    features: Vec<geojson_types::Feature>,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    write_collection(
        fs,
        output,
        &geojson_types::GeoJsonOutput {
            crs: crs(epsg),
            features,
            type_: json!("FeatureCollection"),
        },
    )
}

fn write_collection(
    fs: &impl FileSystem,
    output: &Path,
    collection: &geojson_types::GeoJsonOutput,
) -> anyhow::Result<()> {
    let mut w = BufWriter::new(fs.create(output)?);
    serde_json::to_writer(&mut w, collection)?;
    w.flush()?;
    Ok(())
}

/// Read a FeatureCollection written by this module into the generated types.
fn read_collection(
    fs: &impl FileSystem,
    path: &Path,
) -> anyhow::Result<geojson_types::GeoJsonOutput> {
    Ok(serde_json::from_reader(BufReader::new(fs.open(path)?))?)
}

/// Convert one or more binary DXF files (contours, cliffs, knolls...) into a single
/// GeoJSON FeatureCollection. Polylines become LineStrings and points become Points,
/// each with the properties of its classification (see [`terrain_properties`]).
///
/// Property schema: see `schema/geojson.schema.json` ($defs/ContourProperties,
/// KnollProperties, CliffProperties).
pub fn bindxf_to_geojson(
    fs: &impl FileSystem,
    inputs: &[std::path::PathBuf],
    output: &std::path::Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut features = Vec::new();
    for input in inputs {
        let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
        for geom in dxf.take_geometry() {
            match geom {
                Geometry::Polylines2(pl) => {
                    features.extend(pl.into_iter().filter_map(|(p, c)| {
                        let coords = coords_line(p.iter().map(|pt| [pt.x, pt.y]));
                        terrain_feature(FeatureGeometryType::LineString, coords, c, None)
                    }));
                }
                Geometry::Polylines3(pl) => {
                    features.extend(pl.into_iter().filter_map(|(p, (c, h))| {
                        let coords = coords_line(p.iter().map(|pt| [pt.x, pt.y]));
                        terrain_feature(FeatureGeometryType::LineString, coords, c, Some(h))
                    }));
                }
                Geometry::Points(pts) => {
                    features.extend(pts.into_iter().filter_map(|(p, c)| {
                        let coords = vec![json!(r2(p.x)), json!(r2(p.y))];
                        terrain_feature(FeatureGeometryType::Point, coords, c, None)
                    }));
                }
            }
        }
    }
    write_feature_collection(fs, output, features, epsg)
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

// The ISOM 2017-2 contour and point-symbol rules below are applied by the combined
// export, after the per-tile outputs are merged: the knolls that survive spacing are
// only known then, across tile edges.

/// ISOM 2017-2 minimum dimensions for contours, in ground metres. The standard specifies
/// them on the 1:15,000 original, so ground metres = mm x 15: the smallest bend that can
/// be drawn is 0.25 mm centre to centre (3.75 m) and the mouth of a re-entrant or spur
/// must be wider than 0.5 mm (7.5 m). The wider bound subsumes the narrower one, so a
/// single pass at 8 m enforces both.
const MIN_MOUTH_M: f64 = 8.0;

/// A bound on how much line one splice may consume. Nothing removed can depart further
/// than MIN_MOUTH_M from the join that replaces it, so this only stops a long
/// near-parallel double-back from being swallowed in a single cut.
const MAX_DETOUR_M: f64 = 24.0;

/// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (vx, vy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = vx * vx + vy * vy;
    if len2 == 0.0 {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * vx + (p[1] - a[1]) * vy) / len2).clamp(0.0, 1.0);
    dist(p, [a[0] + t * vx, a[1] + t * vy])
}

/// Splice out excursions that leave and return within MIN_MOUTH_M *and* never depart
/// further than MIN_MOUTH_M from the join replacing them: the wobbles ISOM 2017-2 means
/// by "small details on contours should be avoided because they tend to hide the main
/// features of the terrain".
///
/// Both bounds matter. The first alone would let a narrow re-entrant be truncated at any
/// neck along its length; together they guarantee nothing is removed that reaches beyond
/// what the symbol's own minimum dimension can carry. A closed ring is protected from
/// being consumed whole by requiring the kept remainder to stay above the same bound.
fn generalise_contour(pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if pts.len() < 4 {
        return pts.to_vec();
    }
    let mut cum = Vec::with_capacity(pts.len());
    cum.push(0.0);
    for w in pts.windows(2) {
        cum.push(cum[cum.len() - 1] + dist(w[0], w[1]));
    }
    let total = cum[cum.len() - 1];
    let mut out = Vec::with_capacity(pts.len());
    let mut i = 0;
    while i < pts.len() {
        out.push(pts[i]);
        // the furthest vertex that comes back within the minimum mouth on a short detour
        let mut jump = None;
        let mut j = i + 1;
        while j < pts.len() && cum[j] - cum[i] <= MAX_DETOUR_M {
            let along = cum[j] - cum[i];
            if along > MIN_MOUTH_M
                && along < total - MIN_MOUTH_M
                && dist(pts[i], pts[j]) < MIN_MOUTH_M
                && pts[i + 1..j]
                    .iter()
                    .all(|p| seg_dist(*p, pts[i], pts[j]) < MIN_MOUTH_M)
            {
                jump = Some(j);
            }
            j += 1;
        }
        i = jump.unwrap_or(i + 1);
    }
    out
}

/// ISOM 2017-2 requires that symbol 109/110 "shall not touch or overlap contours", and
/// that "contours shall be adapted or broken in order not to touch" them. The knoll's
/// position is the whole information the symbol carries, so the contour is the side that
/// gives way. 109 is a 0.4 mm dot on the 1:15,000 original (a 6 m footprint, 3 m radius)
/// plus half a contour width of air.
const KNOLL_CLEAR_M: f64 = 3.5;

/// Break a contour into the pieces that stay clear of the knoll symbols, dropping any
/// piece too short to be a line.
///
/// Cuts at vertices rather than interpolating the exact crossing point. Contour vertices
/// are ~1.2 m apart, well inside the clearance, so the gap is right to within a vertex.
fn break_at_knolls(pts: &[[f64; 2]], knolls: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    if knolls.is_empty() {
        return vec![pts.to_vec()];
    }
    let mut parts = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for p in pts {
        if knolls.iter().any(|k| dist(*k, *p) < KNOLL_CLEAR_M) {
            if cur.len() > 1 {
                parts.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        } else {
            cur.push(*p);
        }
    }
    if cur.len() > 1 {
        parts.push(cur);
    }
    parts
}

/// True for the contour family (101 contour, 102 index, 103 form line): the symbols the
/// ISOM contour rules above apply to.
fn is_contour_family(symbol: &str) -> bool {
    matches!(symbol, "101" | "102" | "103")
}

/// Apply the ISOM contour rules to one published line: generalise detail below what the
/// symbol can carry, then break where a knoll symbol needs room. Anything that is not a
/// contour passes through as a single piece, untouched.
fn conform_contour(symbol: &str, pts: &[[f64; 2]], knolls: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    if !is_contour_family(symbol) {
        return vec![pts.to_vec()];
    }
    break_at_knolls(&generalise_contour(pts), knolls)
}

/// ISOM 109/110/111 point symbols must not touch or overlap each other either (12 m
/// footprint length).
const POINT_MIN_SPACING_M: f64 = 12.0;

/// The knoll and small depression point symbols that survive to the map: the Point
/// features of a dot knolls GeoJSON file (see [`DOTKNOLLS`]) through a greedy spacing
/// filter ranked by certainty, so the detector's definite symbols win over the `ugly`
/// ones when two candidates are closer than the minimum. A missing file has none.
pub(crate) fn published_knolls(
    fs: &impl FileSystem,
    path: &std::path::Path,
) -> anyhow::Result<Vec<([f64; 2], geojson_types::KnollProperties)>> {
    if !fs.exists(path) {
        return Ok(Vec::new());
    }
    let candidates = read_collection(fs, path)?
        .features
        .into_iter()
        .filter_map(|f| {
            let FeatureProperties::KnollProperties(props) = f.properties else {
                return None;
            };
            let c = &f.geometry.coordinates;
            let point = f.geometry.type_ == FeatureGeometryType::Point;
            match (point, c.first()?.as_f64(), c.get(1)?.as_f64()) {
                (true, Some(x), Some(y)) => Some(([x, y], props)),
                _ => None,
            }
        })
        .collect();
    Ok(space_knolls(candidates))
}

/// The greedy spacing filter of [`published_knolls`], definite before `ugly`.
fn space_knolls(
    mut candidates: Vec<([f64; 2], geojson_types::KnollProperties)>,
) -> Vec<([f64; 2], geojson_types::KnollProperties)> {
    // stable: definite first, each group in input order
    candidates.sort_by_key(|(_, props)| props.ugly == Some(true));
    let mut kept: Vec<([f64; 2], geojson_types::KnollProperties)> = Vec::new();
    for (p, props) in candidates {
        if kept.iter().all(|(k, _)| dist(*k, p) >= POINT_MIN_SPACING_M) {
            kept.push((p, props));
        }
    }
    kept
}

// Batch mode: each tile's GeoJSON is cropped to the tile into the batch output folder,
// the tiles are merged per output, and the combined export publishes every merged
// output as one GeoJSON file and one DXF file.

/// The symbol code a feature is drawn with.
fn symbol(props: &FeatureProperties) -> String {
    match props {
        FeatureProperties::ContourProperties(p) => p.symbol.to_string(),
        FeatureProperties::KnollProperties(p) => p.symbol.to_string(),
        FeatureProperties::CliffProperties(p) => p.symbol.to_string(),
        FeatureProperties::VegetationProperties(p) => p.symbol.to_string(),
        FeatureProperties::OsmProperties(p) => p.symbol.clone(),
    }
}

/// The vertices of one line or ring (`[[x, y], ...]`).
fn line_points(coords: &[Value]) -> Vec<[f64; 2]> {
    coords
        .iter()
        .filter_map(|p| Some([p.get(0)?.as_f64()?, p.get(1)?.as_f64()?]))
        .collect()
}

/// The rings of a Polygon, exterior first.
fn polygon_rings(coords: &[Value]) -> Vec<Vec<[f64; 2]>> {
    coords
        .iter()
        .map(|ring| ring.as_array().map(|r| line_points(r)).unwrap_or_default())
        .collect()
}

/// Liang-Barsky clip of one segment against the bbox; None when fully outside.
fn clip_seg(a: [f64; 2], b: [f64; 2], bbox: &Rect) -> Option<([f64; 2], [f64; 2])> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-dx, a[0] - bbox.minx),
        (dx, bbox.maxx - a[0]),
        (-dy, a[1] - bbox.miny),
        (dy, bbox.maxy - a[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    Some((
        [a[0] + t0 * dx, a[1] + t0 * dy],
        [a[0] + t1 * dx, a[1] + t1 * dy],
    ))
}

/// Clip one line to the bbox with per-segment intersection (handles sparse vertices),
/// splitting it where it leaves the box.
fn clip_line(pts: &[[f64; 2]], bbox: &Rect) -> Vec<Vec<[f64; 2]>> {
    let mut out = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for w in pts.windows(2) {
        if let Some((a, b)) = clip_seg(w[0], w[1], bbox) {
            let contiguous = cur
                .last()
                .is_some_and(|l| (l[0] - a[0]).abs() < 1e-9 && (l[1] - a[1]).abs() < 1e-9);
            if !contiguous {
                if cur.len() > 1 {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
                cur.push(a);
            }
            cur.push(b);
        } else if cur.len() > 1 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
    }
    if cur.len() > 1 {
        out.push(cur);
    }
    out
}

/// Sutherland-Hodgman clip of a closed ring against the bbox. Returns an empty vec when
/// the ring is entirely outside; otherwise a closed ring (first point repeated last).
fn clip_ring(ring: &[[f64; 2]], bbox: &Rect) -> Vec<[f64; 2]> {
    let mut pts: Vec<[f64; 2]> = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    for edge in 0..4 {
        let inside = |p: &[f64; 2]| match edge {
            0 => p[0] >= bbox.minx,
            1 => p[0] <= bbox.maxx,
            2 => p[1] >= bbox.miny,
            _ => p[1] <= bbox.maxy,
        };
        let intersect = |a: &[f64; 2], b: &[f64; 2]| -> [f64; 2] {
            match edge {
                0 | 1 => {
                    let x = if edge == 0 { bbox.minx } else { bbox.maxx };
                    let t = (x - a[0]) / (b[0] - a[0]);
                    [x, a[1] + t * (b[1] - a[1])]
                }
                _ => {
                    let y = if edge == 2 { bbox.miny } else { bbox.maxy };
                    let t = (y - a[1]) / (b[1] - a[1]);
                    [a[0] + t * (b[0] - a[0]), y]
                }
            }
        };
        let input = std::mem::take(&mut pts);
        if input.is_empty() {
            return vec![];
        }
        for i in 0..input.len() {
            let cur = input[i];
            let prev = input[(i + input.len() - 1) % input.len()];
            match (inside(&prev), inside(&cur)) {
                (true, true) => pts.push(cur),
                (false, true) => {
                    pts.push(intersect(&prev, &cur));
                    pts.push(cur);
                }
                (true, false) => pts.push(intersect(&prev, &cur)),
                (false, false) => {}
            }
        }
    }
    if pts.len() < 3 {
        return vec![];
    }
    pts.push(pts[0]);
    pts
}

/// Clip a Polygon's rings (exterior first). Drops the whole polygon when the exterior
/// vanishes; drops holes that vanish.
fn clip_polygon(coords: &[Value], bbox: &Rect) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for (i, ring) in polygon_rings(coords).iter().enumerate() {
        let clipped = clip_ring(ring, bbox);
        if clipped.is_empty() {
            if i == 0 {
                return None;
            }
            continue;
        }
        out.push(Value::Array(coords_line(clipped)));
    }
    Some(out)
}

/// Crop every feature of a GeoJSON file to the bbox (a batch tile without its padding)
/// and write the result, carrying the input's `crs` over. A line that leaves the box
/// and comes back becomes one LineString feature per part, each with the line's
/// properties.
pub fn crop_geojson(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    bbox: &Rect,
) -> anyhow::Result<()> {
    let mut collection = read_collection(fs, input)?;
    let mut features = Vec::new();
    for f in std::mem::take(&mut collection.features) {
        let coords = &f.geometry.coordinates;
        match f.geometry.type_ {
            FeatureGeometryType::LineString => {
                for part in clip_line(&line_points(coords), bbox) {
                    features.push(feature(
                        FeatureGeometryType::LineString,
                        coords_line(part),
                        f.properties.clone(),
                    ));
                }
            }
            FeatureGeometryType::Polygon => {
                if let Some(rings) = clip_polygon(coords, bbox) {
                    features.push(feature(FeatureGeometryType::Polygon, rings, f.properties));
                }
            }
            FeatureGeometryType::Point => {
                // half-open, so a point on the edge two tiles share lands in one of them
                let inside = matches!(
                    (coords.first().and_then(Value::as_f64), coords.get(1).and_then(Value::as_f64)),
                    (Some(x), Some(y)) if (bbox.minx..bbox.maxx).contains(&x)
                        && (bbox.miny..bbox.maxy).contains(&y)
                );
                if inside {
                    features.push(f);
                }
            }
        }
    }
    collection.features = features;
    write_collection(fs, output, &collection)
}

/// Merge the per-tile `<tile>_<name>.geojson` files in the batch output folder into
/// `merged_<name>.geojson`, for every output in [`GEOJSON_OUTPUTS`]. Tiles are taken in
/// file name order; the first tile's `crs` is carried over.
pub fn merge_geojson(fs: &impl FileSystem, batchoutfolder: &Path) -> anyhow::Result<()> {
    for output in GEOJSON_OUTPUTS {
        let suffix = output.tile_file_name("");
        let merged_name = output.merged_file_name();
        let mut files: Vec<_> = fs
            .list(batchoutfolder)?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f.ends_with(&suffix) && f != merged_name)
            })
            .collect();
        if files.is_empty() {
            info!("No files found for suffix {}, skipping...", output.name);
            continue;
        }
        files.sort();

        let mut merged = read_collection(fs, &files[0])?;
        for file in &files[1..] {
            merged.features.extend(read_collection(fs, file)?.features);
        }
        write_collection(fs, &batchoutfolder.join(merged_name), &merged)?;
    }
    Ok(())
}

/// Symbols whose geometry is organic and gets Bezier curves in the combined DXF (and the
/// same curve, sampled, in the combined GeoJSON). Roads and buildings stay straight.
fn curve_symbol(symbol: &str) -> bool {
    is_contour_family(symbol)
        || matches!(
            symbol,
            "201" | "202" | "306" | "403" | "406" | "407" | "408" | "410"
        )
}

/// Fit a piecewise cubic Bezier through the (thinned) polyline with Catmull-Rom
/// tangents (factor 0.5, like OCAD's own converter default). Returns the control
/// points (3 per segment + the final endpoint), or None when too short for a curve.
fn fit_bezier(pts: &[[f64; 2]], closed: bool) -> Option<Vec<[f64; 2]>> {
    // thin the dense smoothed polyline first so the curve has few, meaningful vertices
    let as_p2: Vec<Point2> = pts.iter().map(|q| Point2::new(q[0], q[1])).collect();
    let thin = if closed && as_p2.len() > 4 {
        crate::vege_vector::simplify_closed(as_p2, 1.0)
    } else {
        crate::vege_vector::dp(&as_p2, 1.0)
    };
    let mut p: Vec<[f64; 2]> = thin.iter().map(|q| [q.x, q.y]).collect();
    if closed
        && p.first() != p.last()
        && let Some(f) = p.first().copied()
    {
        p.push(f);
    }
    let n = p.len();
    if n < 3 {
        return None;
    }

    // Catmull-Rom tangent at vertex i (closed: wrapped, open: one-sided at the ends)
    let tangent = |i: usize| -> [f64; 2] {
        let (prev, next) = if closed {
            // last point duplicates the first: wrap over n-1 distinct points
            let m = n - 1;
            (p[(i + m - 1) % m], p[(i + 1) % m])
        } else if i == 0 {
            (p[0], p[1])
        } else if i == n - 1 {
            (p[n - 2], p[n - 1])
        } else {
            (p[i - 1], p[i + 1])
        };
        [(next[0] - prev[0]) * 0.5, (next[1] - prev[1]) * 0.5]
    };

    let segs = n - 1;
    let mut ctrl: Vec<[f64; 2]> = Vec::with_capacity(3 * segs + 1);
    for i in 0..segs {
        let (t0, t1) = (tangent(i), tangent(i + 1));
        ctrl.push(p[i]);
        ctrl.push([p[i][0] + t0[0] / 3.0, p[i][1] + t0[1] / 3.0]);
        ctrl.push([p[i + 1][0] - t1[0] / 3.0, p[i + 1][1] - t1[1] / 3.0]);
    }
    ctrl.push(p[n - 1]);
    Some(ctrl)
}

/// The polyline a GeoJSON feature gets for a curve symbol: the same fitted Bezier the
/// DXF SPLINE uses, densely sampled (GeoJSON has no curve geometry). Other symbols and
/// too-short lines pass through unchanged.
fn curve_points(symbol: &str, pts: &[[f64; 2]], closed: bool) -> Vec<[f64; 2]> {
    if !(curve_symbol(symbol) && pts.len() > 3) {
        return pts.to_vec();
    }
    let Some(ctrl) = fit_bezier(pts, closed) else {
        return pts.to_vec();
    };
    const SAMPLES: usize = 8; // per Bezier segment; segments are >= 1 m after thinning
    let segs = (ctrl.len() - 1) / 3;
    let mut out = Vec::with_capacity(segs * SAMPLES + 1);
    out.push(ctrl[0]);
    for s in 0..segs {
        let c = &ctrl[3 * s..3 * s + 4];
        for k in 1..=SAMPLES {
            let t = k as f64 / SAMPLES as f64;
            let u = 1.0 - t;
            let (b0, b1, b2, b3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            out.push([
                b0 * c[0][0] + b1 * c[1][0] + b2 * c[2][0] + b3 * c[3][0],
                b0 * c[0][1] + b1 * c[1][1] + b2 * c[2][1] + b3 * c[3][1],
            ]);
        }
    }
    out
}

/// The pieces of one line as published: ISOM-conformed, curve-sampled, then re-checked
/// against the knolls, since fitting a curve through a broken end bows it back over the
/// very symbol the break was made for (measured: 2.95 m from a symbol of 3 m radius).
fn published_pieces(
    symbol: &str,
    pts: &[[f64; 2]],
    closed: bool,
    knolls: &[[f64; 2]],
) -> Vec<Vec<[f64; 2]>> {
    conform_contour(symbol, pts, knolls)
        .into_iter()
        .flat_map(|piece| {
            let still_closed = closed && piece.first() == piece.last();
            let sampled = curve_points(symbol, &piece, still_closed);
            if is_contour_family(symbol) {
                break_at_knolls(&sampled, knolls)
            } else {
                vec![sampled]
            }
        })
        .collect()
}

/// One Chaikin pass takes the segment jitter out of a contour-family line before the
/// ISOM rules and the curve fit; other symbols pass through.
fn smoothed(symbol: &str, pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if !(is_contour_family(symbol) && pts.len() > 3) {
        return pts.to_vec();
    }
    let as_p2: Vec<Point2> = pts.iter().map(|q| Point2::new(q[0], q[1])).collect();
    let sm = if pts.first() == pts.last() {
        let mut r = crate::vege_vector::chaikin_closed(&as_p2[..as_p2.len() - 1]);
        if let Some(f) = r.first().cloned() {
            r.push(f);
        }
        r
    } else {
        crate::vege_vector::chaikin_open(&as_p2)
    };
    sm.iter().map(|q| [q.x, q.y]).collect()
}

/// ISOM 202: minimum cliff length 0.6 mm => 9 m footprint at 1:15,000 (applied to 201
/// as well). Shorter detector fragments are noise, not mappable cliffs.
const CLIFF_MIN_LEN_M: f64 = 9.0;
/// KP emits one ~3 m dash per detected steep cell; dashes within this distance belong
/// to the same cliff face.
const CLIFF_CLUSTER_DIST: f64 = 3.0;

/// Chain KP's per-cell cliff dashes into cliff lines: cluster dash midpoints within
/// CLIFF_CLUSTER_DIST, order each cluster as a greedy nearest-neighbour path from an
/// extreme point refined with 2-opt (untangles the crossings greedy ordering leaves on
/// sharply curved faces), and drop chains shorter than the ISOM minimum. Chains come
/// out in a fixed order (by cluster root), so the export is deterministic.
fn chain_cliff_dashes(mids: &[[f64; 2]]) -> Vec<Vec<[f64; 2]>> {
    // union-find over a coarse grid
    let mut parent: Vec<usize> = (0..mids.len()).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            let r = find(parent, parent[i]);
            parent[i] = r;
        }
        parent[i]
    }
    let cell = CLIFF_CLUSTER_DIST;
    let key = |m: &[f64; 2]| ((m[0] / cell) as i64, (m[1] / cell) as i64);
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, m) in mids.iter().enumerate() {
        grid.entry(key(m)).or_default().push(i);
    }
    for (i, m) in mids.iter().enumerate() {
        let (gx, gy) = key(m);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &j in grid.get(&(gx + dx, gy + dy)).into_iter().flatten() {
                    if j > i && dist(*m, mids[j]) <= CLIFF_CLUSTER_DIST {
                        let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                        if ri != rj {
                            parent[ri] = rj;
                        }
                    }
                }
            }
        }
    }
    let mut clusters: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..mids.len() {
        let r = find(&mut parent, i);
        clusters.entry(r).or_default().push(i);
    }

    let mut chains = Vec::new();
    for members in clusters.values() {
        // start from the point farthest from the cluster centroid
        let n = members.len() as f64;
        let cx = members.iter().map(|&i| mids[i][0]).sum::<f64>() / n;
        let cy = members.iter().map(|&i| mids[i][1]).sum::<f64>() / n;
        let mut rest: Vec<usize> = members.clone();
        let start_pos = rest
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                dist(mids[**a], [cx, cy]).total_cmp(&dist(mids[**b], [cx, cy]))
            })
            .map(|(p, _)| p)
            .unwrap();
        let mut path = vec![mids[rest.swap_remove(start_pos)]];
        while !rest.is_empty() {
            let last = *path.last().unwrap();
            let next_pos = rest
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| dist(mids[**a], last).total_cmp(&dist(mids[**b], last)))
                .map(|(p, _)| p)
                .unwrap();
            path.push(mids[rest.swap_remove(next_pos)]);
        }
        // 2-opt on the open path: reversing path[i+1..=j] swaps edges (i,i+1)/(j,j+1)
        // for (i,j)/(i+1,j+1); when j is the last point only edge (i,i+1) is replaced.
        // Clusters are a handful of dashes, so O(n²) passes are cheap.
        let mut improved = true;
        while improved {
            improved = false;
            let n = path.len();
            for i in 0..n.saturating_sub(2) {
                for j in i + 2..n {
                    let (removed, added) = if j + 1 < n {
                        (
                            dist(path[i], path[i + 1]) + dist(path[j], path[j + 1]),
                            dist(path[i], path[j]) + dist(path[i + 1], path[j + 1]),
                        )
                    } else {
                        (dist(path[i], path[i + 1]), dist(path[i], path[j]))
                    };
                    if added + 1e-9 < removed {
                        path[i + 1..=j].reverse();
                        improved = true;
                    }
                }
            }
        }
        let len: f64 = path.windows(2).map(|w| dist(w[0], w[1])).sum();
        // single-dash clusters have zero path length; use the dash length itself
        if len.max(2.9) >= CLIFF_MIN_LEN_M {
            chains.push(path);
        }
    }
    chains
}

/// OCAD symbol number for a DXF layer (a symbol code) in the cross reference table:
/// `NNN.000`. None for a layer that is not a plain symbol number.
fn crt_symbol(symbol: &str) -> Option<String> {
    (!symbol.is_empty() && symbol.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{symbol}.000"))
}

/// Write one SPLINE entity from the fitted piecewise cubic Bezier (see [`fit_bezier`]).
fn dxf_spline(out: &mut String, layer: &str, pts: &[[f64; 2]], closed: bool) {
    use std::fmt::Write as _;

    let Some(ctrl) = fit_bezier(pts, closed) else {
        dxf_polyline(out, layer, pts, closed, None);
        return;
    };
    let segs = (ctrl.len() - 1) / 3;
    // clamped knot vector for piecewise Bezier: 0 x4, 1 x3, ..., segs x4
    let nctrl = ctrl.len();
    let nknots = nctrl + 4;
    let _ = write!(
        out,
        "SPLINE\r\n  8\r\n{layer}\r\n 70\r\n8\r\n 71\r\n3\r\n 72\r\n{nknots}\r\n 73\r\n{nctrl}\r\n 74\r\n0\r\n"
    );
    for k in 0..=segs {
        let reps = if k == 0 || k == segs { 4 } else { 3 };
        for _ in 0..reps {
            let _ = write!(out, " 40\r\n{k}\r\n");
        }
    }
    for c in &ctrl {
        let _ = write!(out, " 10\r\n{}\r\n 20\r\n{}\r\n 30\r\n0\r\n", c[0], c[1]);
    }
    out.push_str("  0\r\n");
}

/// Write one POLYLINE entity in the same format `BinaryDxf::to_dxf` uses.
fn dxf_polyline(out: &mut String, layer: &str, pts: &[[f64; 2]], closed: bool, elev: Option<f64>) {
    use std::fmt::Write as _;
    out.push_str("POLYLINE\r\n 66\r\n1\r\n  8\r\n");
    out.push_str(layer);
    if let Some(h) = elev {
        let _ = write!(out, "\r\n 38\r\n{h}");
    }
    if closed {
        out.push_str("\r\n 70\r\n1");
    }
    out.push_str("\r\n  0\r\n");
    for p in pts {
        let _ = write!(
            out,
            "VERTEX\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\n",
            p[0], p[1]
        );
    }
    out.push_str("SEQEND\r\n  0\r\n");
}

/// The combined export being assembled: DXF entities (layer = symbol code), GeoJSON
/// features, the layers used and the extent.
struct Combined {
    dxf: String,
    features: Vec<geojson_types::Feature>,
    layers: BTreeSet<String>,
    min: [f64; 2],
    max: [f64; 2],
}

impl Combined {
    fn new() -> Self {
        Self {
            dxf: String::new(),
            features: Vec::new(),
            layers: BTreeSet::new(),
            min: [f64::MAX; 2],
            max: [f64::MIN; 2],
        }
    }

    fn grow(&mut self, pts: &[[f64; 2]]) {
        for p in pts {
            for i in 0..2 {
                self.min[i] = self.min[i].min(p[i]);
                self.max[i] = self.max[i].max(p[i]);
            }
        }
    }

    /// One DXF entity: SPLINE for curve symbols, POLYLINE otherwise.
    fn dxf_entity(&mut self, symbol: &str, pts: &[[f64; 2]], closed: bool, elev: Option<f64>) {
        self.grow(pts);
        if curve_symbol(symbol) && pts.len() > 3 {
            dxf_spline(&mut self.dxf, symbol, pts, closed);
        } else {
            dxf_polyline(&mut self.dxf, symbol, pts, closed, elev);
        }
        self.layers.insert(symbol.to_string());
    }

    /// A line through the ISOM rules: each published piece becomes a DXF entity and a
    /// LineString feature with the line's properties. A line that ends where it starts
    /// (a contour loop) stays closed until a knoll breaks it.
    fn line(&mut self, pts: &[[f64; 2]], props: &FeatureProperties, knolls: &[[f64; 2]]) {
        let symbol = symbol(props);
        let elevation = match props {
            FeatureProperties::ContourProperties(p) => p.elevation,
            _ => None,
        };
        let pts = smoothed(&symbol, pts);
        let closed = pts.len() > 3 && pts.first() == pts.last();
        for piece in published_pieces(&symbol, &pts, closed, knolls) {
            let closed = closed && piece.first() == piece.last();
            self.dxf_entity(&symbol, &piece, closed, elevation);
            self.features.push(feature(
                FeatureGeometryType::LineString,
                coords_line(piece),
                props.clone(),
            ));
        }
    }

    /// An area: each ring (curve-sampled for curve symbols) a closed DXF entity, all of
    /// them one Polygon feature.
    fn polygon(&mut self, rings: &[Vec<[f64; 2]>], props: FeatureProperties, knolls: &[[f64; 2]]) {
        let symbol = symbol(&props);
        let mut coords = Vec::new();
        for ring in rings {
            for piece in published_pieces(&symbol, ring, true, knolls) {
                self.dxf_entity(&symbol, &piece, true, None);
                coords.push(Value::Array(coords_line(piece)));
            }
        }
        if !coords.is_empty() {
            self.features
                .push(feature(FeatureGeometryType::Polygon, coords, props));
        }
    }

    fn point(&mut self, p: [f64; 2], props: FeatureProperties) {
        use std::fmt::Write as _;
        let symbol = symbol(&props);
        self.grow(&[p]);
        let _ = write!(
            self.dxf,
            "POINT\r\n  8\r\n{symbol}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 50\r\n0\r\n  0\r\n",
            p[0], p[1]
        );
        self.features.push(feature(
            FeatureGeometryType::Point,
            vec![json!(r2(p[0])), json!(r2(p[1]))],
            props,
        ));
        self.layers.insert(symbol);
    }

    /// A cliff line chained from dashes: a DXF entity and the sampled curve as a
    /// LineString feature.
    fn cliff(&mut self, chain: &[[f64; 2]], props: FeatureProperties) {
        let symbol = symbol(&props);
        self.dxf_entity(&symbol, chain, false, None);
        self.features.push(feature(
            FeatureGeometryType::LineString,
            coords_line(curve_points(&symbol, chain, false)),
            props,
        ));
    }
}

/// Combine every merged vector output in the batch output folder into [`COMBINED_GEOJSON`]
/// and [`COMBINED_DXF`] (layer names = symbol codes), plus [`COMBINED_CRT`], the cross
/// reference table for OCAD's "Import DXF" layer-to-symbol conversion.
///
/// Sources: `merged_bin` (the batch `merged.dxf.bin`, which exists when the tiles kept
/// their `.dxf.bin` files, `savetempfiles=1`) for the outputs flagged
/// `skip_when_merged_bin`, and the `merged_<name>.geojson` files for every other output
/// (and for all of them when `merged_bin` is missing). On the way:
/// - knoll and small depression points: spacing-filtered by [`published_knolls`];
/// - contours and form lines: one Chaikin pass, ISOM generalisation, broken around the
///   published knolls, and the half-interval contours left out (as `merged.dxf.bin`
///   leaves them out; 103 is the renderer's form lines);
/// - cliffs: KP's per-cell dashes chained into cliff lines, too-short faces dropped;
/// - curve symbols get Bezier SPLINEs in the DXF and the same curve, sampled, in the
///   GeoJSON.
///
/// Writes nothing when there is nothing to export.
pub fn export_combined(
    fs: &impl FileSystem,
    batchoutfolder: &Path,
    merged_bin: &Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    use geojson_types::{ContourPropertiesSymbol, FeatureProperties as P};

    let have_merged_bin = fs.exists(merged_bin);
    let mut bin_lines: Vec<(Vec<[f64; 2]>, Classification, Option<f64>)> = Vec::new();
    let mut bin_knolls = Vec::new();
    if have_merged_bin {
        let dxf = BinaryDxf::from_reader(&mut fs.open(merged_bin)?)?;
        for geom in dxf.take_geometry() {
            match geom {
                Geometry::Polylines2(pl) => bin_lines.extend(
                    pl.into_iter()
                        .map(|(p, c)| (p.iter().map(|q| [q.x, q.y]).collect(), c, None)),
                ),
                Geometry::Polylines3(pl) => bin_lines.extend(
                    pl.into_iter()
                        .map(|(p, (c, h))| (p.iter().map(|q| [q.x, q.y]).collect(), c, Some(h))),
                ),
                Geometry::Points(pts) => {
                    bin_knolls.extend(pts.into_iter().filter_map(
                        |(p, c)| match terrain_properties(c, None)? {
                            P::KnollProperties(props) => Some(([p.x, p.y], props)),
                            _ => None,
                        },
                    ))
                }
            }
        }
    }

    // The point symbols are settled first: ISOM makes the contours give way to them.
    // The merged dot knolls GeoJSON is their source; merged.dxf.bin carries the same
    // points and stands in only without it (vector_vege=0).
    let dotknolls = batchoutfolder.join(DOTKNOLLS.merged_file_name());
    let knolls = if fs.exists(&dotknolls) {
        published_knolls(fs, &dotknolls)?
    } else {
        space_knolls(bin_knolls)
    };
    let knoll_pts: Vec<[f64; 2]> = knolls.iter().map(|(p, _)| *p).collect();

    let mut out = Combined::new();
    // cliff dash midpoints per cliff symbol, chained into cliff lines below
    let mut cliffs: BTreeMap<_, (geojson_types::CliffProperties, Vec<[f64; 2]>)> = BTreeMap::new();
    let mut add_dash = |props: geojson_types::CliffProperties, pts: &[[f64; 2]]| {
        if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
            let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
            cliffs
                .entry(props.symbol)
                .or_insert_with(|| (props, Vec::new()))
                .1
                .push(mid);
        }
    };

    for (pts, c, elevation) in bin_lines {
        match terrain_properties(c, elevation) {
            None => {} // the knoll-detector artifact
            Some(P::CliffProperties(p)) => add_dash(p, &pts),
            Some(props) => out.line(&pts, &props, &knoll_pts),
        }
    }

    for output in GEOJSON_OUTPUTS {
        if output.skip_when_merged_bin && have_merged_bin {
            continue; // taken from merged.dxf.bin above
        }
        let path = batchoutfolder.join(output.merged_file_name());
        if !fs.exists(&path) {
            continue;
        }
        // the half-interval contours: ISOM has no symbol for them, and merged.dxf.bin
        // leaves them out too (103 is the renderer's form lines, from FORMLINES)
        let half_interval = |p: &P| {
            output.name == CONTOURS.name
                && matches!(p, P::ContourProperties(c) if c.symbol == ContourPropertiesSymbol::X103)
        };
        for f in read_collection(fs, &path)?.features {
            let coords = &f.geometry.coordinates;
            match (f.geometry.type_, f.properties) {
                // the knoll points, spacing-filtered, are published below
                (FeatureGeometryType::Point, _) => {}
                (FeatureGeometryType::LineString, P::CliffProperties(p)) => {
                    add_dash(p, &line_points(coords))
                }
                (FeatureGeometryType::LineString, props) if half_interval(&props) => {}
                (FeatureGeometryType::LineString, props) => {
                    out.line(&line_points(coords), &props, &knoll_pts)
                }
                (FeatureGeometryType::Polygon, props) => {
                    out.polygon(&polygon_rings(coords), props, &knoll_pts)
                }
            }
        }
    }

    for (p, props) in knolls {
        out.point(p, props.into());
    }

    for (props, mids) in cliffs.into_values() {
        for chain in chain_cliff_dashes(&mids) {
            out.cliff(&chain, props.clone().into());
        }
    }

    if out.dxf.is_empty() {
        info!("No vector outputs found, skipping the combined export");
        return Ok(());
    }

    write_feature_collection(
        fs,
        &batchoutfolder.join(COMBINED_GEOJSON),
        out.features,
        epsg,
    )?;

    // $ACADVER is required for SPLINE entities
    let ([xmin, ymin], [xmax, ymax]) = (out.min, out.max);
    let mut w = BufWriter::new(fs.create(batchoutfolder.join(COMBINED_DXF))?);
    write!(
        w,
        "  0\r\nSECTION\r\n  2\r\nHEADER\r\n  9\r\n$ACADVER\r\n  1\r\nAC1015\r\n  9\r\n$EXTMIN\r\n 10\r\n{xmin}\r\n 20\r\n{ymin}\r\n  9\r\n$EXTMAX\r\n 10\r\n{xmax}\r\n 20\r\n{ymax}\r\n  0\r\nENDSEC\r\n  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\n"
    )?;
    w.write_all(out.dxf.as_bytes())?;
    w.write_all(b"ENDSEC\r\n  0\r\nEOF\r\n")?;
    w.flush()?;

    // OCAD cross reference table: "SYMBOL LAYERNAME" per DXF layer
    let mut crt = BufWriter::new(fs.create(batchoutfolder.join(COMBINED_CRT))?);
    for layer in &out.layers {
        if let Some(symbol) = crt_symbol(layer) {
            writeln!(crt, "{symbol} {layer}")?;
        }
    }
    crt.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn geojson_outputs_no_duplicate_names() {
        let mut names: Vec<&str> = GEOJSON_OUTPUTS.iter().map(|o| o.name).collect();
        names.sort();
        assert!(
            names.windows(2).all(|w| w[0] != w[1]),
            "duplicate output names: {names:?}"
        );
    }

    #[test]
    fn bindxf_to_geojson_single_path_produces_valid_collection() {
        use crate::geometry::{BinaryDxf, Bounds, Classification, Geometry, Point2, Polylines};
        use crate::io::fs::FileSystem;
        use std::path::PathBuf;

        let fs = crate::io::fs::memory::MemoryFileSystem::new();

        // one 2-point polyline classified as a Contour (symbol "101")
        let mut pls = Polylines::new();
        pls.push(
            vec![Point2::new(0.0, 0.0), Point2::new(100.0, 100.0)],
            Classification::Contour,
        );
        // the knoll-detector artifact has no symbol code and is left out
        pls.push(
            vec![Point2::new(0.0, 0.0), Point2::new(1.0, 1.0)],
            Classification::Knoll1010,
        );
        let dxf = BinaryDxf::new(
            Bounds::new(0.0, 100.0, 0.0, 100.0),
            vec![Geometry::Polylines2(pls)],
        );

        // serialize into the memory FS
        dxf.to_writer(&mut fs.create("test.dxf.bin").unwrap())
            .unwrap();

        bindxf_to_geojson(
            &fs,
            &[PathBuf::from("test.dxf.bin")],
            std::path::Path::new("out.geojson"),
            None,
        )
        .unwrap();

        let val: Value = serde_json::from_reader(fs.open("out.geojson").unwrap()).unwrap();
        assert_eq!(val["type"], "FeatureCollection");
        let feats = val["features"].as_array().unwrap();
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0]["geometry"]["type"], "LineString");
        assert_eq!(feats[0]["properties"]["symbol"], "101");
    }

    /// Write `geometry` as a binary DXF at `path` in the memory file system.
    fn write_bin(fs: &impl FileSystem, path: &str, geometry: Vec<Geometry>) {
        use crate::geometry::Bounds;
        BinaryDxf::new(Bounds::new(0.0, 100.0, 0.0, 100.0), geometry)
            .to_writer(&mut fs.create(path).unwrap())
            .unwrap();
    }

    fn read_features(fs: &impl FileSystem, path: &str) -> Vec<Value> {
        let val: Value = serde_json::from_reader(fs.open(path).unwrap()).unwrap();
        val["features"].as_array().unwrap().clone()
    }

    #[test]
    fn bindxf_to_geojson_maps_each_geometry_type() {
        use crate::geometry::{Point3, Points, Polylines};
        let fs = crate::io::fs::memory::MemoryFileSystem::new();

        let mut contours = Polylines::new();
        contours.push(
            vec![Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 1.0, 0.0)],
            (Classification::ContourIndex, 45.0),
        );
        let mut formlines = Polylines::new();
        formlines.push(
            vec![Point2::new(0.0, 0.0), Point2::new(2.0, 2.0)],
            Classification::Formline,
        );
        let mut knolls = Points::new();
        knolls.push(Point2::new(5.004, 6.0), Classification::Dotknoll);
        write_bin(
            &fs,
            "in.dxf.bin",
            vec![contours.into(), formlines.into(), knolls.into()],
        );

        bindxf_to_geojson(
            &fs,
            &[PathBuf::from("in.dxf.bin")],
            Path::new("out.geojson"),
            None,
        )
        .unwrap();
        let feats = read_features(&fs, "out.geojson");
        assert_eq!(feats.len(), 3);

        // a 3D polyline is a LineString that keeps its elevation
        assert_eq!(feats[0]["geometry"]["type"], "LineString");
        assert_eq!(feats[0]["properties"]["symbol"], "102");
        assert_eq!(feats[0]["properties"]["elevation"], 45.0);
        // a 2D polyline has none
        assert_eq!(feats[1]["geometry"]["type"], "LineString");
        assert_eq!(feats[1]["properties"]["symbol"], "103");
        assert!(feats[1]["properties"].get("elevation").is_none());
        // a point is a Point
        assert_eq!(feats[2]["geometry"]["type"], "Point");
        assert_eq!(feats[2]["geometry"]["coordinates"], json!([5.0, 6.0]));
        assert_eq!(feats[2]["properties"]["symbol"], "109");
    }

    #[test]
    fn bindxf_to_geojson_combines_both_cliff_files() {
        use crate::geometry::Polylines;
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        for (path, class) in [
            ("c2g.dxf.bin", Classification::Cliff2),
            ("c3g.dxf.bin", Classification::Cliff4),
        ] {
            let mut cliffs = Polylines::new();
            cliffs.push(vec![Point2::new(0.0, 0.0), Point2::new(3.0, 0.0)], class);
            write_bin(&fs, path, vec![cliffs.into()]);
        }

        bindxf_to_geojson(
            &fs,
            &[PathBuf::from("c2g.dxf.bin"), PathBuf::from("c3g.dxf.bin")],
            Path::new(&CLIFFS.file_name()),
            None,
        )
        .unwrap();
        let symbols: Vec<Value> = read_features(&fs, &CLIFFS.file_name())
            .iter()
            .map(|f| f["properties"]["symbol"].clone())
            .collect();
        assert_eq!(symbols, [json!("202"), json!("201")]);
    }

    /// A wobble that leaves and returns inside the ISOM minimum mouth is not a bend the
    /// symbol can carry, so it must not survive to the map.
    #[test]
    fn generalise_contour_splices_out_sub_minimum_wobble() {
        // a straight line with a 3 m spike that opens a 2 m mouth
        let mut pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        pts.splice(20..20, [[20.0, 3.0], [21.0, 3.0]]);
        let out = generalise_contour(&pts);
        assert!(
            out.iter().all(|p| p[1] == 0.0),
            "sub-minimum spike survived: {out:?}"
        );
        // the line itself is untouched apart from the spike
        assert_eq!(out.first(), pts.first());
        assert_eq!(out.last(), pts.last());
    }

    /// A deep re-entrant is real terrain, not a wobble, even where its limbs run closer
    /// than the minimum mouth. It may lose no more than what fits inside the ISOM
    /// minimum, never the valley.
    #[test]
    fn generalise_contour_keeps_a_deep_reentrant() {
        let mut pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let deep: Vec<[f64; 2]> = (0..30)
            .map(|i| [20.0, -f64::from(i)])
            .chain((0..30).rev().map(|i| [22.0, -f64::from(i)]))
            .collect();
        pts.splice(20..20, deep);
        let out = generalise_contour(&pts);
        let depth = out.iter().fold(0.0f64, |d, p| d.min(p[1]));
        assert!(
            depth <= -29.0 + MIN_MOUTH_M,
            "a 29 m re-entrant lost more than the ISOM minimum: kept only {depth} m"
        );
    }

    /// ISOM 2017-2: the contour gives way to symbol 109/110, and the gap it leaves has to
    /// be wide enough for the symbol to sit in.
    #[test]
    fn break_at_knolls_opens_a_gap_around_the_symbol() {
        let pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let parts = break_at_knolls(&pts, &[[20.0, 0.0]]);
        assert_eq!(parts.len(), 2, "contour was not broken");
        for part in &parts {
            for p in part {
                assert!(
                    dist(*p, [20.0, 0.0]) >= KNOLL_CLEAR_M,
                    "contour still touches the knoll at {p:?}"
                );
            }
        }
        // and a contour nowhere near a knoll is left as one piece
        assert_eq!(break_at_knolls(&pts, &[[20.0, 50.0]]).len(), 1);
    }

    #[test]
    fn conform_contour_breaks_only_the_contour_family() {
        let pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let knolls = [[10.0, 1.0], [30.0, -1.0]];
        for symbol in ["101", "102", "103"] {
            assert_eq!(conform_contour(symbol, &pts, &knolls).len(), 3, "{symbol}");
        }
        // a cliff passes through whole, even next to a knoll
        assert_eq!(conform_contour("201", &pts, &knolls), vec![pts.clone()]);
    }

    #[test]
    fn published_knolls_prefers_definite_over_ugly() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let name = DOTKNOLLS.file_name();
        let path = Path::new(&name);
        assert!(published_knolls(&fs, path).unwrap().is_empty());

        // an ugly knoll listed first, a definite one 5 m away, a far ugly one, and a
        // contour that is not a point symbol at all
        let knoll = |x: f64, c| {
            terrain_feature(
                FeatureGeometryType::Point,
                vec![json!(x), json!(0.0)],
                c,
                None,
            )
            .unwrap()
        };
        let features = vec![
            knoll(0.0, Classification::UglyDotknoll),
            knoll(5.0, Classification::Dotknoll),
            knoll(50.0, Classification::UglyDotknoll),
            terrain_feature(
                FeatureGeometryType::LineString,
                vec![json!([0.0, 0.0]), json!([1.0, 0.0])],
                Classification::Contour,
                None,
            )
            .unwrap(),
        ];
        write_feature_collection(&fs, path, features, None).unwrap();

        let kept = published_knolls(&fs, path).unwrap();
        let kept: Vec<([f64; 2], Option<bool>)> =
            kept.into_iter().map(|(p, props)| (p, props.ugly)).collect();
        assert_eq!(kept, [([5.0, 0.0], None), ([50.0, 0.0], Some(true))]);
    }

    fn props(c: Classification, elevation: Option<f64>) -> Value {
        serde_json::to_value(terrain_properties(c, elevation).unwrap()).unwrap()
    }

    #[test]
    fn terrain_properties_flags_present_only_when_set() {
        let contour = props(Classification::Contour, Some(12.5));
        assert_eq!(
            contour,
            json!({"symbol": "101", "symbol_name": "contour", "elevation": 12.5})
        );

        let depression = props(Classification::Depression, Some(12.5));
        assert_eq!(depression["symbol"], "101");
        assert_eq!(depression["symbol_name"], "depression contour");
        assert_eq!(depression["depression"], true);
        assert_eq!(
            props(Classification::FormlineDepression, None)["depression"],
            true
        );

        let slope_line = props(Classification::SlopeLine, Some(12.5));
        assert_eq!(slope_line["symbol"], "101");
        assert_eq!(slope_line["slope_line"], true);
        assert!(slope_line.get("depression").is_none());

        assert_eq!(
            props(Classification::Dotknoll, None),
            json!({"symbol": "109", "symbol_name": "knoll"})
        );
        let ugly = props(Classification::UglyUdepression, None);
        assert_eq!(ugly["symbol"], "111");
        assert_eq!(ugly["ugly"], true);

        // knoll and cliff classes carry no elevation, even from a 3D polyline
        assert_eq!(
            props(Classification::SmallDepression, Some(3.0)),
            json!({"symbol": "111", "symbol_name": "small depression"})
        );
        assert_eq!(
            props(Classification::Cliff4, None),
            json!({"symbol": "201", "symbol_name": "impassable cliff"})
        );
        assert!(terrain_properties(Classification::Knoll1010, None).is_none());
    }

    #[test]
    fn terrain_properties_deserialize_into_their_schema_class() {
        use geojson_types::FeatureProperties as P;
        let back = |c, h| serde_json::from_value::<P>(props(c, h)).unwrap();
        assert!(matches!(
            back(Classification::DepressionIndex, Some(1.0)),
            P::ContourProperties(_)
        ));
        assert!(matches!(
            back(Classification::UglyDotknoll, None),
            P::KnollProperties(_)
        ));
        assert!(matches!(
            back(Classification::Cliff2, None),
            P::CliffProperties(_)
        ));
    }

    #[test]
    fn osm_features_carry_symbol_category_and_upper_level() {
        let road = serde_json::to_value(osm_line(
            "502",
            "road-path",
            false,
            &[[1.0, 2.0], [3.004, 4.0]],
        ))
        .unwrap();
        assert_eq!(road["geometry"]["type"], "LineString");
        assert_eq!(
            road["geometry"]["coordinates"],
            json!([[1.0, 2.0], [3.0, 4.0]])
        );
        assert_eq!(
            road["properties"],
            json!({"symbol": "502", "category": "road-path"})
        );

        // the T suffix reaches vector output as a flag; the symbol stays a plain number
        let bridge =
            serde_json::to_value(osm_line("502", "road-path", true, &[[0.0, 0.0]])).unwrap();
        assert_eq!(
            bridge["properties"],
            json!({"symbol": "502", "category": "road-path", "upper_level": true})
        );

        let ring = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        let lake =
            serde_json::to_value(osm_area("301", "water", false, &[ring.clone(), ring])).unwrap();
        assert_eq!(lake["geometry"]["type"], "Polygon");
        assert_eq!(lake["geometry"]["coordinates"].as_array().unwrap().len(), 2);
        assert!(matches!(
            serde_json::from_value::<geojson_types::FeatureProperties>(lake["properties"].clone())
                .unwrap(),
            geojson_types::FeatureProperties::OsmProperties(_)
        ));
    }

    #[test]
    fn write_feature_collection_writes_osm_features() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let features = vec![osm_line("506", "path", false, &[[0.0, 0.0], [1.0, 1.0]])];
        let name = OSM_LINES.file_name();
        let out = std::path::Path::new(&name);
        write_feature_collection(&fs, out, features, None).unwrap();

        let val: Value = serde_json::from_reader(fs.open(out).unwrap()).unwrap();
        assert_eq!(val["type"], "FeatureCollection");
        assert!(val.get("crs").is_none());
        assert_eq!(val["features"][0]["properties"]["symbol"], "506");
        assert_eq!(val["features"][0]["properties"]["category"], "path");
    }

    #[test]
    fn crs_names_the_epsg_code() {
        assert_eq!(
            serde_json::to_value(crs(Some(3067))).unwrap(),
            json!({"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::3067"}})
        );
        assert!(crs(None).is_none());
    }

    #[test]
    fn generated_types_roundtrip_to_featurecollection_json() {
        use geojson_types::{
            ContourProperties, ContourPropertiesSymbol, Feature, FeatureGeometry,
            FeatureGeometryType, FeatureProperties, GeoJsonOutput,
        };

        let contour = ContourProperties {
            depression: None,
            elevation: None,
            slope_line: None,
            symbol: ContourPropertiesSymbol::X101,
            symbol_name: None,
        };
        let feature = Feature {
            geometry: FeatureGeometry {
                coordinates: vec![serde_json::json!(0.0), serde_json::json!(0.0)],
                type_: FeatureGeometryType::Point,
            },
            properties: FeatureProperties::ContourProperties(contour),
            type_: serde_json::json!("Feature"),
        };
        let collection = GeoJsonOutput {
            crs: None,
            features: vec![feature],
            type_: serde_json::json!("FeatureCollection"),
        };

        let json = serde_json::to_value(&collection).unwrap();
        assert_eq!(json["type"], "FeatureCollection");
        assert!(json["features"].is_array());
        assert_eq!(json["features"][0]["type"], "Feature");
        assert_eq!(json["features"][0]["properties"]["symbol"], "101");
        assert!(json["features"][0]["properties"].get("layer").is_none());
    }

    fn bbox(minx: f64, miny: f64, maxx: f64, maxy: f64) -> Rect {
        Rect::new(minx, miny, maxx, maxy)
    }

    fn terrain_line(c: Classification, pts: &[[f64; 2]]) -> geojson_types::Feature {
        terrain_feature(
            FeatureGeometryType::LineString,
            coords_line(pts.iter().copied()),
            c,
            None,
        )
        .unwrap()
    }

    fn terrain_point(c: Classification, p: [f64; 2]) -> geojson_types::Feature {
        terrain_feature(
            FeatureGeometryType::Point,
            vec![json!(p[0]), json!(p[1])],
            c,
            None,
        )
        .unwrap()
    }

    fn square(x: f64, y: f64, side: f64) -> Vec<Point2> {
        vec![
            Point2::new(x, y),
            Point2::new(x + side, y),
            Point2::new(x + side, y + side),
            Point2::new(x, y + side),
        ]
    }

    fn read(fs: &impl FileSystem, path: &Path) -> geojson_types::GeoJsonOutput {
        read_collection(fs, path).unwrap()
    }

    #[test]
    fn clip_ring_square_crossing_bbox() {
        let ring = [
            [5.0, 5.0],
            [15.0, 5.0],
            [15.0, 15.0],
            [5.0, 15.0],
            [5.0, 5.0],
        ];
        // the quarter square [5,10]x[5,10], closed
        let clipped = clip_ring(&ring, &bbox(0.0, 0.0, 10.0, 10.0));
        assert_eq!(clipped.first(), clipped.last());
        let open = &clipped[..clipped.len() - 1];
        assert_eq!(open.len(), 4);
        for p in open {
            assert!(p[0] >= 5.0 && p[0] <= 10.0 && p[1] >= 5.0 && p[1] <= 10.0);
        }
        assert!(clip_ring(&ring, &bbox(20.0, 20.0, 30.0, 30.0)).is_empty());
        assert_eq!(clip_ring(&ring, &bbox(0.0, 0.0, 20.0, 20.0)).len(), 5);
    }

    #[test]
    fn crop_geojson_clips_every_geometry_to_the_tile() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let features = vec![
            // leaves the tile at x=10 and comes back: two parts
            terrain_line(
                Classification::Contour,
                &[[0.0, 5.0], [20.0, 5.0], [20.0, 8.0], [0.0, 8.0]],
            ),
            // entirely outside
            terrain_line(Classification::Cliff2, &[[20.0, 20.0], [30.0, 30.0]]),
            vegetation_area(
                geojson_types::VegetationPropertiesSymbol::X406,
                &[square(5.0, 5.0, 10.0)],
            ),
            terrain_point(Classification::Dotknoll, [2.0, 2.0]),
            terrain_point(Classification::Dotknoll, [12.0, 2.0]),
            // on the edge shared with the next tile: that tile keeps it
            terrain_point(Classification::Dotknoll, [10.0, 2.0]),
        ];
        write_feature_collection(&fs, Path::new("in.geojson"), features, Some(25832)).unwrap();

        let out = Path::new("out.geojson");
        crop_geojson(
            &fs,
            Path::new("in.geojson"),
            out,
            &bbox(0.0, 0.0, 10.0, 10.0),
        )
        .unwrap();

        let out = read(&fs, out);
        let crs = out.crs.expect("the input's crs is carried over");
        assert_eq!(crs.properties.name, "urn:ogc:def:crs:EPSG::25832");
        let summary: Vec<(FeatureGeometryType, String, Vec<[f64; 2]>)> = out
            .features
            .iter()
            .map(|f| {
                let c = &f.geometry.coordinates;
                let pts = match f.geometry.type_ {
                    FeatureGeometryType::Polygon => polygon_rings(c).concat(),
                    FeatureGeometryType::Point => {
                        vec![[c[0].as_f64().unwrap(), c[1].as_f64().unwrap()]]
                    }
                    FeatureGeometryType::LineString => line_points(c),
                };
                (f.geometry.type_, symbol(&f.properties), pts)
            })
            .collect();
        use FeatureGeometryType::*;
        assert_eq!(
            summary,
            [
                (LineString, "101".into(), vec![[0.0, 5.0], [10.0, 5.0]]),
                (LineString, "101".into(), vec![[10.0, 8.0], [0.0, 8.0]]),
                (
                    Polygon,
                    "406".into(),
                    vec![
                        [5.0, 10.0],
                        [5.0, 5.0],
                        [10.0, 5.0],
                        [10.0, 10.0],
                        [5.0, 10.0]
                    ]
                ),
                (Point, "109".into(), vec![[2.0, 2.0]]),
            ]
        );
    }

    #[test]
    fn merge_geojson_concatenates_the_tiles_of_each_output() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        let write = |name: String, features, epsg| {
            write_feature_collection(&fs, &out.join(name), features, epsg).unwrap();
        };
        let contour = |x: f64| terrain_line(Classification::Contour, &[[x, 0.0], [x, 1.0]]);
        // listed out of order; the merge takes the tiles in file name order
        write(CONTOURS.tile_file_name("b"), vec![contour(3.0)], Some(3067));
        write(
            CONTOURS.tile_file_name("a"),
            vec![contour(1.0), contour(2.0)],
            Some(3067),
        );
        // a previous merge in the folder is not read back
        write(CONTOURS.merged_file_name(), vec![contour(9.0)], None);
        write(
            CLIFFS.tile_file_name("a"),
            vec![terrain_line(
                Classification::Cliff3,
                &[[0.0, 0.0], [3.0, 0.0]],
            )],
            None,
        );

        merge_geojson(&fs, out).unwrap();

        let merged = read(&fs, &out.join(CONTOURS.merged_file_name()));
        assert_eq!(
            merged.crs.unwrap().properties.name,
            "urn:ogc:def:crs:EPSG::3067"
        );
        let xs: Vec<f64> = merged
            .features
            .iter()
            .map(|f| line_points(&f.geometry.coordinates)[0][0])
            .collect();
        assert_eq!(xs, [1.0, 2.0, 3.0]);
        assert_eq!(
            read(&fs, &out.join(CLIFFS.merged_file_name()))
                .features
                .len(),
            1
        );
        assert!(!fs.exists(out.join(VEGETATION.merged_file_name())));
    }

    #[test]
    fn crt_symbol_maps_symbol_codes_to_ocad_numbers() {
        assert_eq!(crt_symbol("101").as_deref(), Some("101.000"));
        assert_eq!(crt_symbol("521").as_deref(), Some("521.000"));
        assert_eq!(crt_symbol("502T"), None);
        assert_eq!(crt_symbol(""), None);
    }

    #[test]
    fn curve_points_samples_bezier_for_geojson() {
        // jagged open contour: sampled output is denser, endpoints unchanged
        let pts: Vec<[f64; 2]> = (0..10)
            .map(|i| [i as f64 * 10.0, if i % 2 == 0 { 0.0 } else { 8.0 }])
            .collect();
        let out = curve_points("101", &pts, false);
        assert!(out.len() > pts.len(), "curve symbol must be densified");
        assert_eq!(out.first(), pts.first());
        assert_eq!(out.last(), pts.last());
        // a building is not a curve symbol and passes through untouched
        assert_eq!(curve_points("521", &pts, false), pts);
        // closed ring stays closed
        let ring = [
            [0.0, 0.0],
            [30.0, 0.0],
            [30.0, 30.0],
            [0.0, 30.0],
            [0.0, 0.0],
        ];
        let out = curve_points("406", &ring, true);
        assert_eq!(out.first(), out.last(), "ring must stay closed");
    }

    #[test]
    fn cliff_chain_follows_curved_face() {
        // dash midpoints along a semicircular face (r=30 m), ~2.4 m apart
        let mids: Vec<[f64; 2]> = (0..40)
            .map(|i| {
                let t = i as f64 / 39.0 * std::f64::consts::PI;
                [30.0 * t.cos(), 30.0 * t.sin()]
            })
            .collect();
        let chains = chain_cliff_dashes(&mids);
        assert_eq!(chains.len(), 1, "one face, one chain");
        assert_eq!(chains[0].len(), 40, "all dashes chained");
        // correct ordering walks the arc: every step is one dash spacing, no jumps
        for w in chains[0].windows(2) {
            assert!(dist(w[0], w[1]) < 3.0, "chain jumps across the face");
        }
        // a lone dash is shorter than the ISOM minimum
        assert!(chain_cliff_dashes(&[[100.0, 100.0]]).is_empty());
    }

    /// A batch output folder holding one merged file per terrain output, a vegetation
    /// area and an OSM line: a straight 101 contour along y=0 through a knoll at (50, 0),
    /// a half-interval contour, a form line, and 202 cliff dashes along y=200.
    fn merged_outputs(fs: &impl FileSystem, out: &Path) {
        fs.create_dir_all(out).unwrap();
        let write = |o: &GeoJsonOutput, features| {
            write_feature_collection(fs, &out.join(o.merged_file_name()), features, None).unwrap();
        };
        let along = |y: f64| (0..=100).map(|x| [x as f64, y]).collect::<Vec<_>>();
        write(
            &CONTOURS,
            vec![
                terrain_line(Classification::Contour, &along(0.0)),
                terrain_line(Classification::ContourIntermed, &along(20.0)),
            ],
        );
        write(
            &FORMLINES,
            vec![terrain_line(Classification::Formline, &along(40.0))],
        );
        write(
            &DOTKNOLLS,
            vec![terrain_point(Classification::Dotknoll, [50.0, 0.0])],
        );
        write(
            &CLIFFS,
            (0..12)
                .map(|i| {
                    let x = i as f64 * 2.5;
                    terrain_line(Classification::Cliff2, &[[x, 200.0], [x + 3.0, 200.0]])
                })
                .collect(),
        );
        write(
            &VEGETATION,
            vec![vegetation_area(
                geojson_types::VegetationPropertiesSymbol::X406,
                &[square(0.0, 300.0, 30.0)],
            )],
        );
        write(
            &OSM_LINES,
            vec![osm_line(
                "502",
                "road-path",
                false,
                &[[0.0, 400.0], [100.0, 400.0]],
            )],
        );
    }

    #[test]
    fn export_combined_publishes_every_merged_output_conformed() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        merged_outputs(&fs, out);

        export_combined(
            &fs,
            out,
            Path::new(crate::merge::MERGED_DXF_BIN),
            Some(25832),
        )
        .unwrap();

        let combined = read(&fs, &out.join(COMBINED_GEOJSON));
        assert_eq!(
            combined.crs.unwrap().properties.name,
            "urn:ogc:def:crs:EPSG::25832"
        );
        let mut by_symbol: BTreeMap<String, Vec<&geojson_types::Feature>> = BTreeMap::new();
        for f in &combined.features {
            by_symbol.entry(symbol(&f.properties)).or_default().push(f);
        }
        let counts: Vec<(&str, usize)> = by_symbol
            .iter()
            .map(|(s, f)| (s.as_str(), f.len()))
            .collect();
        // the contour broken around the knoll into two lines; the half-interval contour
        // left out, the form line kept; the dashes chained into one cliff line
        assert_eq!(
            counts,
            [
                ("101", 2),
                ("103", 1),
                ("109", 1),
                ("202", 1),
                ("406", 1),
                ("502", 1)
            ]
        );
        for f in &by_symbol["101"] {
            let pts = line_points(&f.geometry.coordinates);
            assert!(pts.iter().all(|p| dist(*p, [50.0, 0.0]) >= KNOLL_CLEAR_M));
        }
        assert_eq!(
            line_points(&by_symbol["103"][0].geometry.coordinates)[0][1],
            40.0
        );
        assert_eq!(
            by_symbol["406"][0].geometry.type_,
            FeatureGeometryType::Polygon
        );

        let dxf = String::from_utf8(read_bytes(&fs, &out.join(COMBINED_DXF))).unwrap();
        assert!(dxf.contains("$ACADVER"));
        assert!(dxf.contains("POINT\r\n  8\r\n109\r\n"));
        assert!(dxf.contains("  8\r\n101\r\n"));
        assert!(dxf.contains("SPLINE\r\n  8\r\n406\r\n"));
        assert!(dxf.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n502\r\n"));
        let crt = String::from_utf8(read_bytes(&fs, &out.join(COMBINED_CRT))).unwrap();
        assert_eq!(
            crt,
            "101.000 101\n103.000 103\n109.000 109\n202.000 202\n406.000 406\n502.000 502\n"
        );
    }

    fn read_bytes(fs: &impl FileSystem, path: &Path) -> Vec<u8> {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut fs.open(path).unwrap(), &mut buf).unwrap();
        buf
    }

    #[test]
    fn export_combined_takes_skipped_outputs_from_merged_bin() {
        use crate::geometry::{Bounds, Point3, Polylines};

        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        merged_outputs(&fs, out);
        // merged.dxf.bin with one index contour (3D) and a 201 cliff face of five dashes
        let mut contours = Polylines::new();
        contours.push(
            (0..=10)
                .map(|x| Point3::new(x as f64, 500.0, 12.0))
                .collect(),
            (Classification::ContourIndex, 12.0),
        );
        let mut cliffs = Polylines::new();
        for i in 0..5 {
            let x = i as f64 * 2.5;
            cliffs.push(
                vec![Point2::new(x, 600.0), Point2::new(x + 3.0, 600.0)],
                Classification::Cliff3,
            );
        }
        let bin = Path::new(crate::merge::MERGED_DXF_BIN);
        BinaryDxf::new(
            Bounds::new(0.0, 100.0, 0.0, 700.0),
            vec![Geometry::Polylines3(contours), Geometry::Polylines2(cliffs)],
        )
        .to_writer(&mut fs.create(bin).unwrap())
        .unwrap();

        export_combined(&fs, out, bin, None).unwrap();

        let combined = read(&fs, &out.join(COMBINED_GEOJSON));
        assert!(combined.crs.is_none());
        let symbols: BTreeSet<String> = combined
            .features
            .iter()
            .map(|f| symbol(&f.properties))
            .collect();
        // contours, form lines and cliffs (skip_when_merged_bin) come from the bin only;
        // the dot knolls, vegetation and OSM outputs from their merged GeoJSON
        assert_eq!(
            symbols,
            ["102", "109", "201", "406", "502"].map(String::from).into()
        );
        let index = combined
            .features
            .iter()
            .find_map(|f| match &f.properties {
                FeatureProperties::ContourProperties(p) => Some(p),
                _ => None,
            })
            .unwrap();
        assert_eq!(index.elevation, Some(12.0));
    }

    #[test]
    fn export_combined_takes_knolls_from_merged_bin_without_the_geojson() {
        use crate::geometry::{Bounds, Point3, Points, Polylines};

        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        // vector_vege=0: no merged GeoJSON, only merged.dxf.bin with a straight contour
        // through a knoll, and a closed contour loop well away from it
        let mut contours = Polylines::new();
        contours.push(
            (0..=100)
                .map(|x| Point3::new(x as f64, 0.0, 10.0))
                .collect(),
            (Classification::Contour, 10.0),
        );
        let lp = |x: f64, y: f64| Point3::new(x, y, 20.0);
        let mut ring: Vec<Point3> = (0..40).map(|i| lp(200.0 + i as f64, 0.0)).collect();
        ring.extend((0..40).map(|i| lp(240.0, i as f64)));
        ring.extend((0..40).map(|i| lp(240.0 - i as f64, 40.0)));
        ring.extend((0..=40).map(|i| lp(200.0, 40.0 - i as f64)));
        contours.push(ring, (Classification::Contour, 20.0));
        let mut points = Points::new();
        points.push(Point2::new(50.0, 0.0), Classification::Dotknoll);
        let bin = Path::new(crate::merge::MERGED_DXF_BIN);
        BinaryDxf::new(
            Bounds::new(0.0, 300.0, 0.0, 100.0),
            vec![Geometry::Polylines3(contours), Geometry::Points(points)],
        )
        .to_writer(&mut fs.create(bin).unwrap())
        .unwrap();

        export_combined(&fs, out, bin, None).unwrap();

        let combined = read(&fs, &out.join(COMBINED_GEOJSON));
        let lines: Vec<Vec<[f64; 2]>> = combined
            .features
            .iter()
            .filter(|f| f.geometry.type_ == FeatureGeometryType::LineString)
            .map(|f| line_points(&f.geometry.coordinates))
            .collect();
        let points = combined
            .features
            .iter()
            .filter(|f| symbol(&f.properties) == "109")
            .count();
        assert_eq!(points, 1);
        // the straight contour broken in two around the knoll, the loop kept closed
        assert_eq!(lines.len(), 3);
        let closed = lines.iter().filter(|l| l.first() == l.last()).count();
        assert_eq!(closed, 1);
    }

    #[test]
    fn export_combined_writes_nothing_without_outputs() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        export_combined(&fs, out, Path::new(crate::merge::MERGED_DXF_BIN), None).unwrap();
        assert!(!fs.exists(out.join(COMBINED_GEOJSON)));
        assert!(!fs.exists(out.join(COMBINED_DXF)));
    }
}
