//! GeoJSON output for vector features (contours, cliffs, knolls, vector-mapped
//! shapefile features, vegetation areas), one file per table of the isom-maplibre symbol
//! table (ADR 0006), plus the serialization contract generated from the JSON Schema.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use log::{debug, info, warn};
use serde_json::{Value, json};

use crate::cliffs::CliffSet;
use crate::config::Outputs;
use crate::geometry::{BinaryDxf, Classification, Geometry, Point2, Point3, Points, Polylines};
use crate::io::fs::FileSystem;
use crate::isom::{IsomCode, IsomTable, SymbolGeometry};
use crate::knolls::DotKnollSet;
use crate::mapframe::{IsomMinima, MapFrame};
use crate::merge::ContourSet;
use crate::plan::Rect;
use crate::validity;
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

/// File name of a table's GeoJSON: `<table>.geojson`. Per tile in its temp folder, and
/// the combined export's publication of the whole batch in the batch output folder.
pub fn file_name(table: IsomTable) -> String {
    format!("{}.geojson", table.as_str())
}

/// File name of one tile's cropped table in the batch output folder:
/// `<tile>_<table>.geojson`.
pub fn tile_file_name(table: IsomTable, tile: &str) -> String {
    format!("{tile}_{}", file_name(table))
}

/// File name of the batch merge of every tile's table: `merged_<table>.geojson`.
pub fn merged_file_name(table: IsomTable) -> String {
    tile_file_name(table, MERGED_PREFIX)
}

/// Prefix of the batch merge outputs in the batch output folder. Files carrying it are
/// merge outputs, never merge inputs.
pub const MERGED_PREFIX: &str = "merged";

/// The combined export's DXF of every merged vector output in the batch output folder,
/// and the OCAD cross reference table that maps its DXF layers (symbol codes) to OCAD
/// symbols. Its GeoJSON is one [`file_name`] per table.
pub const COMBINED_DXF: &str = "output.dxf";
pub const COMBINED_CRT: &str = "output.ocdCrt";

/// The stage a vector feature comes from, told apart by its properties. A table can take
/// features from several stages (contours and form lines in `contours`, the vegetation
/// grids and a vector mapping in `vegetation_areas`), so each stage replaces only its own
/// features there; see [`write_tables`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Contours, index contours and slope lines: smoothjoin's [`ContourSet`].
    Contours,
    /// The renderer's form lines (103.000), from `formlines.dxf.bin`.
    FormLines,
    /// Knoll and small depression points (109.000, 111.000): the [`DotKnollSet`].
    Knolls,
    /// Cliffs (201.000, 202.000): the [`CliffSet`].
    Cliffs,
    /// Green shades, open land and undergrowth traced from the vegetation grids.
    Vegetation,
    /// Shapefile features matched by a vector mapping rule.
    VectorMapping,
}

impl Source {
    /// Whether a feature with these properties comes from this stage.
    fn owns(self, props: &FeatureProperties) -> bool {
        use FeatureProperties as P;
        use geojson_types::ContourPropertiesIsomCode::X103000;
        match (self, props) {
            (Self::Contours, P::ContourProperties(p)) => p.isom_code != X103000,
            (Self::FormLines, P::ContourProperties(p)) => p.isom_code == X103000,
            (Self::Knolls, P::KnollProperties(_))
            | (Self::Cliffs, P::CliffProperties(_))
            | (Self::Vegetation, P::VegetationProperties(_))
            | (Self::VectorMapping, P::OsmProperties(_)) => true,
            _ => false,
        }
    }
}

/// Legacy GeoJSON `crs` member for a projected EPSG code. RFC 7946 dropped `crs`, but
/// GIS tools still read it, and without it projected coordinates load misplaced.
/// None (no `epsg` config key and no CRS declared by the input) omits the member.
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

/// Typed GeoJSON properties for a terrain classification: the schema class of its
/// code's table (contours, knolls and small depressions, or cliffs). `level_m`, a 3D
/// line's z (the level a contour was traced at), is kept only in the contours table.
/// None for a classification the vector output leaves out: the knoll-detector
/// artifact, which has no symbol code, and the half-interval lines, which the style
/// would draw as form lines; the form lines are the renderer's selection of them
/// ([`Source::FormLines`]).
fn terrain_properties(
    c: Classification,
    level_m: Option<f64>,
) -> Option<geojson_types::FeatureProperties> {
    use geojson_types::{CliffProperties, ContourProperties, KnollProperties};

    if c.is_half_interval_line() {
        return None;
    }
    let code = c.isom_code()?;
    let symbol_name = c.symbol_name().map(String::from);
    let flag = |set: bool| set.then_some(true);
    Some(match code.table() {
        IsomTable::Contours => ContourProperties {
            isom_code: class_code(code),
            symbol_name,
            level_m,
            depression: flag(c.is_depression_line()),
        }
        .into(),
        IsomTable::KnollsPoints => KnollProperties {
            isom_code: class_code(code),
            symbol_name,
            ugly: flag(c.is_ugly()),
        }
        .into(),
        IsomTable::Cliffs => CliffProperties {
            isom_code: class_code(code),
            symbol_name,
        }
        .into(),
        table => unreachable!(
            "{code} is in {}, which has no terrain class",
            table.as_str()
        ),
    })
}

/// A code as the schema class of its table types it; every code KP emits is listed there.
fn class_code<T: std::str::FromStr>(code: IsomCode) -> T {
    code.as_str()
        .parse()
        .unwrap_or_else(|_| panic!("the schema class of {code}'s table does not list it"))
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
    level_m: Option<f64>,
) -> Option<geojson_types::Feature> {
    Some(feature(
        geometry,
        coordinates,
        terrain_properties(c, level_m)?,
    ))
}

/// Typed GeoJSON properties of a shapefile record matched by a vector mapping rule:
/// its symbol code, its category (the mapping's name), and `upper_level` only when set.
fn osm_properties(
    isom_code: IsomCode,
    category: &str,
    upper_level: bool,
) -> geojson_types::FeatureProperties {
    geojson_types::OsmProperties {
        isom_code,
        category: category.to_string(),
        upper_level: upper_level.then_some(true),
    }
    .into()
}

/// LineString feature for one part of a shapefile polyline matched by a vector mapping rule.
pub fn osm_line(
    isom_code: IsomCode,
    category: &str,
    upper_level: bool,
    line: &[[f64; 2]],
) -> geojson_types::Feature {
    feature(
        FeatureGeometryType::LineString,
        coords_line(line.iter().copied()),
        osm_properties(isom_code, category, upper_level),
    )
}

/// Polygon feature (exterior ring, then holes) for a shapefile polygon matched by a
/// vector mapping rule.
pub fn osm_area(
    isom_code: IsomCode,
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
        osm_properties(isom_code, category, upper_level),
    )
}

/// Polygon feature (exterior ring, then holes) for one vegetation area, with its
/// greenshade index as `shade` when given. Rings are open (first vertex not repeated);
/// GeoJSON rings are closed here.
pub fn vegetation_area(
    isom_code: geojson_types::VegetationPropertiesIsomCode,
    shade: Option<std::num::NonZeroU64>,
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
        geojson_types::VegetationProperties { isom_code, shade }.into(),
    )
}

/// The symbol code a feature is drawn with.
fn isom_code(props: &FeatureProperties) -> IsomCode {
    let code = match props {
        FeatureProperties::ContourProperties(p) => p.isom_code.to_string(),
        FeatureProperties::KnollProperties(p) => p.isom_code.to_string(),
        FeatureProperties::CliffProperties(p) => p.isom_code.to_string(),
        FeatureProperties::VegetationProperties(p) => p.isom_code.to_string(),
        FeatureProperties::OsmProperties(p) => return p.isom_code,
    };
    code.parse()
        .unwrap_or_else(|_| panic!("the schema lists {code}, which the symbol table does not"))
}

/// A LineString with fewer than two distinct positions, as written (rounded to cm):
/// RFC 7946 wants two or more, and a zero-length line draws nothing.
fn degenerate_line(f: &geojson_types::Feature) -> bool {
    let coords = &f.geometry.coordinates;
    f.geometry.type_ == FeatureGeometryType::LineString
        && coords.iter().all(|p| Some(p) == coords.first())
}

/// Twice the signed area of a ring as written, on the cm grid (positive
/// counter-clockwise; exactly 0 for a ring with no area).
fn ring_area2(ring: &[Value]) -> i128 {
    let ring: Vec<validity::Cm> = line_points(ring).into_iter().map(validity::cm).collect();
    validity::area2(&ring)
}

/// Put a geometry in the shape RFC 7946 and the simple-feature sinks want: no position
/// repeated next to itself (as written, in cm; the rounding can merge two), and for a
/// Polygon, rings of four or more positions with an area, the exterior
/// counter-clockwise and the holes clockwise (RFC 7946 3.1.6). Returns how many rings
/// were dropped, or None when the exterior was, and the feature with it.
fn normalise_geometry(geometry: &mut geojson_types::FeatureGeometry) -> Option<usize> {
    let coords = &mut geometry.coordinates;
    match geometry.type_ {
        FeatureGeometryType::Point => Some(0),
        FeatureGeometryType::LineString => {
            coords.dedup();
            Some(0)
        }
        FeatureGeometryType::Polygon => {
            for ring in coords.iter_mut() {
                if let Value::Array(ring) = ring {
                    ring.dedup();
                }
            }
            let area = |ring: &Value| match ring.as_array() {
                Some(r) if r.len() >= 4 => ring_area2(r),
                _ => 0,
            };
            if coords.first().is_none_or(|exterior| area(exterior) == 0) {
                return None;
            }
            let before = coords.len();
            coords.retain(|ring| area(ring) != 0);
            for (i, ring) in coords.iter_mut().enumerate() {
                if let Value::Array(ring) = ring
                    && (ring_area2(ring) > 0) != (i == 0)
                {
                    ring.reverse();
                }
            }
            Some(before - coords.len())
        }
    }
}

