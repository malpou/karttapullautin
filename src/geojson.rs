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

#[cfg(test)]
mod tests {
    use super::*;

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
