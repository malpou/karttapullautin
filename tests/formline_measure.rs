//! The form-line measurement behind the per-interval `formlinesteepness` defaults
//! (`formlines::RELIEF_THRESHOLD_DEFAULTS`): a single job with debug_intermediates=1 at
//! each contour interval, then `select_form_lines` on its ground model and contours at a
//! sweep of thresholds, one Markdown table row per threshold.
//!
//! Ignored by default (slow, needs the regression tile). Run with:
//!   PULLAUTA_E2E_DATA=<dir with test_file.laz> \
//!   cargo test --release --test formline_measure -- --ignored --nocapture
//!
//! The columns are heuristics, not agreement with a reference map:
//! - km/km²: form-line length per ground model area;
//! - 103/(101+102): form-line length over contour length;
//! - candidates: the share of the half-interval lines' length kept;
//! - flat <2 % / <5 %: the share of the form-line length on ground whose gradient
//!   (central differences over +-3 cells) is under 2 % or 5 %;
//! - squeezed: the share of the form-line length where the touching test, at the trace
//!   interval, says it cannot stand apart from the contours.

use std::path::{Path, PathBuf};
use std::process::Command;

use pullauta::formlines::{FormLineParams, select_form_lines};
use pullauta::geometry::{BinaryDxf, Point2};
use pullauta::io::fs::local::LocalFileSystem;
use pullauta::io::heightmap::HeightMap;
use pullauta::mapframe::MapFrame;
use pullauta::merge::{ContourSet, FormLineMode};

const THRESHOLDS: [f64; 6] = [0.10, 0.15, 0.20, 0.25, 0.37, 0.50];

fn length(p: &[Point2]) -> f64 {
    p.windows(2).map(|w| w[0].distance(w[1])).sum()
}

/// Run the single job at `contour_interval` with the debug intermediates, in a fresh
/// directory under cargo's per-target temp directory; returns its `temp/`.
fn debug_run(contour_interval: f64) -> PathBuf {
    let data = std::env::var_os("PULLAUTA_E2E_DATA")
        .map(PathBuf::from)
        .expect("set PULLAUTA_E2E_DATA to a directory holding test_file.laz");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("formlines-{contour_interval}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    let template = include_str!("../pullauta.default.ini");
    let ini: Vec<String> = template
        .lines()
        .map(|l| match l.split_once('=') {
            Some(("debug_intermediates", _)) => "debug_intermediates=1".into(),
            Some(("contour_interval", _)) => format!("contour_interval={contour_interval}"),
            _ => l.to_string(),
        })
        .collect();
    std::fs::write(dir.join("pullauta.ini"), ini.join("\n") + "\n").unwrap();
    std::fs::copy(data.join("test_file.laz"), dir.join("test_file.laz")).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_pullauta"))
        .arg("test_file.laz")
        .current_dir(&dir)
        .env("RUST_LOG", "warn")
        .status()
        .expect("failed to execute pullauta");
    assert!(status.success());
    dir.join("temp")
}

fn measure(contour_interval: f64) {
    let temp = debug_run(contour_interval);
    let ground = HeightMap::from_file(&LocalFileSystem, temp.join("xyz2.hmap")).unwrap();
    let dxf = BinaryDxf::from_reader(&mut std::fs::File::open(temp.join("out2.dxf.bin")).unwrap())
        .unwrap();
    let contours = ContourSet::from_bindxf(dxf).unwrap();
    let g = &ground.grid;
    let (w, h) = (g.width(), g.height());
    let area_km2 = (w as f64 * ground.scale) * (h as f64 * ground.scale) / 1e6;

    let (mut contour_km, mut candidate_km) = (0.0, 0.0);
    for (line, &(class, _)) in contours.lines.iter() {
        let p: Vec<Point2> = line.iter().map(|q| Point2::new(q.x, q.y)).collect();
        match class.contour_kind() {
            Some(k) if k.half_interval() && !k.index() => candidate_km += length(&p) / 1000.0,
            Some(_) => contour_km += length(&p) / 1000.0,
            None => {}
        }
    }
    let trace = contour_interval / 2.0;
    let gradient = |x: usize, y: usize| {
        let gx = (g[(x + 3, y)] - g[(x - 3, y)]) / (6.0 * ground.scale);
        let gy = (g[(x, y + 3)] - g[(x, y - 3)]) / (6.0 * ground.scale);
        (gx * gx + gy * gy).sqrt()
    };
    let apart = |x: usize, y: usize| {
        let d = trace * 7.0 / 5.0;
        (g[(x - 1, y)] - g[(x + 1, y)]).abs() < trace
            && (g[(x, y - 1)] - g[(x, y + 1)]).abs() < trace
            && (g[(x, y)] - g[(x + 1, y + 1)]).abs() < d
            && (g[(x - 1, y - 1)] - g[(x + 1, y + 1)]).abs() < d
            && (g[(x + 1, y - 1)] - g[(x - 1, y + 1)]).abs() < d
    };

    println!(
        "\n{contour_interval} m: area {area_km2:.3} km², contours {contour_km:.2} km, \
         half-interval lines {candidate_km:.2} km\n"
    );
    println!(
        "| steepness | 103 lines | 103 km | km/km² | 103/(101+102) | candidates | flat <2 % | flat <5 % | squeezed |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let frame = MapFrame::default();
    for threshold in THRESHOLDS {
        let params = FormLineParams {
            form_lines: FormLineMode::Selective,
            frame,
            relief_threshold: threshold,
            addition_vertices: 17.0,
            minimum_gap_vertices: 30,
            label_depressions: false,
            remove_touching_contours: false,
            trace_interval_m: trace,
            ring_length_m: frame.ground_minima().ring_length_m,
        };
        let selection = select_form_lines(&contours, &ground, &params).unwrap();
        let (mut lines, mut km) = (0, 0.0);
        let (mut measured, mut flat2, mut flat5, mut squeezed) = (0.0, 0.0, 0.0, 0.0);
        for (p, _) in selection.lines.iter() {
            lines += 1;
            km += length(p) / 1000.0;
            // each segment by its first vertex, away from the model's edge
            for s in p.windows(2) {
                let x = ((s[0].x - ground.xoffset) / ground.scale).floor() as isize;
                let y = ((s[0].y - ground.yoffset) / ground.scale).floor() as isize;
                if x < 4 || y < 4 || x as usize + 4 >= w || y as usize + 4 >= h {
                    continue;
                }
                let (x, y) = (x as usize, y as usize);
                let l = s[0].distance(s[1]);
                measured += l;
                let gr = gradient(x, y);
                if gr < 0.02 {
                    flat2 += l;
                }
                if gr < 0.05 {
                    flat5 += l;
                }
                if !apart(x, y) {
                    squeezed += l;
                }
            }
        }
        println!(
            "| {threshold:.2} | {lines} | {km:.2} | {:.2} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
            km / area_km2,
            km / contour_km,
            km / candidate_km,
            flat2 / measured,
            flat5 / measured,
            squeezed / measured
        );
    }
}

#[test]
#[ignore]
fn measure_form_lines_at_both_isom_intervals() {
    measure(5.0);
    measure(2.5);
}