/// Write one FeatureCollection (with the legacy `crs` member when an EPSG code is given),
/// each geometry normalised and degenerate lines left out (see [`write_collection`]).
pub fn write_feature_collection(
    fs: &impl FileSystem,
    output: &std::path::Path,
    features: Vec<geojson_types::Feature>,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    write_collection(
        fs,
        output,
        geojson_types::GeoJsonOutput {
            crs: crs(epsg),
            features,
            type_: json!("FeatureCollection"),
        },
    )
}

/// Write the features of one stage into the tables in `folder`, each feature to the
/// table of its symbol code. A table keeps the features of the other stages and loses
/// what `source` wrote there before, so a stage that runs again replaces its features
/// instead of adding to them. A table file is created only when it gets a feature, and
/// one that cannot be read (left by an earlier version) is replaced.
pub fn write_tables(
    fs: &impl FileSystem,
    folder: &Path,
    source: Source,
    features: Vec<geojson_types::Feature>,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut by_table: HashMap<IsomTable, Vec<geojson_types::Feature>> = HashMap::new();
    for f in features {
        // a feature the stage does not own would be kept, not replaced, on its next run
        anyhow::ensure!(
            source.owns(&f.properties),
            "{source:?} does not own the feature it writes: {:?}",
            f.properties
        );
        by_table
            .entry(isom_code(&f.properties).table())
            .or_default()
            .push(f);
    }
    for &table in IsomTable::ALL {
        let new = by_table.remove(&table).unwrap_or_default();
        let path = folder.join(file_name(table));
        let mut features = if fs.exists(&path) {
            // one this version cannot read is left from a run of an earlier one
            read_collection(fs, &path).map_or_else(
                |e| {
                    warn!("Replacing {}, unreadable: {e}", path.display());
                    Vec::new()
                },
                |c| c.features,
            )
        } else if new.is_empty() {
            continue;
        } else {
            Vec::new()
        };
        features.retain(|f| !source.owns(&f.properties));
        features.extend(new);
        write_feature_collection(fs, &path, features, epsg)?;
    }
    Ok(())
}

/// Every GeoJSON file goes through here, so this is where each geometry is put in the
/// shape RFC 7946 and the simple-feature sinks want ([`normalise_geometry`]) and
/// degenerate lines ([`degenerate_line`]) are left out: a crop can cut a line down to
/// one position.
fn write_collection(
    fs: &impl FileSystem,
    output: &Path,
    mut collection: geojson_types::GeoJsonOutput,
) -> anyhow::Result<()> {
    let (mut rings, before) = (0, collection.features.len());
    collection
        .features
        .retain_mut(|f| match normalise_geometry(&mut f.geometry) {
            Some(dropped) => {
                rings += dropped;
                !degenerate_line(f)
            }
            None => false,
        });
    let features = before - collection.features.len();
    if rings + features > 0 {
        debug!(
            "{}: left out {rings} degenerate rings and {features} degenerate features",
            output.display()
        );
    }
    let mut w = BufWriter::new(fs.create(output)?);
    serde_json::to_writer(&mut w, &collection)?;
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

/// Write the terrain of one or more binary DXF files (cliffs, form lines...) from
/// `source` into the tables in `folder` (see [`write_tables`]); each geometry becomes
/// features as [`geometry_features`] makes them.
///
/// Property schema: see `schema/geojson.schema.json` ($defs/ContourProperties,
/// KnollProperties, CliffProperties).
pub fn bindxf_to_tables(
    fs: &impl FileSystem,
    inputs: &[std::path::PathBuf],
    folder: &Path,
    source: Source,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut features = Vec::new();
    for input in inputs {
        let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
        for geom in dxf.take_geometry() {
            features.extend(geometry_features(&geom));
        }
    }
    write_tables(fs, folder, source, features, epsg)
}

/// Write smoothjoin's contours ([`Source::Contours`]) into the tables in `folder`, as
/// [`bindxf_to_tables`] writes them from `out2.dxf.bin`.
pub fn write_contour_tables(
    fs: &impl FileSystem,
    folder: &Path,
    contours: &ContourSet,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let features = line3_features(&contours.lines).collect();
    write_tables(fs, folder, Source::Contours, features, epsg)
}

/// Write the dot knolls ([`Source::Knolls`]) into the tables in `folder`, as
/// [`bindxf_to_tables`] writes them from `dotknolls.dxf.bin`.
pub fn write_knoll_tables(
    fs: &impl FileSystem,
    folder: &Path,
    dot_knolls: &DotKnollSet,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let features = point_features(&dot_knolls.points).collect();
    write_tables(fs, folder, Source::Knolls, features, epsg)
}

/// Write the cliffs ([`Source::Cliffs`]) into the tables in `folder`, as
/// [`bindxf_to_tables`] writes them from `c2g.dxf.bin` and `c3g.dxf.bin`: the passable
/// cliffs, then the impassable ones.
pub fn write_cliff_tables(
    fs: &impl FileSystem,
    folder: &Path,
    cliffs: &CliffSet,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let features = line2_features(&cliffs.passable)
        .chain(line2_features(&cliffs.impassable))
        .collect();
    write_tables(fs, folder, Source::Cliffs, features, epsg)
}

/// The features of one terrain geometry, in its order: polylines become LineStrings and
/// points become Points, each with the properties of its classification (see
/// [`terrain_properties`]; a 3D line keeps its level). Records the vector output leaves
/// out are skipped.
fn geometry_features(geom: &Geometry) -> Vec<geojson_types::Feature> {
    match geom {
        Geometry::Polylines2(pl) => line2_features(pl).collect(),
        Geometry::Polylines3(pl) => line3_features(pl).collect(),
        Geometry::Points(pts) => point_features(pts).collect(),
    }
}

/// [`geometry_features`] of 2D lines.
fn line2_features(
    lines: &Polylines<Point2, Classification>,
) -> impl Iterator<Item = geojson_types::Feature> + '_ {
    lines.iter().filter_map(|(p, &c)| {
        let coords = coords_line(p.iter().map(|pt| [pt.x, pt.y]));
        terrain_feature(FeatureGeometryType::LineString, coords, c, None)
    })
}

/// [`geometry_features`] of 3D lines.
fn line3_features(
    lines: &Polylines<Point3, (Classification, f64)>,
) -> impl Iterator<Item = geojson_types::Feature> + '_ {
    lines.iter().filter_map(|(p, &(c, h))| {
        let coords = coords_line(p.iter().map(|pt| [pt.x, pt.y]));
        terrain_feature(FeatureGeometryType::LineString, coords, c, Some(h))
    })
}

/// [`geometry_features`] of points.
fn point_features(points: &Points) -> impl Iterator<Item = geojson_types::Feature> + '_ {
    points.iter().filter_map(|(p, &c)| {
        let coords = vec![json!(r2(p.x)), json!(r2(p.y))];
        terrain_feature(FeatureGeometryType::Point, coords, c, None)
    })
}

fn point2([x, y]: [f64; 2]) -> Point2 {
    Point2::new(x, y)
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    point2(a).distance(point2(b))
}

// The ISOM 2017-2 contour and point-symbol rules below are applied by the combined
// export, after the per-tile outputs are merged: the knolls that survive spacing are
// only known then, across tile edges.

// ISOM 2017-2 minimum dimensions for contours ([`IsomMinima`], in ground metres). The
// standard specifies them on the 1:15,000 original, ground metres = mm x 15 there: the
// smallest bend that can be drawn is 0.25 mm centre to centre (3.75 m) and the mouth of
// a re-entrant or spur must be wider than 0.5 mm (7.5 m). The wider bound subsumes the
// narrower one, so a single pass at `contour_mouth` (8 m) enforces both.
//
// `contour_max_detour` (24 m) bounds how much line one splice may consume. Nothing
// removed can depart further than `contour_mouth` from the join that replaces it, so it
// only stops a long near-parallel double-back from being swallowed in a single cut.

/// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    point2(p).distance_to_segment(point2(a), point2(b))
}

