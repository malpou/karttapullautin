//! End-to-end tests: run the pullauta binary on the regression inputs and check the
//! vector (GeoJSON) outputs.
//!
//! Ignored by default (slow, needs the test data). Run with:
//!   cargo test --release --test e2e_test -- --ignored
//!
//! Inputs are the regression tile and OSM shapefile zip (same as
//! regression/run.sh). They are read from `$PULLAUTA_E2E_DATA` (a
//! directory holding `test_file.laz` and `test_file.shp.zip`) when set, otherwise
//! downloaded once into cargo's per-target temp directory. Each run works in a fresh
//! directory under that temp directory, never in the repository.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use pullauta::geojson;
use pullauta::isom::{IsomCode, IsomTable};
use serde_json::Value;

const INPUTS: &[(&str, &str)] = &[
    ("test_file.laz", "https://cdn.routechoic.es/test.laz"),
    (
        "test_file.shp.zip",
        "https://cdn.routechoic.es/test-osm.shp.zip",
    ),
];

/// Path of an input file, downloading it to the cache directory when missing.
fn input(name: &str) -> PathBuf {
    let dir = std::env::var_os("PULLAUTA_E2E_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("e2e-data"));
    let path = dir.join(name);
    if !path.exists() {
        let (_, url) = INPUTS.iter().find(|(n, _)| *n == name).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("curl")
            .args(["-Lf", "-o"])
            .arg(&path)
            .arg(url)
            .status()
            .expect("failed to run curl");
        assert!(status.success(), "failed to download {url}");
    }
    path
}

/// A fresh, empty run directory named `name` under cargo's per-target temp directory.
fn run_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The key of a `key=value` (or `key = value`) template line.
fn ini_key(line: &str) -> Option<&str> {
    let (key, _) = line.split_once('=')?;
    (!line.starts_with('#')).then(|| key.trim())
}

/// Write `pullauta.ini` into `dir`: the default template with the given `key=value`
/// lines replaced (every key must already be in the template).
fn write_ini(dir: &Path, settings: &[(&str, &str)]) {
    let template = include_str!("../pullauta.default.ini");
    let mut lines: Vec<String> = template.lines().map(String::from).collect();
    for (key, value) in settings {
        let line = lines
            .iter_mut()
            .find(|l| ini_key(l) == Some(key))
            .unwrap_or_else(|| panic!("{key} is not in pullauta.default.ini"));
        *line = format!("{key}={value}");
    }
    std::fs::write(dir.join("pullauta.ini"), lines.join("\n") + "\n").unwrap();
}

fn run_pullauta(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_pullauta"))
        .args(args)
        .current_dir(dir)
        .env("RUST_LOG", "warn")
        .output()
        .expect("failed to execute pullauta");
    assert!(
        output.status.success(),
        "pullauta {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Read a GeoJSON file, check it is a non-empty FeatureCollection of Features, and
/// return its features.
fn feature_collection(path: &Path) -> Vec<Value> {
    assert!(path.exists(), "{} was not written", path.display());
    let content = std::fs::read_to_string(path).unwrap();
    let mut val: Value = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", path.display()));
    assert_eq!(val["type"], "FeatureCollection", "{}", path.display());
    let features = val["features"]
        .as_array_mut()
        .map(std::mem::take)
        .unwrap_or_else(|| panic!("{}: features is not an array", path.display()));
    assert!(!features.is_empty(), "{}: no features", path.display());
    for f in &features {
        assert_eq!(f["type"], "Feature", "{}: {f}", path.display());
        assert!(
            f["geometry"]["coordinates"].is_array(),
            "{}: {f}",
            path.display()
        );
    }
    features
}

/// Validate every `.geojson` file under `dir` against `schema/geojson.schema.json`, the
/// public contract of the vector output, and against the isom-maplibre symbol table (see
/// [`assert_table_conformance`]). Returns how many files were checked.
fn assert_schema_conformance(dir: &Path) -> usize {
    let schema: Value =
        serde_json::from_str(include_str!("../schema/geojson.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("invalid schema");
    let mut checked = 0;
    for path in files(dir) {
        if path.extension().is_none_or(|e| e != "geojson") {
            continue;
        }
        let val: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", path.display()));
        let errors: Vec<String> = validator
            .iter_errors(&val)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{}: {errors:#?}", path.display());
        assert_table_conformance(&path, &val);
        checked += 1;
    }
    checked
}

/// The table a GeoJSON file holds, by its name: `<table>.geojson`, or with a tile or
/// the merge prefix, `<prefix>_<table>.geojson`.
fn table_of(path: &Path) -> IsomTable {
    let stem = path.file_stem().unwrap().to_str().unwrap();
    *IsomTable::ALL
        .iter()
        .find(|t| stem == t.as_str() || stem.ends_with(&format!("_{}", t.as_str())))
        .unwrap_or_else(|| panic!("{} is not named after a table", path.display()))
}

/// Every feature's `isom_code` is a code of the isom-maplibre symbol table (a `stack`
/// entry of the vendored `isom.yaml`), and the file it was written to is that code's
/// table: the style draws every feature, from the source it reads the code from.
fn assert_table_conformance(path: &Path, collection: &Value) {
    let table = table_of(path);
    for f in collection["features"].as_array().unwrap() {
        let code = f["properties"]["isom_code"].as_str().unwrap_or_default();
        let code: IsomCode = code
            .parse()
            .unwrap_or_else(|e| panic!("{}: {e}: {f}", path.display()));
        assert_eq!(
            code.table(),
            table,
            "{}: {code} belongs to {}",
            path.display(),
            code.table().as_str()
        );
    }
}

/// Every file under `dir`, recursively, in name order.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The OSM features of a table (those with a `category`): only the keys `isom_code`,
/// `category` and `upper_level`, the last either absent or true.
fn osm_features(path: &Path) -> Vec<Value> {
    let features: Vec<Value> = feature_collection(path)
        .into_iter()
        .filter(|f| f["properties"].get("category").is_some())
        .collect();
    for f in &features {
        let p = f["properties"].as_object().unwrap();
        assert!(
            p.keys()
                .all(|k| ["isom_code", "category", "upper_level"].contains(&k.as_str())),
            "{}: {f}",
            path.display()
        );
        assert!(p["category"].is_string(), "{}: {f}", path.display());
        assert!(
            p.get("upper_level").is_none() || p["upper_level"] == true,
            "{}: {f}",
            path.display()
        );
    }
    features
}

/// The template's `vector_greenshade_isom`: the symbol code per greenshade index (1-based;
/// the last code repeats for higher indices).
fn greenshade_isom() -> Vec<String> {
    let template = include_str!("../pullauta.default.ini");
    let line = template
        .lines()
        .find_map(|l| l.strip_prefix("vector_greenshade_isom="))
        .unwrap();
    line.split('|').map(|c| c.trim().to_string()).collect()
}

/// Check a feature's `shade` against its code: a green area (a code in `map`, the
/// template's 406/408/410) has its greenshade index as an integer, mapped to its code
/// by `vector_greenshade_isom`; every other feature has none.
fn assert_shade(f: &Value, map: &[String]) {
    let p = &f["properties"];
    match p["isom_code"].as_str().unwrap_or_default() {
        code if map.iter().any(|m| m == code) && p.get("category").is_none() => {
            let shade = p["shade"]
                .as_u64()
                .unwrap_or_else(|| panic!("no shade: {f}"));
            assert!(shade >= 1, "{f}");
            let index = (shade as usize).min(map.len()) - 1;
            assert_eq!(map[index], code, "{f}");
        }
        _ => assert!(p.get("shade").is_none(), "{f}"),
    }
}

/// Check the vegetation areas of a `vegetation_areas` table (those without a
/// `category`): Polygons whose rings are closed, and properties that are only a
/// vegetation `isom_code`, plus the greenshade index on green areas when the run had
/// `vector_shade=1` (see [`assert_shade`]). Returns the codes present.
fn assert_vegetation_features(path: &Path, vector_shade: bool) -> BTreeSet<String> {
    let map = greenshade_isom();
    let mut codes = BTreeSet::new();
    let features = feature_collection(path)
        .into_iter()
        .filter(|f| f["properties"].get("category").is_none());
    for f in features {
        assert_eq!(f["geometry"]["type"], "Polygon", "{}: {f}", path.display());
        for ring in f["geometry"]["coordinates"].as_array().unwrap() {
            let ring = ring.as_array().unwrap();
            assert!(ring.len() >= 4, "{}: {f}", path.display());
            assert_eq!(ring.first(), ring.last(), "{}: {f}", path.display());
        }
        let p = f["properties"].as_object().unwrap();
        assert!(
            p.keys().all(|k| k == "isom_code" || k == "shade"),
            "{}: {f}",
            path.display()
        );
        if vector_shade {
            assert_shade(&f, &map);
        } else {
            assert!(p.get("shade").is_none(), "{}: {f}", path.display());
        }
        let code = p["isom_code"].as_str().unwrap();
        assert!(
            ["403.000", "406.000", "407.000", "408.000", "410.000"].contains(&code),
            "{}: {f}",
            path.display()
        );
        codes.insert(code.to_string());
    }
    codes
}

/// Check a terrain table: every feature has the expected geometry type and a code from
/// `allowed`. Returns the features.
fn assert_terrain_features(path: &Path, geometry: &str, allowed: &[&str]) -> Vec<Value> {
    let features = feature_collection(path);
    for f in &features {
        assert_eq!(f["geometry"]["type"], geometry, "{}: {f}", path.display());
        let code = f["properties"]["isom_code"].as_str().unwrap_or_default();
        assert!(allowed.contains(&code), "{}: {f}", path.display());
    }
    features
}

fn codes(features: &[Value]) -> BTreeSet<String> {
    features
        .iter()
        .map(|f| f["properties"]["isom_code"].as_str().unwrap().to_string())
        .collect()
}

fn table_path(folder: &Path, table: IsomTable) -> PathBuf {
    folder.join(geojson::file_name(table))
}

/// Check the terrain tables a tile's temp folder holds with vector_vege=1: contours with
/// the renderer's form lines, knoll points and cliffs.
fn assert_terrain_outputs(tile: &Path) {
    let contours = assert_terrain_features(
        &table_path(tile, IsomTable::Contours),
        "LineString",
        &["101.000", "101.001", "102.000", "103.000"],
    );
    let found = codes(&contours);
    for code in ["101.000", "102.000", "103.000"] {
        assert!(found.contains(code), "{found:?}");
    }
    // contours carry their level; the form lines, the renderer's selection of the
    // half-interval contours, have none, and the half-interval contours are left out
    for f in &contours {
        let form_line = f["properties"]["isom_code"] == "103.000";
        assert_eq!(f["properties"]["level_m"].is_number(), !form_line, "{f}");
    }
    // the regression tile has depressions; the flag is only ever true
    assert!(
        contours
            .iter()
            .any(|f| f["properties"]["depression"] == true)
    );
    assert!(contours.iter().all(|f| {
        let d = &f["properties"]["depression"];
        d.is_null() || *d == true
    }));

    let knolls = assert_terrain_features(
        &table_path(tile, IsomTable::KnollsPoints),
        "Point",
        &["109.000", "111.000"],
    );
    assert_eq!(
        codes(&knolls),
        ["109.000", "111.000"].map(String::from).into()
    );

    let cliffs = assert_terrain_features(
        &table_path(tile, IsomTable::Cliffs),
        "LineString",
        &["201.000", "202.000"],
    );
    // the regression tile has both cliff kinds
    assert_eq!(
        codes(&cliffs),
        ["201.000", "202.000"].map(String::from).into()
    );
}

/// Single job on the regression tile: run.sh's single job plus vector_vege=1. The
/// terrain and vegetation tables land in temp/, declaring the tile's CRS, the green
/// areas without `shade` (vector_shade=0).
#[test]
#[ignore]
fn single_job_writes_terrain_geojson() {
    let dir = run_single_job("e2e-single", &[]);

    let temp = dir.join("temp");
    assert_terrain_outputs(&temp);
    assert_crs(&table_path(&temp, IsomTable::Contours));
    for name in ["pullautus.png", "pullautus_depr.png"] {
        assert_raster_crs(&dir.join(name));
    }
    let vegetation = table_path(&temp, IsomTable::VegetationAreas);
    let green = assert_vegetation_features(&vegetation, false);
    assert!(green.contains("406.000"), "{green:?}");

    // contours, knolls_points, cliffs and vegetation_areas; no OSM tables without a
    // vectorconf
    assert_eq!(assert_schema_conformance(&dir), 4);
}

/// Run the single job on the regression tile in a fresh run directory `name`, with
/// vegetation vectorization on and the given extra settings. Returns the directory.
fn run_single_job(name: &str, settings: &[(&str, &str)]) -> PathBuf {
    let dir = run_dir(name);
    std::fs::copy(input("test_file.laz"), dir.join("test_file.laz")).unwrap();
    write_ini(&dir, &[&[("vector_vege", "1")], settings].concat());

    run_pullauta(&dir, &["test_file.laz"]);
    dir
}

/// Batch job as in regression/run.sh: one tile plus the OSM shapefile zip, with
/// `vectorconf=osm.txt`, and vegetation vectorization on, with the greenshade index
/// (`vector_shade=1`). `savetempfolders=1` keeps the
/// tile's temp folder as `temp_test_file_dir/`. `batchmerge=1` runs the merges and the
/// combined export into `out/`. `epsg` is unset: every GeoJSON file declares the EPSG
/// code the tile's GeoTIFF CRS keys name.
#[test]
#[ignore]
fn batch_with_osm_vectorconf() {
    let dir = run_batch_job("e2e-batch-osm", &[]);

    let tile = dir.join("temp_test_file_dir");

    // osm.txt rules, each feature in the table of its code: primary roads are wide
    // roads and paths small footpaths (paths), buildings and fences are manmade, lakes
    // water
    let osm: Vec<(IsomTable, Value)> = [IsomTable::Paths, IsomTable::Manmade, IsomTable::Water]
        .into_iter()
        .flat_map(|t| {
            osm_features(&table_path(&tile, t))
                .into_iter()
                .map(move |f| (t, f))
        })
        .collect();
    let has = |geometry: &str, code: &str, category: &str| {
        osm.iter().any(|(_, f)| {
            f["geometry"]["type"] == geometry
                && f["properties"]["isom_code"] == code
                && f["properties"]["category"] == category
        })
    };
    assert!(has("LineString", "502.000", "road-path"));
    assert!(has("LineString", "506.000", "road-path"));
    assert!(has("LineString", "516.000", "barrier"));
    assert!(has("Polygon", "521.000", "building"));
    assert!(has("Polygon", "301.000", "water"));
    for (_, f) in osm
        .iter()
        .filter(|(_, f)| f["geometry"]["type"] == "Polygon")
    {
        let rings = f["geometry"]["coordinates"].as_array().unwrap();
        assert!(!rings.is_empty(), "{f}");
        assert!(
            rings.iter().all(|r| r.as_array().unwrap().len() >= 4),
            "{f}"
        );
    }

    // vegetation: the default vector_greenshade_isom maps the greenshades to 406/408/410,
    // open land is 403, undergrowth 407; only the green areas carry a shade
    let vegetation =
        assert_vegetation_features(&table_path(&tile, IsomTable::VegetationAreas), true);
    assert!(
        vegetation.is_subset(
            &["403.000", "406.000", "407.000", "408.000", "410.000"]
                .map(String::from)
                .into()
        ),
        "{vegetation:?}"
    );
    for code in ["403.000", "406.000", "407.000"] {
        assert!(vegetation.contains(code), "{vegetation:?}");
    }

    // the same areas as closed DXF polylines, one DXF layer per symbol code
    let dxf_path = tile.join("vegetation.dxf");
    assert!(dxf_path.exists(), "{} was not written", dxf_path.display());
    let dxf = std::fs::read_to_string(dxf_path).unwrap();
    for code in &vegetation {
        assert!(
            dxf.contains(&format!(
                "POLYLINE\r\n 66\r\n1\r\n  8\r\n{code}\r\n 70\r\n1\r\n"
            )),
            "no closed polyline on layer {code}"
        );
    }
    assert!(tile.join("vegetation.dxf.bin").exists());

    assert_terrain_outputs(&tile);

    assert_batch_merge(&dir.join("out"));

    // every table in the thread's temp1/ and its savetempfolders copy, cropped, merged
    // and combined in out/
    assert_eq!(assert_schema_conformance(&dir), 5 * IsomTable::ALL.len());
}

/// Run the batch job of [`batch_with_osm_vectorconf`] in a fresh run directory `name`,
/// with the given extra settings. Returns the directory.
fn run_batch_job(name: &str, settings: &[(&str, &str)]) -> PathBuf {
    let dir = run_dir(name);
    for sub in ["in", "out"] {
        std::fs::create_dir(dir.join(sub)).unwrap();
    }
    for name in ["test_file.laz", "test_file.shp.zip"] {
        std::fs::copy(input(name), dir.join("in").join(name)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("osm.txt"),
        dir.join("osm.txt"),
    )
    .unwrap();
    let batch = [
        ("batch", "1"),
        ("vectorconf", "osm.txt"),
        ("vector_vege", "1"),
        ("vector_shade", "1"),
        ("output_dxf", "1"),
        ("savetempfolders", "1"),
        ("batchmerge", "1"),
    ];
    write_ini(&dir, &[&batch, settings].concat());

    run_pullauta(&dir, &[]);
    dir
}

/// Two runs of the same job write byte-identical files, also with thinning on
/// (`thinfactor`, `cliffthin` < 1: the samplings seeded from the tile name).
#[test]
#[ignore]
fn runs_are_deterministic() {
    let thin = [("thinfactor", "0.5"), ("cliffthin", "0.5")];
    assert_same_files(
        &run_single_job("e2e-determinism-single-a", &thin),
        &run_single_job("e2e-determinism-single-b", &thin),
    );
    assert_same_files(
        &run_batch_job("e2e-determinism-batch-a", &thin),
        &run_batch_job("e2e-determinism-batch-b", &thin),
    );
}

/// Check that `a` and `b` hold the same files with the same bytes.
fn assert_same_files(a: &Path, b: &Path) {
    let relative = |dir: &Path| -> Vec<PathBuf> {
        files(dir)
            .into_iter()
            .map(|p| p.strip_prefix(dir).unwrap().to_path_buf())
            .collect()
    };
    let names = relative(a);
    assert_eq!(names, relative(b));
    for name in &names {
        assert!(
            std::fs::read(a.join(name)).unwrap() == std::fs::read(b.join(name)).unwrap(),
            "{} differs between two runs",
            name.display()
        );
    }
}

/// The e2e settings and the template use the `vector_` keys: none of the fork's old key
/// names is left, so a run cannot silently fall back to a default.
#[test]
fn template_has_no_old_vector_keys() {
    let template = include_str!("../pullauta.default.ini");
    for old in ["vectorvege", "vegeshade", "greenshadeisom", "vegesimplify"] {
        assert!(
            template.lines().all(|l| ini_key(l) != Some(old)),
            "{old} is in pullauta.default.ini"
        );
    }
}

/// The regression tile's CRS (ETRS-TM35FIN), from its GeoTIFF ProjectedCSTypeGeoKey.
const EPSG: &str = "3067";

/// Check the GeoJSON `crs` member names [`EPSG`].
fn assert_crs(path: &Path) {
    let val: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        val["crs"]["properties"]["name"],
        format!("urn:ogc:def:crs:EPSG::{EPSG}"),
        "{}",
        path.display()
    );
}

/// Check the GDAL sidecar `<raster>.aux.xml` names [`EPSG`].
fn assert_raster_crs(raster: &Path) {
    let path = PathBuf::from(format!("{}.aux.xml", raster.display()));
    let xml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(xml.contains(&format!("<SRS>EPSG:{EPSG}</SRS>")), "{xml}");
}

/// Check the batch output folder after `batchmerge=1`: every table cropped per tile,
/// merged and combined, the combined DXF and CRT, and the merged rasters (not in the
/// working directory).
fn assert_batch_merge(out: &Path) {
    let mut combined = Vec::new();
    for &table in IsomTable::ALL {
        for name in [
            geojson::tile_file_name(table, "test_file"),
            geojson::merged_file_name(table),
            geojson::file_name(table),
        ] {
            let path = out.join(name);
            feature_collection(&path);
            assert_crs(&path);
        }
        combined.extend(feature_collection(&table_path(out, table)));
    }
    // (merged_vege.png needs savetempfiles=1, which writes the tile vegetation rasters)
    for name in [
        "test_file.png",
        "test_file_depr.png",
        "merged.png",
        "merged.jpg",
        "merged_depr.png",
    ] {
        assert_raster_crs(&out.join(name));
    }
    for name in ["merged.png", "merged.pgw", "merged_depr.png"] {
        assert!(out.join(name).exists(), "{name} is not in out/");
        assert!(
            !out.parent().unwrap().join(name).exists(),
            "{name} is in cwd"
        );
    }

    for name in [geojson::COMBINED_DXF, geojson::COMBINED_CRT] {
        assert!(out.join(name).exists(), "{name} was not written");
    }
    let found = codes(&combined);
    for code in [
        "101.000", "102.000", "103.000", // contours, form lines
        "109.000", "111.000", // knolls and small depressions
        "201.000", "202.000", // cliffs
        "403.000", "406.000", "407.000", // open land, vegetation, undergrowth
        "502.000", "521.000", "516.000", // OSM: wide road, building, fence
    ] {
        assert!(
            found.contains(code),
            "no {code} in the combined tables: {found:?}"
        );
    }
    // the green areas keep their shade through crop, merge and the combined export
    let map = greenshade_isom();
    for f in &combined {
        assert_shade(f, &map);
    }
    // OSM features keep their category; terrain and vegetation have none
    assert!(
        combined
            .iter()
            .any(|f| f["properties"]["category"] == "building")
    );
    for f in &combined {
        let geometry = f["geometry"]["type"].as_str().unwrap();
        assert!(
            ["Point", "LineString", "Polygon"].contains(&geometry),
            "{f}"
        );
    }

    // one DXF layer per symbol code, each mapped to the OCAD symbol of the same number
    let crt = std::fs::read_to_string(out.join(geojson::COMBINED_CRT)).unwrap();
    let dxf = std::fs::read_to_string(out.join(geojson::COMBINED_DXF)).unwrap();
    for code in &found {
        assert!(crt.contains(&format!("{code} {code}\n")), "{code}");
        assert!(dxf.contains(&format!("  8\r\n{code}\r\n")), "{code}");
    }
}
