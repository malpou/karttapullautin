//! GeoJSON output for vector features (contours, cliffs, knolls, vector-mapped
//! shapefile features, vegetation areas), plus the serialization contract generated from the JSON Schema.

use std::io::{BufWriter, Write};

use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Classification, Geometry, Point2};
use crate::io::fs::FileSystem;
use geojson_types::FeatureGeometryType;

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
}

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
    let collection = geojson_types::GeoJsonOutput {
        crs: crs(epsg),
        features,
        type_: json!("FeatureCollection"),
    };
    let mut w = BufWriter::new(fs.create(output)?);
    serde_json::to_writer(&mut w, &collection)?;
    w.flush()?;
    Ok(())
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
#[cfg_attr(not(test), expect(dead_code, reason = "called by the combined export"))]
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
#[cfg_attr(not(test), expect(dead_code, reason = "called by the combined export"))]
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
#[cfg_attr(not(test), expect(dead_code, reason = "called by the combined export"))]
fn is_contour_family(symbol: &str) -> bool {
    matches!(symbol, "101" | "102" | "103")
}

/// Apply the ISOM contour rules to one published line: generalise detail below what the
/// symbol can carry, then break where a knoll symbol needs room. Anything that is not a
/// contour passes through as a single piece, untouched.
#[cfg_attr(not(test), expect(dead_code, reason = "called by the combined export"))]
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
#[cfg_attr(not(test), expect(dead_code, reason = "called by the combined export"))]
pub(crate) fn published_knolls(
    fs: &impl FileSystem,
    path: &std::path::Path,
) -> anyhow::Result<Vec<([f64; 2], geojson_types::KnollProperties)>> {
    if !fs.exists(path) {
        return Ok(Vec::new());
    }
    let collection: geojson_types::GeoJsonOutput =
        serde_json::from_reader(std::io::BufReader::new(fs.open(path)?))?;
    let mut candidates: Vec<_> = collection
        .features
        .into_iter()
        .filter_map(|f| {
            let geojson_types::FeatureProperties::KnollProperties(props) = f.properties else {
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
    // stable: definite first, each group in file order
    candidates.sort_by_key(|(_, props)| props.ugly == Some(true));
    let mut kept: Vec<([f64; 2], geojson_types::KnollProperties)> = Vec::new();
    for (p, props) in candidates {
        if kept.iter().all(|(k, _)| dist(*k, p) >= POINT_MIN_SPACING_M) {
            kept.push((p, props));
        }
    }
    Ok(kept)
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
        let symbols: Vec<Value> = read_features(&fs, "cliffs.geojson")
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
        let path = Path::new("dotknolls.geojson");
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
        let out = std::path::Path::new("osm_lines.geojson");
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
}
