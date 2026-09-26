//! GeoJSON output for vector features (contours, cliffs, knolls),
//! plus the serialization contract generated from the JSON Schema.

use std::io::{BufWriter, Write};

use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Classification, Geometry};
use crate::io::fs::FileSystem;
use geojson_types::FeatureGeometryType;

/// Rust types generated from `schema/geojson.schema.json` by `typify` in `build.rs`.
///
/// These types are the serialization contract for GeoJSON output properties.
/// Add new property classes to the schema file; `cargo build` regenerates this
/// module automatically.
#[allow(clippy::all)]
mod geojson_types {
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

pub const GEOJSON_OUTPUTS: &[GeoJsonOutput] = &[
    GeoJsonOutput {
        name: "contours",
        skip_when_merged_bin: true,
    },
    GeoJsonOutput {
        name: "formlines",
        skip_when_merged_bin: true,
    },
    GeoJsonOutput {
        name: "dotknolls",
        skip_when_merged_bin: false,
    },
    GeoJsonOutput {
        name: "cliffs",
        skip_when_merged_bin: true,
    },
    GeoJsonOutput {
        name: "vegetation",
        skip_when_merged_bin: false,
    },
    GeoJsonOutput {
        name: "yellow",
        skip_when_merged_bin: false,
    },
    GeoJsonOutput {
        name: "undergrowth",
        skip_when_merged_bin: false,
    },
    GeoJsonOutput {
        name: "osm_lines",
        skip_when_merged_bin: false,
    },
    GeoJsonOutput {
        name: "osm_areas",
        skip_when_merged_bin: false,
    },
];

/// Legacy GeoJSON `crs` member for a projected EPSG code. RFC 7946 dropped `crs`, but
/// GIS tools still read it, and without it projected coordinates load misplaced.
/// None (no `epsg` config key) omits the member.
pub fn crs(epsg: Option<u32>) -> Option<Value> {
    epsg.map(|code| {
        json!({"type":"name","properties":{"name": format!("urn:ogc:def:crs:EPSG::{code}")}})
    })
}

fn write_prelude<W: Write>(w: &mut W, crs: Option<&Value>) -> anyhow::Result<()> {
    w.write_all(b"{\"type\":\"FeatureCollection\"")?;
    if let Some(c) = crs {
        w.write_all(b",\"crs\":")?;
        serde_json::to_writer(&mut *w, c)?;
    }
    w.write_all(b",\"features\":[")?;
    Ok(())
}

/// Round to cm to keep files small; sub-cm is noise at map scale.
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Build the coordinate array for one line/ring (`.into()` for a [`Value`]).
pub fn coords_line<I: IntoIterator<Item = [f64; 2]>>(pts: I) -> Vec<Value> {
    pts.into_iter()
        .map(|[x, y]| json!([r2(x), r2(y)]))
        .collect()
}

/// Build a GeoJSON feature with string properties.
pub fn feature(gtype: &str, coordinates: Value, props: &[(&str, &str)]) -> Value {
    let properties = serde_json::Map::from_iter(
        props
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string()))),
    );
    json!({
        "type": "Feature",
        "geometry": { "type": gtype, "coordinates": coordinates },
        "properties": properties,
    })
}

/// Write a FeatureCollection. `crs` is included verbatim when given (see [`crs`]).
pub fn write_feature_collection<W: Write>(
    w: &mut W,
    features: &[Value],
    crs: Option<&Value>,
) -> anyhow::Result<()> {
    write_prelude(w, crs)?;
    let mut first = true;
    for f in features {
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *w, f)?;
    }
    w.write_all(b"]}")?;
    Ok(())
}

/// Typed GeoJSON properties for a terrain classification: contour family, knoll and
/// small depression, or cliff, chosen by which schema class accepts its symbol code.
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
    Some(if let Ok(symbol) = code.parse() {
        ContourProperties {
            symbol,
            symbol_name,
            elevation,
            depression: flag(c.is_depression_line()),
            slope_line: flag(c == Classification::SlopeLine),
        }
        .into()
    } else if let Ok(symbol) = code.parse() {
        KnollProperties {
            symbol,
            symbol_name,
            ugly: flag(c.is_ugly()),
        }
        .into()
    } else {
        CliffProperties {
            symbol: code.parse().ok()?,
            symbol_name,
        }
        .into()
    })
}

fn terrain_feature(
    geometry: FeatureGeometryType,
    coordinates: Vec<Value>,
    c: Classification,
    elevation: Option<f64>,
) -> Option<geojson_types::Feature> {
    Some(geojson_types::Feature {
        geometry: geojson_types::FeatureGeometry {
            coordinates,
            type_: geometry,
        },
        properties: terrain_properties(c, elevation)?,
        type_: json!("Feature"),
    })
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
    let collection = geojson_types::GeoJsonOutput {
        crs: crs(epsg).map(serde_json::from_value).transpose()?,
        features,
        type_: json!("FeatureCollection"),
    };
    let mut w = BufWriter::new(fs.create(output)?);
    serde_json::to_writer(&mut w, &collection)?;
    w.flush()?;
    Ok(())
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
