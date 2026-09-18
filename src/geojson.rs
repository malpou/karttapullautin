//! GeoJSON output for vector features (contours, cliffs, knolls),
//! plus the serialization contract generated from the JSON Schema.

use std::io::{BufWriter, Write};

use serde_json::{Value, json};

use crate::geometry::{BinaryDxf, Geometry};
use crate::io::fs::FileSystem;

/// Rust types generated from `schema/geojson.schema.json` by `typify` in `build.rs`.
///
/// These types are the serialization contract for GeoJSON output properties.
/// Add new property classes to the schema file; `cargo build` regenerates this
/// module automatically.
#[allow(dead_code, clippy::all)]
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

/// Build a coordinate array for one line/ring.
pub fn coords_line<I: IntoIterator<Item = [f64; 2]>>(pts: I) -> Value {
    Value::Array(
        pts.into_iter()
            .map(|[x, y]| json!([r2(x), r2(y)]))
            .collect(),
    )
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

/// ISOM 2017-2 symbol code for a KP layer name, where one exists.
/// 101 contour, 102 index contour, 103 form line, 109 small knoll,
/// 111 small depression, 201 impassable cliff, 202 rock face.
fn layer_isom(layer: &str) -> Option<&'static str> {
    Some(match layer {
        "cont" | "contour" | "depression" => "101",
        "contour_index" | "depression_index" => "102",
        // intermediate (half-interval) contours are represented as form lines in ISOM
        "contour_intermed"
        | "contour_index_intermed"
        | "depression_intermed"
        | "depression_index_intermed"
        | "formline"
        | "formline_depression" => "103",
        "dotknoll" | "uglydotknoll" => "109",
        "udepression" | "uglyudepression" => "111",
        "cliff2" => "202",
        "cliff3" | "cliff4" => "201",
        _ => return None,
    })
}

fn layer_props(layer: &str) -> Vec<(&str, &str)> {
    let mut props = vec![("layer", layer)];
    if let Some(isom) = layer_isom(layer) {
        props.push(("isom", isom));
    }
    props
}

/// Convert one or more binary DXF files (contours, cliffs, knolls...) into a single
/// GeoJSON FeatureCollection. Polylines become LineStrings with `layer` and (when known)
/// `isom` properties; points become Points.
///
/// Property schema: see `schema/geojson.schema.json` ($defs/ContourProperties,
/// KnollProperties, CliffProperties).
pub fn bindxf_to_geojson(
    fs: &impl FileSystem,
    inputs: &[std::path::PathBuf],
    output: &std::path::Path,
    epsg: Option<u32>,
) -> anyhow::Result<()> {
    let mut feats = Vec::new();
    for input in inputs {
        let dxf = BinaryDxf::from_reader(&mut fs.open(input)?)?;
        for geom in dxf.take_geometry() {
            match geom {
                Geometry::Polylines2(pl) => {
                    for (p, c) in pl.into_iter() {
                        feats.push(feature(
                            "LineString",
                            coords_line(p.iter().map(|pt| [pt.x, pt.y])),
                            &layer_props(c.to_layer()),
                        ));
                    }
                }
                Geometry::Polylines3(pl) => {
                    for (p, (c, h)) in pl.into_iter() {
                        let mut f = feature(
                            "LineString",
                            coords_line(p.iter().map(|pt| [pt.x, pt.y])),
                            &layer_props(c.to_layer()),
                        );
                        f["properties"]["elevation"] = json!(h);
                        feats.push(f);
                    }
                }
                Geometry::Points(pts) => {
                    for (p, c) in pts.into_iter() {
                        feats.push(feature(
                            "Point",
                            json!([r2(p.x), r2(p.y)]),
                            &layer_props(c.to_layer()),
                        ));
                    }
                }
            }
        }
    }
    write_feature_collection(
        &mut BufWriter::new(fs.create(output)?),
        &feats,
        crs(epsg).as_ref(),
    )
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

        // one 2-point polyline classified as a Contour (layer "contour", isom "101")
        let mut pls = Polylines::new();
        pls.push(
            vec![Point2::new(0.0, 0.0), Point2::new(100.0, 100.0)],
            Classification::Contour,
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
        assert_eq!(feats[0]["properties"]["isom"], "101");
    }

    #[test]
    fn generated_types_roundtrip_to_featurecollection_json() {
        use geojson_types::{
            ContourProperties, ContourPropertiesIsom, ContourPropertiesLayer, Feature,
            FeatureGeometry, FeatureGeometryType, FeatureProperties, GeoJsonOutput,
        };

        let contour = ContourProperties {
            depression: None,
            elevation: None,
            isom: ContourPropertiesIsom::X101,
            layer: ContourPropertiesLayer::X101,
            layer_description: None,
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
        assert_eq!(json["features"][0]["properties"]["isom"], "101");
    }
}