/// Splice out excursions that leave and return within `contour_mouth` *and* never depart
/// further than `contour_mouth` from the join replacing them: the wobbles ISOM 2017-2 means
/// by "small details on contours should be avoided because they tend to hide the main
/// features of the terrain".
///
/// Both bounds matter. The first alone would let a narrow re-entrant be truncated at any
/// neck along its length; together they guarantee nothing is removed that reaches beyond
/// what the symbol's own minimum dimension can carry. A closed ring is protected from
/// being consumed whole by requiring the kept remainder to stay above the same bound.
fn generalise_contour(pts: &[[f64; 2]], minima: &IsomMinima) -> Vec<[f64; 2]> {
    let mouth = minima.contour_mouth;
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
        while j < pts.len() && cum[j] - cum[i] <= minima.contour_max_detour {
            let along = cum[j] - cum[i];
            if along > mouth
                && along < total - mouth
                && dist(pts[i], pts[j]) < mouth
                && pts[i + 1..j]
                    .iter()
                    .all(|p| seg_dist(*p, pts[i], pts[j]) < mouth)
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
/// plus half a contour width of air: `clearance`, [`IsomMinima::knoll_clearance`] (3.5 m).
///
/// Break a contour into the pieces that stay clear of the knoll symbols, dropping any
/// piece too short to be a line.
///
/// Cuts at vertices rather than interpolating the exact crossing point. Contour vertices
/// are ~1.2 m apart, well inside the clearance, so the gap is right to within a vertex.
fn break_at_knolls(pts: &[[f64; 2]], knolls: &[[f64; 2]], clearance: f64) -> Vec<Vec<[f64; 2]>> {
    if knolls.is_empty() {
        return vec![pts.to_vec()];
    }
    let mut parts = Vec::new();
    let mut cur: Vec<[f64; 2]> = Vec::new();
    for p in pts {
        if knolls.iter().any(|k| dist(*k, *p) < clearance) {
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

/// True for the contour family, the codes of the `contours` table (101 contour and its
/// slope line, 102 index, 103 form line, and 104/105 earth bank and wall from a vector
/// mapping): the symbols the ISOM contour rules above apply to.
fn is_contour_family(code: IsomCode) -> bool {
    code.table() == IsomTable::Contours
}

/// Apply the ISOM contour rules to one published line: generalise detail below what the
/// symbol can carry, then break where a knoll symbol needs room. Anything that is not a
/// contour passes through as a single piece, untouched.
fn conform_contour(
    code: IsomCode,
    pts: &[[f64; 2]],
    knolls: &[[f64; 2]],
    minima: &IsomMinima,
) -> Vec<Vec<[f64; 2]>> {
    if !is_contour_family(code) {
        return vec![pts.to_vec()];
    }
    break_at_knolls(
        &generalise_contour(pts, minima),
        knolls,
        minima.knoll_clearance,
    )
}

/// ISOM 109/110/111 point symbols must not touch or overlap each other either (12 m
/// footprint length).
const POINT_MIN_SPACING_M: f64 = 12.0;

/// The knoll and small depression point symbols that survive to the map: the Point
/// features of a `knolls_points` table through a greedy spacing
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

// Batch mode: each tile's tables are cropped to the tile into the batch output folder,
// the tiles are merged per table, and the combined export publishes every merged table
// under its own name, and all of them as one DXF file.

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

/// A box on the cm grid the writer rounds to.
fn cm_box(bbox: &Rect) -> validity::CmBox {
    let (x0, y0) = validity::cm([bbox.minx, bbox.miny]);
    let (x1, y1) = validity::cm([bbox.maxx, bbox.maxy]);
    [x0, y0, x1, y1]
}

/// The Polygons of a Polygon's part within the bbox ([`validity::clip_polygon`]): none
/// when it is apart, several when it leaves the box and comes back, each valid.
fn clip_polygon(rings: &[Vec<[f64; 2]>], bbox: &Rect) -> Vec<Vec<Value>> {
    let rings: Vec<Vec<validity::Cm>> = rings
        .iter()
        .map(|r| r.iter().map(|&p| validity::cm(p)).collect())
        .collect();
    validity::clip_polygon(&rings, &cm_box(bbox))
        .into_iter()
        .map(|polygon| {
            polygon
                .into_iter()
                .map(|ring| Value::Array(coords_line(ring.into_iter().map(validity::metres))))
                .collect()
        })
        .collect()
}

/// Crop every feature of a GeoJSON file to the bbox (a batch tile without its padding)
/// and write the result, carrying the input's `crs` over. A line that leaves the box
/// and comes back becomes one LineString feature per part, and a polygon one Polygon
/// feature per part, each with the feature's properties.
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
                for rings in clip_polygon(&polygon_rings(coords), bbox) {
                    features.push(feature(
                        FeatureGeometryType::Polygon,
                        rings,
                        f.properties.clone(),
                    ));
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
    write_collection(fs, output, collection)
}

/// Merge the per-tile `<tile>_<table>.geojson` files in the batch output folder into
/// `merged_<table>.geojson`, for every table. Tiles are taken in file name order; the
/// first tile's `crs` is carried over.
pub fn merge_geojson(fs: &impl FileSystem, batchoutfolder: &Path) -> anyhow::Result<()> {
    for &table in IsomTable::ALL {
        let suffix = tile_file_name(table, "");
        let merged_name = merged_file_name(table);
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
            info!("No files found for table {}, skipping...", table.as_str());
            continue;
        }
        files.sort();

        let mut merged = read_collection(fs, &files[0])?;
        for file in &files[1..] {
            merged.features.extend(read_collection(fs, file)?.features);
        }
        write_collection(fs, &batchoutfolder.join(merged_name), merged)?;
    }
    Ok(())
}

/// Symbols whose geometry is organic and gets Bezier curves in the combined DXF (and the
/// same curve, sampled, in the combined GeoJSON): the contour family and the cliff lines,
/// traced by KP, plus the vegetation areas KP traces from its grids and the minor
/// watercourse. The rest of their tables stays a hand list on purpose: roads, buildings
/// and mapped field edges (401, 415 in `vegetation_areas`) keep their straight lines.
fn curve_symbol(code: IsomCode) -> bool {
    use IsomCode::*;
    is_contour_family(code)
        || (code.table() == IsomTable::Cliffs && code.geometry() == SymbolGeometry::Line)
        || matches!(
            code,
            C306_000 | C403_000 | C406_000 | C407_000 | C408_000 | C410_000
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
fn curve_points(code: IsomCode, pts: &[[f64; 2]], closed: bool) -> Vec<[f64; 2]> {
    if !(curve_symbol(code) && pts.len() > 3) {
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
    code: IsomCode,
    pts: &[[f64; 2]],
    closed: bool,
    knolls: &[[f64; 2]],
    minima: &IsomMinima,
) -> Vec<Vec<[f64; 2]>> {
    conform_contour(code, pts, knolls, minima)
        .into_iter()
        .flat_map(|piece| {
            let still_closed = closed && piece.first() == piece.last();
            let sampled = curve_points(code, &piece, still_closed);
            if is_contour_family(code) {
                break_at_knolls(&sampled, knolls, minima.knoll_clearance)
            } else {
                vec![sampled]
            }
        })
        .collect()
}

/// One Chaikin pass takes the segment jitter out of a contour-family line before the
/// ISOM rules and the curve fit; other symbols pass through.
fn smoothed(code: IsomCode, pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if !(is_contour_family(code) && pts.len() > 3) {
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
/// features per table, the layers used and the extent; the contours are conformed to
/// `minima`, and the GeoJSON features are clipped to `bounds`, the extent of the merged
/// tables, which the curves can overshoot.
struct Combined {
    minima: IsomMinima,
    bounds: Rect,
    dxf: String,
    tables: HashMap<IsomTable, Vec<geojson_types::Feature>>,
    layers: BTreeSet<IsomCode>,
    min: [f64; 2],
    max: [f64; 2],
}

impl Combined {
    fn new(minima: IsomMinima, bounds: Rect) -> Self {
        Self {
            minima,
            bounds,
            dxf: String::new(),
            tables: HashMap::new(),
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

    /// One GeoJSON feature, to the table of its symbol code.
    fn feature(
        &mut self,
        geometry: FeatureGeometryType,
        coordinates: Vec<Value>,
        props: FeatureProperties,
    ) {
        let table = isom_code(&props).table();
        self.tables
            .entry(table)
            .or_default()
            .push(feature(geometry, coordinates, props));
    }

    fn within(&self, p: &[f64; 2]) -> bool {
        let b = &self.bounds;
        (b.minx..=b.maxx).contains(&p[0]) && (b.miny..=b.maxy).contains(&p[1])
    }

    /// The parts of a published line within the bounds: the line itself when it stays
    /// inside.
    fn clipped(&self, line: Vec<[f64; 2]>) -> Vec<Vec<[f64; 2]>> {
        if line.iter().all(|p| self.within(p)) {
            vec![line]
        } else {
            clip_line(&line, &self.bounds)
        }
    }

    /// LineString features of a published line, clipped to the bounds.
    fn line_features(&mut self, line: Vec<[f64; 2]>, props: &FeatureProperties) {
        for part in self.clipped(line) {
            self.feature(
                FeatureGeometryType::LineString,
                coords_line(part),
                props.clone(),
            );
        }
    }

    /// One DXF entity: SPLINE for curve symbols, POLYLINE otherwise.
    fn dxf_entity(&mut self, code: IsomCode, pts: &[[f64; 2]], closed: bool, elev: Option<f64>) {
        self.grow(pts);
        if curve_symbol(code) && pts.len() > 3 {
            dxf_spline(&mut self.dxf, code.as_str(), pts, closed);
        } else {
            dxf_polyline(&mut self.dxf, code.as_str(), pts, closed, elev);
        }
        self.layers.insert(code);
    }

    /// A line through the ISOM rules: each published piece becomes a DXF entity and a
    /// LineString feature with the line's properties. A line that ends where it starts
    /// (a contour loop) stays closed until a knoll breaks it.
    fn line(&mut self, pts: &[[f64; 2]], props: &FeatureProperties, knolls: &[[f64; 2]]) {
        let code = isom_code(props);
        let level_m = match props {
            FeatureProperties::ContourProperties(p) => p.level_m,
            _ => None,
        };
        let pts = smoothed(code, pts);
        let closed = pts.len() > 3 && pts.first() == pts.last();
        for piece in published_pieces(code, &pts, closed, knolls, &self.minima) {
            let closed = closed && piece.first() == piece.last();
            self.dxf_entity(code, &piece, closed, level_m);
            self.line_features(piece, props);
        }
    }

    /// An area: each ring (curve-sampled for curve symbols) a closed DXF entity, and
    /// Polygon features of them. A ring's curve is taken only where the polygon stays
    /// OGC-valid with it ([`validity::keep_valid`]), the ring as read where not: a curve
    /// fitted to each ring alone can cross itself or another ring. The polygon is then
    /// clipped to the bounds, one feature per part ([`clip_polygon`]). The DXF keeps
    /// every curve.
    fn polygon(&mut self, rings: &[Vec<[f64; 2]>], props: FeatureProperties) {
        if rings.is_empty() {
            return;
        }
        let code = isom_code(&props);
        // as written: on the cm grid, no position repeated next to itself
        let on_grid = |ring: &[[f64; 2]]| {
            let mut ring: Vec<validity::Cm> = ring.iter().map(|&p| validity::cm(p)).collect();
            ring.dedup();
            ring
        };
        let (mut original, mut candidate) = (Vec::new(), Vec::new());
        for ring in rings {
            let curve = curve_points(code, ring, ring.first() == ring.last());
            self.dxf_entity(code, &curve, true, None);
            original.push(on_grid(ring));
            candidate.push(on_grid(&curve));
        }
        let rings = validity::keep_valid(&original, &candidate);
        let parts = if rings
            .iter()
            .flatten()
            .all(|&p| self.within(&validity::metres(p)))
        {
            vec![
                rings
                    .into_iter()
                    .map(|ring| Value::Array(coords_line(ring.into_iter().map(validity::metres))))
                    .collect(),
            ]
        } else {
            let rings: Vec<Vec<[f64; 2]>> = rings
                .into_iter()
                .map(|r| r.into_iter().map(validity::metres).collect())
                .collect();
            clip_polygon(&rings, &self.bounds)
        };
        for coords in parts {
            self.feature(FeatureGeometryType::Polygon, coords, props.clone());
        }
    }

    fn point(&mut self, p: [f64; 2], props: FeatureProperties) {
        use std::fmt::Write as _;
        let code = isom_code(&props);
        self.grow(&[p]);
        let _ = write!(
            self.dxf,
            "POINT\r\n  8\r\n{code}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 50\r\n0\r\n  0\r\n",
            p[0], p[1]
        );
        self.feature(
            FeatureGeometryType::Point,
            vec![json!(r2(p[0])), json!(r2(p[1]))],
            props,
        );
        self.layers.insert(code);
    }

    /// A cliff line chained from dashes: a DXF entity and the sampled curve, clipped to
    /// the bounds, as LineString features.
    fn cliff(&mut self, chain: &[[f64; 2]], props: FeatureProperties) {
        let code = isom_code(&props);
        self.dxf_entity(code, chain, false, None);
        self.line_features(curve_points(code, chain, false), &props);
    }
}

/// Combine every merged table in the batch output folder into one GeoJSON file per
/// table ([`file_name`], every table, empty ones included, so none is left over from an
/// earlier run) with the geojson family in `outputs`, and with the dxf family into
/// [`COMBINED_DXF`] (layer names = symbol codes), plus [`COMBINED_CRT`], the cross
/// reference table for OCAD's "Import DXF" layer-to-symbol conversion.
///
/// The source is the `merged_<table>.geojson` files: the terrain and vegetation tables
/// and a vector mapping's features. On the way:
/// - knoll and small depression points: spacing-filtered by [`published_knolls`];
/// - contours and form lines: one Chaikin pass, ISOM generalisation, broken around the
///   published knolls, with the ISOM sizes at `frame`'s map scale;
/// - cliffs: KP's per-cell dashes chained into cliff lines, too-short faces dropped;
/// - curve symbols get Bezier SPLINEs in the DXF and the same curve, sampled, in the
///   GeoJSON.
///
/// Writes nothing when there is nothing to export.
pub fn export_combined(
    fs: &impl FileSystem,
    batchoutfolder: &Path,
    frame: &MapFrame,
    epsg: Option<u32>,
    outputs: Outputs,
) -> anyhow::Result<()> {
    use geojson_types::FeatureProperties as P;

    // The point symbols are settled first: ISOM makes the contours give way to them.
    let knolls_table = batchoutfolder.join(merged_file_name(IsomTable::KnollsPoints));
    let knolls = if fs.exists(&knolls_table) {
        published_knolls(fs, &knolls_table)?
    } else {
        Vec::new()
    };
    let knoll_pts: Vec<[f64; 2]> = knolls.iter().map(|(p, _)| *p).collect();

    let mut merged = Vec::new();
    for &table in IsomTable::ALL {
        let path = batchoutfolder.join(merged_file_name(table));
        if fs.exists(&path) {
            merged.extend(read_collection(fs, &path)?.features);
        }
    }
    // the merged tables' extent: the tiles, which every table is cropped to
    let mut bounds = Rect::new(f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for f in &merged {
        let coords = &f.geometry.coordinates;
        let positions = match f.geometry.type_ {
            FeatureGeometryType::Point => line_points(&[Value::Array(coords.clone())]),
            FeatureGeometryType::LineString => line_points(coords),
            FeatureGeometryType::Polygon => polygon_rings(coords).concat(),
        };
        for [x, y] in positions {
            bounds = Rect::new(
                bounds.minx.min(x),
                bounds.miny.min(y),
                bounds.maxx.max(x),
                bounds.maxy.max(y),
            );
        }
    }

    let mut out = Combined::new(frame.isom_minima(), bounds);
    // cliff dash midpoints per cliff symbol, chained into cliff lines below
    let mut cliffs: BTreeMap<_, (geojson_types::CliffProperties, Vec<[f64; 2]>)> = BTreeMap::new();
    let mut add_dash = |props: geojson_types::CliffProperties, pts: &[[f64; 2]]| {
        if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
            let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
            cliffs
                .entry(props.isom_code)
                .or_insert_with(|| (props, Vec::new()))
                .1
                .push(mid);
        }
    };

    for f in merged {
        let coords = &f.geometry.coordinates;
        match (f.geometry.type_, f.properties) {
            // the knoll points, spacing-filtered, are published below
            (FeatureGeometryType::Point, _) => {}
            (FeatureGeometryType::LineString, P::CliffProperties(p)) => {
                add_dash(p, &line_points(coords))
            }
            (FeatureGeometryType::LineString, props) => {
                out.line(&line_points(coords), &props, &knoll_pts)
            }
            (FeatureGeometryType::Polygon, props) => out.polygon(&polygon_rings(coords), props),
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

    if outputs.geojson {
        for &table in IsomTable::ALL {
            let features = out.tables.remove(&table).unwrap_or_default();
            write_feature_collection(fs, &batchoutfolder.join(file_name(table)), features, epsg)?;
        }
    }
    if !outputs.dxf {
        return Ok(());
    }

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

    // OCAD cross reference table: "SYMBOL LAYERNAME" per DXF layer; the layer is the
    // symbol code, which is also the OCAD symbol number
    let mut crt = BufWriter::new(fs.create(batchoutfolder.join(COMBINED_CRT))?);
    for code in &out.layers {
        writeln!(crt, "{code} {code}")?;
    }
    crt.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::ContourKind;
    use std::path::{Path, PathBuf};

    const CONTOUR: Classification = Classification::Contour(ContourKind::CONTOUR);

    /// The ISOM minima at the default map scale.
    fn minima() -> IsomMinima {
        MapFrame::default().isom_minima()
    }

    #[test]
    fn table_file_names() {
        assert_eq!(file_name(IsomTable::KnollsPoints), "knolls_points.geojson");
        assert_eq!(
            tile_file_name(IsomTable::Contours, "tile"),
            "tile_contours.geojson"
        );
        assert_eq!(
            merged_file_name(IsomTable::VegetationAreas),
            "merged_vegetation_areas.geojson"
        );
    }

    /// Write `geometry` as a binary DXF at `path` in the memory file system.
    fn write_bin(fs: &impl FileSystem, path: &str, geometry: Vec<Geometry>) {
        use crate::geometry::Bounds;
        BinaryDxf::new(Bounds::new(0.0, 100.0, 0.0, 100.0), geometry)
            .to_writer(&mut fs.create(path).unwrap())
            .unwrap();
    }

    fn read_features(fs: &impl FileSystem, table: IsomTable) -> Vec<Value> {
        let val: Value = serde_json::from_reader(fs.open(file_name(table)).unwrap()).unwrap();
        assert_eq!(val["type"], "FeatureCollection");
        val["features"].as_array().unwrap().clone()
    }

    #[test]
    fn bindxf_to_tables_maps_each_geometry_type() {
        use crate::geometry::{Point3, Points, Polylines};
        let fs = crate::io::fs::memory::MemoryFileSystem::new();

        let mut contours = Polylines::new();
        contours.push(
            vec![Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 1.0, 0.0)],
            (Classification::Contour(ContourKind::INDEX), 45.0),
        );
        // a half-interval contour: the style would draw it as a form line, so it is left out
        contours.push(
            vec![Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 1.0, 0.0)],
            (Classification::Contour(ContourKind::HALF_INTERVAL), 42.5),
        );
        let mut slope_lines = Polylines::new();
        slope_lines.push(
            vec![Point2::new(0.0, 0.0), Point2::new(2.0, 2.0)],
            Classification::SlopeLine,
        );
        // the knoll-detector artifact has no symbol code and is left out
        slope_lines.push(
            vec![Point2::new(0.0, 0.0), Point2::new(1.0, 1.0)],
            Classification::Knoll1010,
        );
        let mut knolls = Points::new();
        knolls.push(Point2::new(5.004, 6.0), Classification::Dotknoll);
        write_bin(&fs, "in.dxf.bin", vec![contours.into(), slope_lines.into()]);
        write_bin(&fs, "knolls.dxf.bin", vec![knolls.into()]);

        let folder = Path::new("");
        for (input, source) in [
            ("in.dxf.bin", Source::Contours),
            ("knolls.dxf.bin", Source::Knolls),
        ] {
            bindxf_to_tables(&fs, &[PathBuf::from(input)], folder, source, None).unwrap();
        }
        let contours = read_features(&fs, IsomTable::Contours);
        assert_eq!(contours.len(), 2);
        // a 3D polyline is a LineString that keeps its level
        assert_eq!(contours[0]["geometry"]["type"], "LineString");
        assert_eq!(contours[0]["properties"]["isom_code"], "102.000");
        assert_eq!(contours[0]["properties"]["level_m"], 45.0);
        // a 2D polyline has none
        assert_eq!(contours[1]["geometry"]["type"], "LineString");
        assert_eq!(contours[1]["properties"]["isom_code"], "101.001");
        assert!(contours[1]["properties"].get("level_m").is_none());
        // a point is a Point, in its own table
        let knolls = read_features(&fs, IsomTable::KnollsPoints);
        assert_eq!(knolls.len(), 1);
        assert_eq!(knolls[0]["geometry"]["type"], "Point");
        assert_eq!(knolls[0]["geometry"]["coordinates"], json!([5.0, 6.0]));
        assert_eq!(knolls[0]["properties"]["isom_code"], "109.000");
        // no table file for a table nothing was written to
        assert!(!fs.exists(file_name(IsomTable::Cliffs)));
    }

    /// The typed writers write the same table bytes as `bindxf_to_tables` on the values'
    /// `.dxf.bin` dumps (both build their features with `line2_features`,
    /// `line3_features` and `point_features`, so this checks the dump round trip and the
    /// table plumbing), and the tables pass the schema.
    #[test]
    fn the_typed_writers_match_bindxf_to_tables() {
        use crate::geometry::{Bounds, Point3, Points, Polylines};
        use crate::knolls::DotKnollSet;
        use crate::merge::ContourSet;
        let bounds = Bounds::new(0.0, 100.0, 0.0, 100.0);
        let mut lines = Polylines::new();
        for (class, level) in [
            (Classification::Contour(ContourKind::INDEX), 45.0),
            (Classification::Contour(ContourKind::HALF_INTERVAL), 42.5),
            (
                Classification::Contour(ContourKind::CONTOUR.with_depression(true)),
                40.0,
            ),
            (Classification::SlopeLine, 40.0),
        ] {
            let line = vec![
                Point3::new(0.0, 0.0, level),
                Point3::new(1.234, 5.678, level),
            ];
            lines.push(line, (class, level));
        }
        let contours = ContourSet {
            lines,
            bounds: bounds.clone(),
        };
        let mut points = Points::new();
        points.push(Point2::new(5.004, 6.0), Classification::Dotknoll);
        points.push(Point2::new(7.0, 8.0), Classification::UglyUdepression);
        let dot_knolls = DotKnollSet {
            points,
            bounds: bounds.clone(),
        };
        let dash = |x: f64, class| {
            let mut lines = Polylines::new();
            lines.push(vec![Point2::new(x, 1.0), Point2::new(x, 3.94)], class);
            lines
        };
        let mut impassable = dash(20.0, Classification::Cliff3);
        impassable.push(
            vec![Point2::new(30.0, 1.0), Point2::new(30.0, 3.94)],
            Classification::Cliff4,
        );
        let cliffs = CliffSet {
            passable: dash(10.004, Classification::Cliff2),
            impassable,
            bounds,
        };

        let from_values = crate::io::fs::memory::MemoryFileSystem::new();
        write_contour_tables(&from_values, Path::new(""), &contours, Some(25832)).unwrap();
        write_knoll_tables(&from_values, Path::new(""), &dot_knolls, Some(25832)).unwrap();
        write_cliff_tables(&from_values, Path::new(""), &cliffs, Some(25832)).unwrap();

        let from_files = crate::io::fs::memory::MemoryFileSystem::new();
        for (dumps, source) in [
            (
                vec![("out2.dxf.bin", contours.to_bindxf())],
                Source::Contours,
            ),
            (
                vec![("dotknolls.dxf.bin", dot_knolls.to_bindxf())],
                Source::Knolls,
            ),
            (
                vec![
                    ("c2g.dxf.bin", cliffs.passable_bindxf()),
                    ("c3g.dxf.bin", cliffs.impassable_bindxf()),
                ],
                Source::Cliffs,
            ),
        ] {
            let mut input = Vec::new();
            for (dump, dxf) in dumps {
                dxf.to_writer(&mut from_files.create(dump).unwrap())
                    .unwrap();
                input.push(PathBuf::from(dump));
            }
            bindxf_to_tables(&from_files, &input, Path::new(""), source, Some(25832)).unwrap();
        }

        let schema: Value =
            serde_json::from_str(include_str!("../schema/geojson.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for table in [
            IsomTable::Contours,
            IsomTable::KnollsPoints,
            IsomTable::Cliffs,
        ] {
            let read = |fs: &crate::io::fs::memory::MemoryFileSystem| {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut fs.open(file_name(table)).unwrap(), &mut bytes)
                    .unwrap();
                bytes
            };
            let bytes = read(&from_values);
            assert_eq!(bytes, read(&from_files), "{table:?}");
            let collection: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(validator.is_valid(&collection), "{table:?}");
        }
        // the half-interval line is left out, as bindxf_to_tables leaves it out
        assert_eq!(read_features(&from_values, IsomTable::Contours).len(), 3);
        assert_eq!(
            read_features(&from_values, IsomTable::KnollsPoints).len(),
            2
        );
        let codes: Vec<Value> = read_features(&from_values, IsomTable::Cliffs)
            .iter()
            .map(|f| f["properties"]["isom_code"].clone())
            .collect();
        assert_eq!(codes, ["202.000", "201.000", "201.000"]);
    }

    #[test]
    fn bindxf_to_tables_combines_both_cliff_files() {
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

        bindxf_to_tables(
            &fs,
            &[PathBuf::from("c2g.dxf.bin"), PathBuf::from("c3g.dxf.bin")],
            Path::new(""),
            Source::Cliffs,
            None,
        )
        .unwrap();
        let codes: Vec<Value> = read_features(&fs, IsomTable::Cliffs)
            .iter()
            .map(|f| f["properties"]["isom_code"].clone())
            .collect();
        assert_eq!(codes, [json!("202.000"), json!("201.000")]);
    }

    /// A form line with fewer than two distinct positions (the renderer's selection can
    /// end one after a single vertex) is not a LineString (RFC 7946), so it is not written.
    #[test]
    fn bindxf_to_tables_drops_degenerate_lines() {
        use crate::geometry::Polylines;
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let mut formlines = Polylines::new();
        for pts in [
            vec![[1.0, 1.0]],
            // two positions that round to the same cm
            vec![[2.0, 2.0], [2.001, 2.0]],
            vec![[0.0, 0.0], [3.0, 0.0]],
        ] {
            let pts = pts.into_iter().map(point2).collect();
            formlines.push(pts, Classification::Formline);
        }
        write_bin(&fs, "formlines.dxf.bin", vec![formlines.into()]);

        bindxf_to_tables(
            &fs,
            &[PathBuf::from("formlines.dxf.bin")],
            Path::new(""),
            Source::FormLines,
            None,
        )
        .unwrap();
        let lines: Vec<Value> = read_features(&fs, IsomTable::Contours)
            .iter()
            .map(|f| f["geometry"]["coordinates"].clone())
            .collect();
        assert_eq!(lines, [json!([[0.0, 0.0], [3.0, 0.0]])]);
    }

    /// A table file an earlier version left in a reused temp folder is replaced, not an
    /// error.
    #[test]
    fn write_tables_replaces_an_unreadable_table() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let old = json!({"type": "FeatureCollection", "features": [{"type": "Feature",
            "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
            "properties": {"symbol": "201"}}]});
        serde_json::to_writer(fs.create(file_name(IsomTable::Cliffs)).unwrap(), &old).unwrap();
        let cliff = terrain_line(Classification::Cliff2, &[[0.0, 0.0], [3.0, 0.0]]);
        write_tables(&fs, Path::new(""), Source::Cliffs, vec![cliff], None).unwrap();
        let cliffs = read_features(&fs, IsomTable::Cliffs);
        assert_eq!(cliffs.len(), 1);
        assert_eq!(cliffs[0]["properties"]["isom_code"], "202.000");
    }

    /// Two stages share the contours table: each run of a stage replaces its own
    /// features there and keeps the other's.
    #[test]
    fn write_tables_replaces_only_the_sources_own_features() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let folder = Path::new("");
        let line = |c, x: f64| terrain_line(c, &[[x, 0.0], [x, 1.0]]);
        let write = |source, features| write_tables(&fs, folder, source, features, None).unwrap();
        write(Source::Contours, vec![line(CONTOUR, 1.0)]);
        write(Source::FormLines, vec![line(Classification::Formline, 2.0)]);
        write(Source::FormLines, vec![line(Classification::Formline, 3.0)]);
        write(
            Source::VectorMapping,
            vec![osm_area(
                IsomCode::C401_000,
                "farm",
                false,
                &[vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [0.0, 0.0]]],
            )],
        );
        write(
            Source::Contours,
            vec![line(Classification::Contour(ContourKind::INDEX), 4.0)],
        );

        let summary: Vec<(Value, f64)> = read_features(&fs, IsomTable::Contours)
            .iter()
            .map(|f| {
                let x = f["geometry"]["coordinates"][0][0].as_f64().unwrap();
                (f["properties"]["isom_code"].clone(), x)
            })
            .collect();
        assert_eq!(summary, [(json!("103.000"), 3.0), (json!("102.000"), 4.0)]);
        // the mapped open land went to its own table
        let farm = read_features(&fs, IsomTable::VegetationAreas);
        assert_eq!(farm[0]["properties"]["isom_code"], "401.000");
        assert_eq!(farm[0]["properties"]["category"], "farm");
    }

    /// A wobble that leaves and returns inside the ISOM minimum mouth is not a bend the
    /// symbol can carry, so it must not survive to the map.
    #[test]
    fn generalise_contour_splices_out_sub_minimum_wobble() {
        // a straight line with a 3 m spike that opens a 2 m mouth
        let mut pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        pts.splice(20..20, [[20.0, 3.0], [21.0, 3.0]]);
        let out = generalise_contour(&pts, &minima());
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
        let out = generalise_contour(&pts, &minima());
        let depth = out.iter().fold(0.0f64, |d, p| d.min(p[1]));
        assert!(
            depth <= -29.0 + minima().contour_mouth,
            "a 29 m re-entrant lost more than the ISOM minimum: kept only {depth} m"
        );
    }

    /// ISOM 2017-2: the contour gives way to symbol 109/110, and the gap it leaves has to
    /// be wide enough for the symbol to sit in.
    #[test]
    fn break_at_knolls_opens_a_gap_around_the_symbol() {
        let pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let parts = break_at_knolls(&pts, &[[20.0, 0.0]], 3.5);
        assert_eq!(parts.len(), 2, "contour was not broken");
        for part in &parts {
            for p in part {
                assert!(
                    dist(*p, [20.0, 0.0]) >= 3.5,
                    "contour still touches the knoll at {p:?}"
                );
            }
        }
        // and a contour nowhere near a knoll is left as one piece
        assert_eq!(break_at_knolls(&pts, &[[20.0, 50.0]], 3.5).len(), 1);
    }

    #[test]
    fn conform_contour_breaks_only_the_contour_family() {
        let pts: Vec<[f64; 2]> = (0..40).map(|i| [f64::from(i), 0.0]).collect();
        let knolls = [[10.0, 1.0], [30.0, -1.0]];
        use IsomCode::*;
        for code in [C101_000, C101_001, C102_000, C103_000] {
            assert_eq!(
                conform_contour(code, &pts, &knolls, &minima()).len(),
                3,
                "{code}"
            );
        }
        // a cliff passes through whole, even next to a knoll
        assert_eq!(
            conform_contour(C201_000, &pts, &knolls, &minima()),
            vec![pts.clone()]
        );
    }

    #[test]
    fn published_knolls_prefers_definite_over_ugly() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let name = file_name(IsomTable::KnollsPoints);
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
                CONTOUR,
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

    fn props(c: Classification, level_m: Option<f64>) -> Value {
        serde_json::to_value(terrain_properties(c, level_m).unwrap()).unwrap()
    }

    #[test]
    fn terrain_properties_flags_present_only_when_set() {
        let contour = props(CONTOUR, Some(12.5));
        assert_eq!(
            contour,
            json!({"isom_code": "101.000", "symbol_name": "contour", "level_m": 12.5})
        );

        let depression = props(
            Classification::Contour(ContourKind::CONTOUR.with_depression(true)),
            Some(12.5),
        );
        assert_eq!(depression["isom_code"], "101.000");
        assert_eq!(depression["symbol_name"], "depression contour");
        assert_eq!(depression["depression"], true);
        assert_eq!(
            props(Classification::FormlineDepression, None)["depression"],
            true
        );

        // the slope line is 101's variant code, not a flag
        assert_eq!(
            props(Classification::SlopeLine, Some(12.5)),
            json!({"isom_code": "101.001", "symbol_name": "slope line", "level_m": 12.5})
        );

        assert_eq!(
            props(Classification::Dotknoll, None),
            json!({"isom_code": "109.000", "symbol_name": "knoll"})
        );
        let ugly = props(Classification::UglyUdepression, None);
        assert_eq!(ugly["isom_code"], "111.000");
        assert_eq!(ugly["ugly"], true);

        // knoll and cliff classes carry no level, even from a 3D polyline
        assert_eq!(
            props(Classification::SmallDepression, Some(3.0)),
            json!({"isom_code": "111.000", "symbol_name": "small depression"})
        );
        assert_eq!(
            props(Classification::Cliff4, None),
            json!({"isom_code": "201.000", "symbol_name": "impassable cliff"})
        );
        assert!(terrain_properties(Classification::Knoll1010, None).is_none());
        assert!(
            terrain_properties(
                Classification::Contour(ContourKind::INDEX_HALF_INTERVAL.with_depression(true)),
                Some(1.0)
            )
            .is_none()
        );
    }

    /// Every code a classification or a schema class enum carries converts between the
    /// symbol table and the class's generated enum both ways.
    #[test]
    fn codes_round_trip_between_the_table_and_the_schema_enums() {
        use Classification::*;
        for c in [
            ContourSimple,
            Contour(ContourKind::CONTOUR),
            Contour(ContourKind::INDEX),
            Contour(ContourKind::CONTOUR.with_depression(true)),
            Contour(ContourKind::INDEX.with_depression(true)),
            Formline,
            FormlineDepression,
            Dotknoll,
            Udepression,
            UglyDotknoll,
            UglyUdepression,
            Cliff2,
            Cliff3,
            Cliff4,
            SlopeLine,
            SmallDepression,
        ] {
            let props = terrain_properties(c, None).unwrap();
            assert_eq!(Some(isom_code(&props)), c.isom_code(), "{c:?}");
        }
        for c in [Veg403, Veg406, Veg407, Veg408, Veg410] {
            let code = c.isom_code().unwrap();
            let class: geojson_types::VegetationPropertiesIsomCode = class_code(code);
            assert_eq!(class.to_string(), code.as_str());
        }
        let schema: Value =
            serde_json::from_str(include_str!("../schema/geojson.schema.json")).unwrap();
        for class in [
            "ContourProperties",
            "KnollProperties",
            "CliffProperties",
            "VegetationProperties",
        ] {
            for code in schema["$defs"][class]["properties"]["isom_code"]["enum"]
                .as_array()
                .unwrap()
            {
                let props = json!({"isom_code": code});
                let back: FeatureProperties = serde_json::from_value(props.clone()).unwrap();
                assert_eq!(isom_code(&back).as_str(), code, "{class}");
                assert_eq!(serde_json::to_value(&back).unwrap(), props, "{class}");
            }
        }
    }

    /// The schema takes a contour's level as `level_m` and no longer knows `elevation`
    /// (contour properties allow no other member).
    #[test]
    fn schema_accepts_level_m_on_contours() {
        let schema: Value =
            serde_json::from_str(include_str!("../schema/geojson.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let collection = |properties: Value| {
            json!({"type": "FeatureCollection", "features": [{
                "type": "Feature",
                "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]},
                "properties": properties,
            }]})
        };
        let contour = props(Classification::Contour(ContourKind::INDEX), Some(12.5));
        assert_eq!(contour["level_m"], 12.5);
        assert!(validator.is_valid(&collection(contour)));
        assert!(!validator.is_valid(&collection(
            json!({"isom_code": "102.000", "elevation": 12.5})
        )));
        assert!(!validator.is_valid(&collection(
            json!({"isom_code": "102.000", "level_m": "12.5"})
        )));
    }

    #[test]
    fn terrain_properties_deserialize_into_their_schema_class() {
        use geojson_types::FeatureProperties as P;
        let back = |c, h| serde_json::from_value::<P>(props(c, h)).unwrap();
        assert!(matches!(
            back(
                Classification::Contour(ContourKind::INDEX.with_depression(true)),
                Some(1.0)
            ),
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
    fn osm_features_carry_isom_code_category_and_upper_level() {
        let road = serde_json::to_value(osm_line(
            IsomCode::C502_000,
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
            json!({"isom_code": "502.000", "category": "road-path"})
        );

        // the T suffix reaches vector output as a flag; the code stays the symbol's
        let bridge = serde_json::to_value(osm_line(
            IsomCode::C502_000,
            "road-path",
            true,
            &[[0.0, 0.0]],
        ))
        .unwrap();
        assert_eq!(
            bridge["properties"],
            json!({"isom_code": "502.000", "category": "road-path", "upper_level": true})
        );

        let ring = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        let lake = serde_json::to_value(osm_area(
            IsomCode::C301_000,
            "water",
            false,
            &[ring.clone(), ring],
        ))
        .unwrap();
        assert_eq!(lake["geometry"]["type"], "Polygon");
        assert_eq!(lake["geometry"]["coordinates"].as_array().unwrap().len(), 2);
        assert!(matches!(
            serde_json::from_value::<geojson_types::FeatureProperties>(lake["properties"].clone())
                .unwrap(),
            geojson_types::FeatureProperties::OsmProperties(_)
        ));

        // reading back holds the code to the symbol table
        let mut unknown = lake["properties"].clone();
        unknown["isom_code"] = json!("518.000");
        assert!(serde_json::from_value::<geojson_types::FeatureProperties>(unknown).is_err());
    }

    #[test]
    fn write_tables_writes_osm_features_to_the_table_of_their_code() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let line = [[0.0, 0.0], [1.0, 1.0]];
        let features = vec![
            osm_line(IsomCode::C506_000, "path", false, &line),
            osm_line(IsomCode::C516_000, "barrier", false, &line),
        ];
        write_tables(&fs, Path::new(""), Source::VectorMapping, features, None).unwrap();

        let val: Value = serde_json::from_reader(fs.open("paths.geojson").unwrap()).unwrap();
        assert!(val.get("crs").is_none());
        assert_eq!(val["features"][0]["properties"]["isom_code"], "506.000");
        assert_eq!(val["features"][0]["properties"]["category"], "path");
        let manmade = read_features(&fs, IsomTable::Manmade);
        assert_eq!(manmade[0]["properties"]["isom_code"], "516.000");
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
            ContourProperties, ContourPropertiesIsomCode, Feature, FeatureGeometry,
            FeatureGeometryType, FeatureProperties, GeoJsonOutput,
        };

        let contour = ContourProperties {
            depression: None,
            level_m: None,
            isom_code: ContourPropertiesIsomCode::X101000,
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
        assert_eq!(json["features"][0]["properties"]["isom_code"], "101.000");
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
    fn crop_geojson_drops_lines_cut_to_one_position() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let features = vec![
            // touches the tile only at its corner (0, 0)
            terrain_line(CONTOUR, &[[-1.0, 1.0], [1.0, -1.0]]),
            // dips 4 mm into the tile: both clipped ends round to the same cm
            terrain_line(CONTOUR, &[[-1.0, 5.0], [0.004, 5.0], [-1.0, 5.001]]),
            terrain_line(CONTOUR, &[[1.0, 1.0], [2.0, 2.0]]),
        ];
        write_feature_collection(&fs, Path::new("in.geojson"), features, None).unwrap();

        let out = Path::new("out.geojson");
        crop_geojson(
            &fs,
            Path::new("in.geojson"),
            out,
            &bbox(0.0, 0.0, 10.0, 10.0),
        )
        .unwrap();

        assert_eq!(read(&fs, out).features.len(), 1);
    }

    #[test]
    fn crop_geojson_clips_every_geometry_to_the_tile() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let features = vec![
            // leaves the tile at x=10 and comes back: two parts
            terrain_line(CONTOUR, &[[0.0, 5.0], [20.0, 5.0], [20.0, 8.0], [0.0, 8.0]]),
            // entirely outside
            terrain_line(Classification::Cliff2, &[[20.0, 20.0], [30.0, 30.0]]),
            vegetation_area(
                geojson_types::VegetationPropertiesIsomCode::X406000,
                std::num::NonZeroU64::new(2),
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
        let summary: Vec<(FeatureGeometryType, IsomCode, Vec<[f64; 2]>)> = out
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
                (f.geometry.type_, isom_code(&f.properties), pts)
            })
            .collect();
        use FeatureGeometryType::*;
        use IsomCode::*;
        assert_eq!(
            summary,
            [
                (LineString, C101_000, vec![[0.0, 5.0], [10.0, 5.0]]),
                (LineString, C101_000, vec![[10.0, 8.0], [0.0, 8.0]]),
                (
                    Polygon,
                    C406_000,
                    vec![
                        [5.0, 10.0],
                        [5.0, 5.0],
                        [10.0, 5.0],
                        [10.0, 10.0],
                        [5.0, 10.0]
                    ]
                ),
                (Point, C109_000, vec![[2.0, 2.0]]),
            ]
        );
        let FeatureProperties::VegetationProperties(veg) = &out.features[2].properties else {
            panic!("not vegetation: {:?}", out.features[2].properties);
        };
        assert_eq!(
            veg.shade.map(u64::from),
            Some(2),
            "the shade survives the crop"
        );
    }

    #[test]
    fn merge_geojson_concatenates_the_tiles_of_each_table() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        let write = |name: String, features, epsg| {
            write_feature_collection(&fs, &out.join(name), features, epsg).unwrap();
        };
        let contour = |x: f64| terrain_line(CONTOUR, &[[x, 0.0], [x, 1.0]]);
        // listed out of order; the merge takes the tiles in file name order
        let contours = IsomTable::Contours;
        write(
            tile_file_name(contours, "b"),
            vec![contour(3.0)],
            Some(3067),
        );
        write(
            tile_file_name(contours, "a"),
            vec![contour(1.0), contour(2.0)],
            Some(3067),
        );
        // a previous merge or combined export in the folder is not read back
        write(merged_file_name(contours), vec![contour(9.0)], None);
        write(file_name(contours), vec![contour(8.0)], None);
        write(
            tile_file_name(IsomTable::Cliffs, "a"),
            vec![terrain_line(
                Classification::Cliff3,
                &[[0.0, 0.0], [3.0, 0.0]],
            )],
            None,
        );

        merge_geojson(&fs, out).unwrap();

        let merged = read(&fs, &out.join(merged_file_name(contours)));
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
            read(&fs, &out.join(merged_file_name(IsomTable::Cliffs)))
                .features
                .len(),
            1
        );
        assert!(!fs.exists(out.join(merged_file_name(IsomTable::VegetationAreas))));
    }

    #[test]
    fn curve_points_samples_bezier_for_geojson() {
        // jagged open contour: sampled output is denser, endpoints unchanged
        let pts: Vec<[f64; 2]> = (0..10)
            .map(|i| [i as f64 * 10.0, if i % 2 == 0 { 0.0 } else { 8.0 }])
            .collect();
        let out = curve_points(IsomCode::C101_000, &pts, false);
        assert!(out.len() > pts.len(), "curve symbol must be densified");
        assert_eq!(out.first(), pts.first());
        assert_eq!(out.last(), pts.last());
        // a building is not a curve symbol and passes through untouched
        assert_eq!(curve_points(IsomCode::C521_000, &pts, false), pts);
        // closed ring stays closed
        let ring = [
            [0.0, 0.0],
            [30.0, 0.0],
            [30.0, 30.0],
            [0.0, 30.0],
            [0.0, 0.0],
        ];
        let out = curve_points(IsomCode::C406_000, &ring, true);
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

    /// A batch output folder holding the merged tables: a straight 101 contour along y=0
    /// through a knoll at (50, 0) and a form line along y=40 in `contours`, 202 cliff
    /// dashes along y=200, a vegetation area, and an OSM road in `paths`.
    fn merged_outputs(fs: &impl FileSystem, out: &Path) {
        fs.create_dir_all(out).unwrap();
        let write = |table, features| {
            write_feature_collection(fs, &out.join(merged_file_name(table)), features, None)
                .unwrap();
        };
        let along = |y: f64| (0..=100).map(|x| [x as f64, y]).collect::<Vec<_>>();
        write(
            IsomTable::Contours,
            vec![
                terrain_line(CONTOUR, &along(0.0)),
                terrain_line(Classification::Formline, &along(40.0)),
            ],
        );
        write(
            IsomTable::KnollsPoints,
            vec![terrain_point(Classification::Dotknoll, [50.0, 0.0])],
        );
        write(
            IsomTable::Cliffs,
            (0..12)
                .map(|i| {
                    let x = i as f64 * 2.5;
                    terrain_line(Classification::Cliff2, &[[x, 200.0], [x + 3.0, 200.0]])
                })
                .collect(),
        );
        write(
            IsomTable::VegetationAreas,
            vec![vegetation_area(
                geojson_types::VegetationPropertiesIsomCode::X406000,
                std::num::NonZeroU64::new(2),
                &[square(0.0, 300.0, 30.0)],
            )],
        );
        write(
            IsomTable::Paths,
            vec![osm_line(
                IsomCode::C502_000,
                "road-path",
                false,
                &[[0.0, 400.0], [100.0, 400.0]],
            )],
        );
    }

    /// The combined export's features, each with the table file it was read from. Every
    /// table has a file.
    fn read_combined(fs: &impl FileSystem, out: &Path) -> Vec<(IsomTable, geojson_types::Feature)> {
        IsomTable::ALL
            .iter()
            .flat_map(|&table| {
                read(fs, &out.join(file_name(table)))
                    .features
                    .into_iter()
                    .map(move |f| (table, f))
            })
            .collect()
    }

    #[test]
    fn export_combined_publishes_every_merged_table_conformed() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        merged_outputs(&fs, out);

        export_combined(&fs, out, &MapFrame::default(), Some(25832), Outputs::ALL).unwrap();

        let contours = read(&fs, &out.join(file_name(IsomTable::Contours)));
        assert_eq!(
            contours.crs.unwrap().properties.name,
            "urn:ogc:def:crs:EPSG::25832"
        );
        let combined = read_combined(&fs, out);
        let mut by_code: BTreeMap<IsomCode, Vec<&geojson_types::Feature>> = BTreeMap::new();
        for (table, f) in &combined {
            let code = isom_code(&f.properties);
            assert_eq!(code.table(), *table, "{code} published to the wrong table");
            by_code.entry(code).or_default().push(f);
        }
        let counts: Vec<(&str, usize)> =
            by_code.iter().map(|(c, f)| (c.as_str(), f.len())).collect();
        // the contour broken around the knoll into two lines, the form line kept; the
        // dashes chained into one cliff line
        assert_eq!(
            counts,
            [
                ("101.000", 2),
                ("103.000", 1),
                ("109.000", 1),
                ("202.000", 1),
                ("406.000", 1),
                ("502.000", 1)
            ]
        );
        use IsomCode::*;
        for f in &by_code[&C101_000] {
            let pts = line_points(&f.geometry.coordinates);
            assert!(
                pts.iter()
                    .all(|p| dist(*p, [50.0, 0.0]) >= minima().knoll_clearance)
            );
        }
        assert_eq!(
            line_points(&by_code[&C103_000][0].geometry.coordinates)[0][1],
            40.0
        );
        let veg = by_code[&C406_000][0];
        assert_eq!(veg.geometry.type_, FeatureGeometryType::Polygon);
        let FeatureProperties::VegetationProperties(props) = &veg.properties else {
            panic!("not vegetation: {:?}", veg.properties);
        };
        assert_eq!(
            props.shade.map(u64::from),
            Some(2),
            "the shade is published"
        );
        // a table nothing was published to is still written, empty
        assert!(
            read(&fs, &out.join(file_name(IsomTable::Water)))
                .features
                .is_empty()
        );

        let dxf = String::from_utf8(read_bytes(&fs, &out.join(COMBINED_DXF))).unwrap();
        assert!(dxf.contains("$ACADVER"));
        assert!(dxf.contains("POINT\r\n  8\r\n109.000\r\n"));
        assert!(dxf.contains("  8\r\n101.000\r\n"));
        assert!(dxf.contains("SPLINE\r\n  8\r\n406.000\r\n"));
        assert!(dxf.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n502.000\r\n"));
        let crt = String::from_utf8(read_bytes(&fs, &out.join(COMBINED_CRT))).unwrap();
        assert_eq!(
            crt,
            "101.000 101.000\n103.000 103.000\n109.000 109.000\n202.000 202.000\n\
             406.000 406.000\n502.000 502.000\n"
        );
    }

    fn read_bytes(fs: &impl FileSystem, path: &Path) -> Vec<u8> {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut fs.open(path).unwrap(), &mut buf).unwrap();
        buf
    }

    #[test]
    fn export_combined_breaks_lines_around_knolls_and_keeps_loops_closed() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        // a straight contour through a knoll, and a closed contour loop well away from it
        let straight: Vec<[f64; 2]> = (0..=100).map(|x| [x as f64, 0.0]).collect();
        let mut ring: Vec<[f64; 2]> = (0..40).map(|i| [200.0 + i as f64, 0.0]).collect();
        ring.extend((0..40).map(|i| [240.0, i as f64]));
        ring.extend((0..40).map(|i| [240.0 - i as f64, 40.0]));
        ring.extend((0..=40).map(|i| [200.0, 40.0 - i as f64]));
        let write = |table, features| {
            write_feature_collection(&fs, &out.join(merged_file_name(table)), features, None)
                .unwrap();
        };
        write(
            IsomTable::Contours,
            vec![
                terrain_line(CONTOUR, &straight),
                terrain_line(CONTOUR, &ring),
            ],
        );
        write(
            IsomTable::KnollsPoints,
            vec![terrain_point(Classification::Dotknoll, [50.0, 0.0])],
        );
        // a lake reaching past them all, so the extent holds the loop's curve
        let lake = [
            [-50.0, -50.0],
            [300.0, -50.0],
            [300.0, 100.0],
            [-50.0, -50.0],
        ];
        write(
            IsomTable::Water,
            vec![osm_area(
                IsomCode::C301_000,
                "water",
                false,
                &[lake.to_vec()],
            )],
        );

        export_combined(&fs, out, &MapFrame::default(), None, Outputs::ALL).unwrap();

        let combined = read_combined(&fs, out);
        let lines: Vec<Vec<[f64; 2]>> = combined
            .iter()
            .filter(|(_, f)| f.geometry.type_ == FeatureGeometryType::LineString)
            .map(|(_, f)| line_points(&f.geometry.coordinates))
            .collect();
        let points = combined
            .iter()
            .filter(|(_, f)| isom_code(&f.properties) == IsomCode::C109_000)
            .count();
        assert_eq!(points, 1);
        // the straight contour broken in two around the knoll, the loop kept closed
        assert_eq!(lines.len(), 3);
        let closed = lines.iter().filter(|l| l.first() == l.last()).count();
        assert_eq!(closed, 1);
    }

    /// The rings of a Polygon feature, exterior first.
    fn rings_of(f: &geojson_types::Feature) -> Vec<Vec<[f64; 2]>> {
        polygon_rings(&f.geometry.coordinates)
    }

    #[test]
    fn written_rings_run_counter_clockwise_around_clockwise_holes() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        // a shapefile's exterior is clockwise and its holes counter-clockwise
        let cw = vec![
            [0.0, 0.0],
            [0.0, 10.0],
            [10.0, 10.0],
            [10.0, 0.0],
            [0.0, 0.0],
        ];
        let ccw_hole = vec![[2.0, 2.0], [8.0, 2.0], [8.0, 8.0], [2.0, 8.0], [2.0, 2.0]];
        let lake = osm_area(IsomCode::C301_000, "water", false, &[cw, ccw_hole]);
        write_feature_collection(&fs, Path::new("water.geojson"), vec![lake], None).unwrap();

        let lake = &read(&fs, Path::new("water.geojson")).features[0];
        let area = |k: usize| ring_area2(lake.geometry.coordinates[k].as_array().unwrap());
        assert!(area(0) > 0 && area(1) < 0, "{lake:?}");
        let rings = rings_of(lake);
        assert_eq!(rings[0][0], [0.0, 0.0]);
        assert_eq!(rings[0].first(), rings[0].last());
    }

    #[test]
    fn written_geometry_repeats_no_position_next_to_itself() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        // two positions a mm apart are one at cm
        let line = [[0.0, 0.0], [1.0, 0.0], [1.001, 0.0], [2.0, 0.0]];
        let ring = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 0.001],
            [10.0, 10.0],
            [0.0, 0.0],
        ];
        // a hole that collapses to three positions is dropped, and so is one that
        // runs back and forth with no area
        let sliver = vec![[2.0, 2.0], [2.001, 2.0], [5.0, 5.0], [2.0, 2.0]];
        let flat = vec![[2.0, 2.0], [5.0, 5.0], [2.0, 2.0], [5.0, 5.0], [2.0, 2.0]];
        let features = vec![
            terrain_line(Classification::Cliff2, &line),
            osm_area(IsomCode::C301_000, "water", false, &[ring, sliver, flat]),
        ];
        write_feature_collection(&fs, Path::new("t.geojson"), features, None).unwrap();

        let features = read(&fs, Path::new("t.geojson")).features;
        assert_eq!(
            line_points(&features[0].geometry.coordinates),
            [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]]
        );
        assert_eq!(
            rings_of(&features[1]),
            [vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 0.0]]]
        );
    }

    #[test]
    fn a_polygon_whose_exterior_collapses_is_not_written() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let ring = vec![[0.0, 0.0], [0.001, 0.0], [5.0, 5.0], [0.0, 0.0]];
        let flat = vec![[0.0, 0.0], [5.0, 0.0], [10.0, 0.0], [5.0, 0.0], [0.0, 0.0]];
        let lakes = [ring, flat]
            .map(|r| osm_area(IsomCode::C301_000, "water", false, &[r]))
            .to_vec();
        write_feature_collection(&fs, Path::new("w.geojson"), lakes, None).unwrap();
        assert!(read(&fs, Path::new("w.geojson")).features.is_empty());
    }

    /// Every position of every combined feature, by code.
    fn combined_positions(fs: &impl FileSystem, out: &Path) -> Vec<(IsomCode, [f64; 2])> {
        read_combined(fs, out)
            .into_iter()
            .flat_map(|(_, f)| {
                let code = isom_code(&f.properties);
                let coords = &f.geometry.coordinates;
                let positions = match f.geometry.type_ {
                    FeatureGeometryType::Polygon => polygon_rings(coords).concat(),
                    FeatureGeometryType::LineString => line_points(coords),
                    FeatureGeometryType::Point => line_points(&[Value::Array(coords.clone())]),
                };
                positions.into_iter().map(move |p| (code, p))
            })
            .collect()
    }

    #[test]
    fn export_combined_clips_the_curves_to_the_merged_extent() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        // an open land area filling the extent: its curve bulges past the corners'
        // sides; a contour along the top edge, bent down, bows past it
        let area = vegetation_area(
            geojson_types::VegetationPropertiesIsomCode::X403000,
            None,
            &[square(0.0, 0.0, 100.0)],
        );
        let contour: Vec<[f64; 2]> = (0..=20)
            .map(|i| [i as f64 * 5.0, 100.0 - (i as f64 - 10.0).abs()])
            .collect();
        for (table, features) in [
            (IsomTable::VegetationAreas, vec![area]),
            (IsomTable::Contours, vec![terrain_line(CONTOUR, &contour)]),
        ] {
            write_feature_collection(&fs, &out.join(merged_file_name(table)), features, None)
                .unwrap();
        }

        export_combined(&fs, out, &MapFrame::default(), None, Outputs::ALL).unwrap();

        let positions = combined_positions(&fs, out);
        for code in [IsomCode::C403_000, IsomCode::C101_000] {
            assert!(positions.iter().any(|(c, _)| *c == code), "no {code}");
        }
        for (code, [x, y]) in positions {
            assert!(
                (0.0..=100.0).contains(&x) && (0.0..=100.0).contains(&y),
                "{code} at {x} {y}"
            );
        }
        // the DXF keeps the whole curve
        let dxf = String::from_utf8(read_bytes(&fs, &out.join(COMBINED_DXF))).unwrap();
        assert!(dxf.contains("SPLINE\r\n  8\r\n403.000\r\n"));
    }

    #[test]
    fn export_combined_keeps_a_ring_as_read_where_its_curve_would_cross_a_hole() {
        use crate::validity::{cm, inside, ring_is_simple, rings_meet};
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        // an L: at its inner corner the curve cuts into the area, through a hole
        // that hugs the inner edge there
        let pts = |v: &[[f64; 2]]| v.iter().map(|&[x, y]| Point2::new(x, y)).collect();
        let exterior: Vec<Point2> = pts(&[
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [50.0, 100.0],
            [50.0, 50.0],
            [0.0, 50.0],
        ]);
        let hole: Vec<Point2> = pts(&[[30.0, 47.5], [30.0, 49.5], [45.0, 49.5], [45.0, 47.5]]);
        let rings = vec![exterior.clone(), hole.clone()];
        // the curve fitted to the exterior alone cuts the hole
        let closed: Vec<[f64; 2]> = exterior
            .iter()
            .chain(exterior.first())
            .map(|p| [p.x, p.y])
            .collect();
        let curve: Vec<_> = curve_points(IsomCode::C406_000, &closed, true)
            .into_iter()
            .map(cm)
            .collect();
        let hole_cm: Vec<_> = hole
            .iter()
            .chain(hole.first())
            .map(|p| cm([p.x, p.y]))
            .collect();
        assert!(
            rings_meet(&curve, &hole_cm) || !inside(hole_cm[0], &curve),
            "the case needs a curve that cuts the hole"
        );
        let area = vegetation_area(
            geojson_types::VegetationPropertiesIsomCode::X406000,
            None,
            &rings,
        );
        write_feature_collection(
            &fs,
            &out.join(merged_file_name(IsomTable::VegetationAreas)),
            vec![area],
            None,
        )
        .unwrap();

        export_combined(&fs, out, &MapFrame::default(), None, Outputs::ALL).unwrap();

        let combined = read_combined(&fs, out);
        assert_eq!(combined.len(), 1);
        let rings: Vec<Vec<_>> = rings_of(&combined[0].1)
            .into_iter()
            .map(|r| r.into_iter().map(cm).collect())
            .collect();
        assert_eq!(rings.len(), 2);
        assert!(rings.iter().all(|r| ring_is_simple(r)), "{rings:?}");
        assert!(!rings_meet(&rings[0], &rings[1]), "{rings:?}");
        assert!(inside(rings[1][0], &rings[0]), "{rings:?}");
    }

    #[test]
    fn export_combined_writes_nothing_without_outputs() {
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let out = Path::new("out");
        fs.create_dir_all(out).unwrap();
        export_combined(&fs, out, &MapFrame::default(), None, Outputs::ALL).unwrap();
        for &table in IsomTable::ALL {
            assert!(!fs.exists(out.join(file_name(table))));
        }
        assert!(!fs.exists(out.join(COMBINED_DXF)));
    }

    #[test]
    fn export_combined_writes_the_selected_families() {
        let combined = |geojson, dxf| {
            let fs = crate::io::fs::memory::MemoryFileSystem::new();
            let out = Path::new("out");
            merged_outputs(&fs, out);
            let outputs = Outputs {
                raster: false,
                dxf,
                geojson,
            };
            export_combined(&fs, out, &MapFrame::default(), None, outputs).unwrap();
            let tables = IsomTable::ALL
                .iter()
                .filter(|&&t| fs.exists(out.join(file_name(t))))
                .count();
            let dxf = [COMBINED_DXF, COMBINED_CRT].map(|name| fs.exists(out.join(name)));
            (tables, dxf)
        };
        assert_eq!(
            combined(true, false),
            (IsomTable::ALL.len(), [false, false])
        );
        assert_eq!(combined(false, true), (0, [true, true]));
        assert_eq!(combined(true, true), (IsomTable::ALL.len(), [true, true]));
    }
}
