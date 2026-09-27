//! End-to-end test of `pullauta eval` on synthetic reference data: a trusted map
//! written as the pipeline's outputs (a classified vegetation raster and the
//! isom-maplibre tables), and a candidate that differs from it in known ways.
//! It runs the binary, so it also checks that eval creates no config file or
//! temp folder.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use image::{Rgba, RgbaImage};
use serde_json::{Value, json};

const GREEN: Rgba<u8> = Rgba([200, 254, 200, 255]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);

/// A fresh, empty directory named `name` under cargo's per-target temp directory.
fn fresh_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 10x10 vegetation raster, green in the first `green_columns` columns.
fn vegetation(green_columns: u32) -> RgbaImage {
    RgbaImage::from_fn(10, 10, |x, _| if x < green_columns { GREEN } else { WHITE })
}

fn line(code: &str, coords: Value) -> Value {
    json!({"type": "Feature", "properties": {"isom_code": code},
           "geometry": {"type": "LineString", "coordinates": coords}})
}

fn write_table(dir: &Path, table: &str, features: Vec<Value>) {
    let fc = json!({"type": "FeatureCollection", "features": features});
    std::fs::write(dir.join(format!("{table}.geojson")), fc.to_string()).unwrap();
}

/// The trusted map, or the candidate with its known differences.
fn write_map(dir: &Path, candidate: bool) {
    std::fs::create_dir_all(dir.join("temp")).unwrap();
    vegetation(if candidate { 6 } else { 5 })
        .save(dir.join("temp/vegetation.png"))
        .unwrap();
    let tables = dir.join("temp");
    let mut contours = vec![line(
        "101.000",
        json!([[0, if candidate { 0.5 } else { 0.0 }], [100, 0]]),
    )];
    if candidate {
        // a contour crossing the first: one topology error
        contours.push(line("101.000", json!([[50, -10], [50, 10]])));
    }
    write_table(&tables, "contours", contours);
    let cliff = if candidate {
        json!([[50, 20], [250, 20]])
    } else {
        json!([[0, 20], [100, 20]])
    };
    write_table(&tables, "cliffs", vec![line("201.000", cliff)]);
    let square = json!({"type": "Feature", "properties": {"isom_code": "406.000"},
        "geometry": {"type": "Polygon",
                     "coordinates": [[[0, 30], [10, 30], [10, 40], [0, 40], [0, 30]]]}});
    write_table(&tables, "vegetation_areas", vec![square]);
}

/// Run `pullauta eval <args>` in `cwd`.
fn eval(cwd: &Path, args: &[&Path], flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pullauta"))
        .current_dir(cwd)
        .arg("eval")
        .args(args)
        .args(flags)
        .output()
        .expect("failed to run pullauta")
}

fn json_report(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn eval_measures_a_candidate_against_a_synthetic_reference() {
    let root = fresh_dir("e2e-eval");
    let (reference, candidate, cwd) = (root.join("ref"), root.join("cand"), root.join("cwd"));
    write_map(&reference, false);
    write_map(&candidate, true);
    std::fs::create_dir_all(&cwd).unwrap();

    let out = eval(&cwd, &[&reference, &candidate], &["--format", "json"]);
    let r = json_report(&out);

    // per-class vegetation IoU: 50 green pixels grow to 60
    let vege = &r["rasters"]["temp/vegetation.png"];
    assert_eq!(vege["changed_pixels"], 10);
    assert_eq!(vege["classes"]["#c8fec8"]["iou"], 0.833333);
    assert_eq!(vege["classes"]["#ffffff"]["iou"], 0.8);

    // contours: a tilt of up to 0.5 m within the 1 m tolerance, and a crossing
    // contour whose far end is 10 m away
    let contours = &r["vectors"]["temp/contours.geojson"]["codes"]["101.000"];
    assert_eq!(contours["baseline"]["crossings"], 0);
    assert_eq!(contours["candidate"]["crossings"], 1);
    assert_eq!(contours["lines"]["recall"], 1.0);
    assert_eq!(contours["lines"]["hausdorff_m"], 10.0);

    // cliffs: length-weighted precision and recall of an overlapping line
    let cliffs = &r["vectors"]["temp/cliffs.geojson"]["codes"]["201.000"]["lines"];
    let (p, rc) = (
        cliffs["precision"].as_f64().unwrap(),
        cliffs["recall"].as_f64().unwrap(),
    );
    assert!((p - 51.0 / 200.0).abs() <= 0.0025, "{p}");
    assert!((rc - 51.0 / 100.0).abs() <= 0.005, "{rc}");

    // an unchanged table agrees fully
    let vege_areas = &r["vectors"]["temp/vegetation_areas.geojson"]["codes"]["406.000"];
    assert_eq!(vege_areas["baseline"], vege_areas["candidate"]);
    assert_eq!(vege_areas["boundaries"]["hausdorff_m"], 0.0);

    // the reference map as one file against the run's tables
    let one_file = root.join("reference.geojson");
    let features: Vec<Value> = ["contours", "cliffs", "vegetation_areas"]
        .iter()
        .flat_map(|t| {
            let text = std::fs::read_to_string(reference.join(format!("temp/{t}.geojson")));
            let fc: Value = serde_json::from_str(&text.unwrap()).unwrap();
            fc["features"].as_array().unwrap().clone()
        })
        .collect();
    let fc = json!({"type": "FeatureCollection", "features": features});
    std::fs::write(&one_file, fc.to_string()).unwrap();
    let merged = json_report(&eval(
        &cwd,
        &[&one_file, &candidate.join("temp")],
        &["--format", "json"],
    ));
    let codes = &merged["vectors"]["reference.geojson"]["codes"];
    assert_eq!(codes["101.000"], *contours);
    assert_eq!(codes["201.000"]["lines"], *cliffs);

    // the gates: a changed candidate fails, the reference against itself passes
    let gate = eval(&cwd, &[&reference, &candidate], &["--fail-on-change"]);
    assert_eq!(gate.status.code(), Some(2));
    let same = eval(&cwd, &[&reference, &reference], &["--fail-on-change"]);
    assert_eq!(same.status.code(), Some(0));

    // eval only reads its inputs
    let written: Vec<_> = std::fs::read_dir(&cwd).unwrap().collect();
    assert!(written.is_empty(), "eval wrote into its working directory");

    std::fs::remove_dir_all(&root).unwrap();
}
