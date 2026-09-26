//! End-to-end tests: run the pullauta binary on the regression inputs and check the
//! vector (GeoJSON) outputs.
//!
//! Ignored by default (slow, needs the test data). Run with:
//!   cargo test --release --test e2e_test -- --ignored
//!
//! Inputs are the regression tile and OSM shapefile zip (same as
//! .github/workflows/regression.yml). They are read from `$PULLAUTA_E2E_DATA` (a
//! directory holding `test_file.laz` and `test_file.shp.zip`) when set, otherwise
//! downloaded once into cargo's per-target temp directory. Each run works in a fresh
//! directory under that temp directory, never in the repository.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Write `pullauta.ini` into `dir`: the default template with the given `key=value`
/// lines replaced (every key must already be in the template).
fn write_ini(dir: &Path, settings: &[(&str, &str)]) {
    let template = include_str!("../pullauta.default.ini");
    let mut lines: Vec<String> = template.lines().map(String::from).collect();
    for (key, value) in settings {
        let prefix = format!("{key}=");
        let line = lines
            .iter_mut()
            .find(|l| l.starts_with(&prefix))
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

/// A plain ISOM symbol number ("502"). osm.txt uses no OOM sub-symbols ("501.2").
fn is_plain_symbol(s: &str) -> bool {
    s.len() == 3 && s.chars().all(|c| c.is_ascii_digit())
}

/// Check OSM features: the expected geometry type, a plain symbol number, a category, and
/// `upper_level` either absent or true.
fn assert_osm_features(path: &Path, geometry: &str) -> Vec<Value> {
    let features = feature_collection(path);
    for f in &features {
        let p = &f["properties"];
        assert_eq!(f["geometry"]["type"], geometry, "{}: {f}", path.display());
        let symbol = p["symbol"].as_str().unwrap_or_default();
        assert!(is_plain_symbol(symbol), "{}: {f}", path.display());
        assert!(p["category"].is_string(), "{}: {f}", path.display());
        assert!(
            p.get("upper_level").is_none() || p["upper_level"] == true,
            "{}: {f}",
            path.display()
        );
    }
    features
}

/// Batch job as in the regression workflow: one tile plus the OSM shapefile zip, with
/// `vectorconf=osm.txt`. `savetempfolders=1` keeps the tile's temp folder as
/// `temp_test_file_dir/`.
#[test]
#[ignore]
fn batch_with_osm_vectorconf() {
    let dir = run_dir("e2e-batch-osm");
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
    write_ini(
        &dir,
        &[
            ("batch", "1"),
            ("vectorconf", "osm.txt"),
            ("savetempfolders", "1"),
        ],
    );

    run_pullauta(&dir, &[]);

    let tile = dir.join("temp_test_file_dir");

    let has = |features: &[Value], symbol: &str, category: &str| {
        features
            .iter()
            .any(|f| f["properties"]["symbol"] == symbol && f["properties"]["category"] == category)
    };

    let lines = assert_osm_features(&tile.join("osm_lines.geojson"), "LineString");
    // osm.txt rules in ISOM 2017-2: primary roads are wide roads, paths small footpaths
    assert!(has(&lines, "502", "road-path"));
    assert!(has(&lines, "506", "road-path"));

    let areas = assert_osm_features(&tile.join("osm_areas.geojson"), "Polygon");
    assert!(has(&areas, "521", "building"));
    assert!(has(&areas, "301", "water"));
    for f in &areas {
        let rings = f["geometry"]["coordinates"].as_array().unwrap();
        assert!(!rings.is_empty(), "{f}");
        assert!(
            rings.iter().all(|r| r.as_array().unwrap().len() >= 4),
            "{f}"
        );
    }
}
