//! The `eval` command: measure how one run's output differs from another's,
//! or from a reference map.
//!
//! `pullauta eval <baseline> <candidate>` takes two files or two directories.
//! Directories are walked recursively and files are paired by relative path:
//! `*.png` pairs get a pixel comparison ([`raster`]), `*.geojson` pairs get
//! per-symbol-code metrics ([`vector`]). The baseline can be the output of
//! the base branch or a reference map in the same formats, so the same
//! report serves "what did this change do" and "how close is this to a
//! trusted map".
//!
//! The JSON report is deterministic: keys are sorted, run-specific paths are
//! left out and every measure is rounded to six decimals, so a report can be
//! committed and compared byte for byte. `--fail-on-change` and `--expected`
//! turn it into a gate.

pub mod raster;
pub mod vector;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;

pub const USAGE: &str = "USAGE: pullauta eval <baseline> <candidate> [--tolerance <metres>] [--diff-dir <dir>] [--format text|json] [--fail-on-change] [--expected <report.json>]

Compares two files, or two directories paired by relative path.
  *.png      changed pixels; per-colour IoU when both hold at most 32 colours
  *.geojson  per symbol code (symbol, else isom, else layer): counts, length,
             area, crossings, line precision/recall/Hausdorff/mean distance and
             point precision/recall/Hausdorff
  --tolerance       match distance in metres for GeoJSON geometry (default 1)
  --diff-dir        write <name>.diff.png for every PNG pair that differs
  --format          text (default) or json
  --fail-on-change  exit with status 2 when anything differs
  --expected        exit with status 2 when anything differs and the JSON report
                    is not equal to this file";

/// Exit status of the `eval` command when the gate fails.
pub const EXIT_CHANGED: i32 = 2;

/// Round a measure to six decimals (micrometres, or parts per million), so
/// last-bit noise does not reach the report.
pub(crate) fn round6(x: f64) -> f64 {
    // adding 0.0 turns -0.0 into 0.0, so both print the same
    (x * 1e6).round() / 1e6 + 0.0
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub tolerance_m: f64,
    pub diff_dir: Option<PathBuf>,
    pub json: bool,
    pub fail_on_change: bool,
    pub expected: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tolerance_m: 1.0,
            diff_dir: None,
            json: false,
            fail_on_change: false,
            expected: None,
        }
    }
}

/// A metric result, or why it could not be computed for this pair.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Outcome<T> {
    Ok(T),
    Err { error: String },
}

impl<T> From<anyhow::Result<T>> for Outcome<T> {
    fn from(r: anyhow::Result<T>) -> Self {
        match r {
            Ok(v) => Outcome::Ok(v),
            Err(e) => Outcome::Err {
                error: format!("{e:#}"),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RasterEntry {
    #[serde(flatten)]
    pub comparison: raster::RasterComparison,
    /// Written where the command line said; kept out of the JSON report.
    #[serde(skip)]
    pub diff_image: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// The two inputs, shown in the text report only: the JSON report must
    /// not depend on where the runs were written.
    #[serde(skip)]
    pub baseline: PathBuf,
    #[serde(skip)]
    pub candidate: PathBuf,
    pub tolerance_m: f64,
    pub rasters: BTreeMap<String, Outcome<RasterEntry>>,
    pub vectors: BTreeMap<String, Outcome<BTreeMap<String, vector::CodeComparison>>>,
    /// Files of any kind present only in the baseline directory.
    pub baseline_only: Vec<String>,
    /// Files of any kind present only in the candidate directory.
    pub candidate_only: Vec<String>,
}

impl Report {
    /// True when any pair differs or fails to compare, or a PNG or GeoJSON
    /// file has no partner. Other unpaired files are listed but are not a
    /// change: eval cannot measure them.
    pub fn has_change(&self) -> bool {
        let measurable = |f: &String| kind(Path::new(f)).is_some();
        let raster_changed = |r: &Outcome<RasterEntry>| match r {
            Outcome::Ok(e) => e.comparison.changed_pixels > 0,
            Outcome::Err { .. } => true,
        };
        let vector_changed = |v: &Outcome<BTreeMap<String, vector::CodeComparison>>| match v {
            Outcome::Ok(codes) => codes.values().any(vector::CodeComparison::has_change),
            Outcome::Err { .. } => true,
        };
        self.baseline_only.iter().any(measurable)
            || self.candidate_only.iter().any(measurable)
            || self.rasters.values().any(raster_changed)
            || self.vectors.values().any(vector_changed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Raster,
    Vector,
}

fn kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some(Kind::Raster),
        "geojson" => Some(Kind::Vector),
        _ => None,
    }
}

/// Every file under `dir`, relative to it, with `/` separators.
fn walk(dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let full = dir.join(&rel);
        let entries =
            std::fs::read_dir(&full).with_context(|| format!("listing {}", full.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let parts: Vec<_> = path.iter().map(|p| p.to_string_lossy()).collect();
                out.insert(parts.join("/"));
            }
        }
    }
    Ok(out)
}

fn compare_raster(
    baseline: &Path,
    candidate: &Path,
    name: &str,
    diff_dir: Option<&Path>,
) -> anyhow::Result<RasterEntry> {
    // errors name the side, not the path, so the JSON report stays the same
    // wherever the runs were written
    let open = |p: &Path, side: &str| {
        image::open(p)
            .with_context(|| format!("reading the {side} image"))
            .map(|i| i.to_rgba8())
    };
    let (b, c) = (open(baseline, "baseline")?, open(candidate, "candidate")?);
    let comparison = raster::compare(&b, &c)?;
    let mut diff_image = None;
    if let Some(dir) = diff_dir
        && comparison.changed_pixels > 0
    {
        let path = dir.join(format!("{name}.diff.png"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        raster::diff_image(&b, &c)
            .save(&path)
            .context("writing the diff image")?;
        diff_image = Some(path);
    }
    Ok(RasterEntry {
        comparison,
        diff_image,
    })
}

fn compare_vector(
    baseline: &Path,
    candidate: &Path,
    tolerance: f64,
) -> anyhow::Result<BTreeMap<String, vector::CodeComparison>> {
    let b = vector::load(baseline).context("reading the baseline GeoJSON")?;
    let c = vector::load(candidate).context("reading the candidate GeoJSON")?;
    Ok(vector::compare(&b, &c, tolerance))
}

/// Compare `baseline` with `candidate` (two files, or two directories).
pub fn evaluate(baseline: &Path, candidate: &Path, opts: &Options) -> anyhow::Result<Report> {
    let mut report = Report {
        baseline: baseline.to_path_buf(),
        candidate: candidate.to_path_buf(),
        tolerance_m: opts.tolerance_m,
        rasters: BTreeMap::new(),
        vectors: BTreeMap::new(),
        baseline_only: Vec::new(),
        candidate_only: Vec::new(),
    };
    // (report key, baseline file, candidate file)
    let pairs: Vec<(String, PathBuf, PathBuf)> = match (baseline.is_dir(), candidate.is_dir()) {
        (true, true) => {
            let (b, c) = (walk(baseline)?, walk(candidate)?);
            report.baseline_only = b.difference(&c).cloned().collect();
            report.candidate_only = c.difference(&b).cloned().collect();
            b.intersection(&c)
                .filter(|f| kind(Path::new(f)).is_some())
                .map(|f| (f.clone(), baseline.join(f), candidate.join(f)))
                .collect()
        }
        (false, false) => {
            if kind(baseline).is_none() || kind(baseline) != kind(candidate) {
                bail!("both files must be .png or both .geojson");
            }
            let name = candidate
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            vec![(name, baseline.to_path_buf(), candidate.to_path_buf())]
        }
        _ => bail!("compare two files or two directories, not one of each"),
    };
    for (name, b, c) in pairs {
        match kind(&c) {
            Some(Kind::Raster) => {
                let entry = compare_raster(&b, &c, &name, opts.diff_dir.as_deref());
                report.rasters.insert(name, entry.into());
            }
            Some(Kind::Vector) => {
                let entry = compare_vector(&b, &c, opts.tolerance_m);
                report.vectors.insert(name, entry.into());
            }
            None => unreachable!("pairs hold only .png and .geojson files"),
        }
    }
    Ok(report)
}

/// Parse the arguments that follow `eval` on the command line.
pub fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, PathBuf, Options)> {
    let mut opts = Options::default();
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--tolerance" => {
                let v = value()?;
                opts.tolerance_m = v
                    .parse()
                    .ok()
                    .filter(|t: &f64| *t > 0.0 && t.is_finite())
                    .with_context(|| format!("--tolerance must be a positive number, got {v}"))?;
            }
            "--diff-dir" => opts.diff_dir = Some(PathBuf::from(value()?)),
            "--expected" => opts.expected = Some(PathBuf::from(value()?)),
            "--fail-on-change" => opts.fail_on_change = true,
            "--format" => {
                opts.json = match value()?.as_str() {
                    "json" => true,
                    "text" => false,
                    other => bail!("--format must be text or json, got {other}"),
                }
            }
            flag if flag.starts_with("--") => bail!("unknown option {flag}"),
            path => paths.push(PathBuf::from(path)),
        }
    }
    let [baseline, candidate]: [PathBuf; 2] = paths
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected a baseline and a candidate path"))?;
    Ok((baseline, candidate, opts))
}

/// Entry point for `pullauta eval ...`: prints the report to stdout and
/// returns whether the gate passed (always true without a gate option).
pub fn run(args: &[String]) -> anyhow::Result<bool> {
    let (baseline, candidate, opts) =
        parse_args(args).map_err(|e| anyhow::anyhow!("{e:#}\n\n{USAGE}"))?;
    let report = evaluate(&baseline, &candidate, &opts)?;
    // read the expected report first, so a bad path fails even without change
    let expected = match &opts.expected {
        Some(path) => {
            Some(vector::read_json(path).with_context(|| format!("reading {}", path.display()))?)
        }
        None => None,
    };
    let json = serde_json::to_value(&report)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print!("{}", text(&report));
    }
    if !report.has_change() {
        return Ok(true);
    }
    // --expected decides alone: it accepts exactly the committed change
    if let Some(expected) = expected {
        if expected == json {
            return Ok(true);
        }
        eprintln!("eval: the report differs from the expected report");
        return Ok(false);
    }
    if opts.fail_on_change {
        eprintln!("eval: the outputs differ");
        return Ok(false);
    }
    Ok(true)
}

/// Before and after, or just the value when both sides agree.
fn pair(b: f64, c: f64, decimals: usize) -> String {
    if b == c {
        format!("{b:.decimals$}")
    } else {
        format!("{b:.decimals$} -> {c:.decimals$}")
    }
}

/// A terminal-friendly rendering of the report.
pub fn text(r: &Report) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "baseline:  {}", r.baseline.display());
    let _ = writeln!(s, "candidate: {}", r.candidate.display());
    let _ = writeln!(s, "tolerance: {} m", r.tolerance_m);
    for (name, entry) in &r.rasters {
        let _ = write!(s, "\n{name}: ");
        match entry {
            Outcome::Err { error } => {
                let _ = writeln!(s, "error: {error}");
            }
            Outcome::Ok(e) => {
                let c = &e.comparison;
                if c.changed_pixels == 0 {
                    let _ = writeln!(s, "identical ({}x{})", c.width, c.height);
                    continue;
                }
                let _ = writeln!(
                    s,
                    "{} of {}x{} pixels changed ({:.4}%)",
                    c.changed_pixels, c.width, c.height, c.changed_percent
                );
                for (colour, k) in c.classes.iter().flatten() {
                    let _ = writeln!(
                        s,
                        "  {colour}  px {:>10}  IoU {:.4}",
                        pair(k.baseline_px as f64, k.candidate_px as f64, 0),
                        k.iou
                    );
                }
                if let Some(p) = &e.diff_image {
                    let _ = writeln!(s, "  diff image: {}", p.display());
                }
            }
        }
    }
    for (name, entry) in &r.vectors {
        let _ = writeln!(s, "\n{name}:");
        let codes = match entry {
            Outcome::Err { error } => {
                let _ = writeln!(s, "  error: {error}");
                continue;
            }
            Outcome::Ok(codes) => codes,
        };
        for (code, c) in codes {
            let (b, k) = (&c.baseline, &c.candidate); // k: candidate
            let _ = write!(
                s,
                "  {code:<8} features {}",
                pair(b.features as f64, k.features as f64, 0)
            );
            if b.points + k.points > 0 {
                let _ = write!(s, "  points {}", pair(b.points as f64, k.points as f64, 0));
            }
            if b.length_m + k.length_m > 0.0 {
                let _ = write!(s, "  length {} m", pair(b.length_m, k.length_m, 1));
            }
            if b.polygons + k.polygons > 0 {
                let _ = write!(s, "  area {} m2", pair(b.area_m2, k.area_m2, 1));
            }
            if b.crossings + k.crossings > 0 {
                let _ = write!(
                    s,
                    "  crossings {}",
                    pair(b.crossings as f64, k.crossings as f64, 0)
                );
            }
            let _ = writeln!(s);
            if let Some(l) = &c.lines {
                let _ = writeln!(
                    s,
                    "           lines  precision {:.4}  recall {:.4}  hausdorff {:.2} m  mean {:.3} / {:.3} m",
                    l.precision, l.recall, l.hausdorff_m, l.mean_distance_m, l.mean_distance_back_m
                );
            }
            if let Some(l) = &c.boundaries {
                let _ = writeln!(
                    s,
                    "           rings  precision {:.4}  recall {:.4}  hausdorff {:.2} m  mean {:.3} / {:.3} m",
                    l.precision, l.recall, l.hausdorff_m, l.mean_distance_m, l.mean_distance_back_m
                );
            }
            if let Some(p) = &c.points {
                let _ = writeln!(
                    s,
                    "           points precision {:.4}  recall {:.4}",
                    p.precision, p.recall
                );
            }
        }
    }
    for (label, files) in [
        ("only in baseline", &r.baseline_only),
        ("only in candidate", &r.candidate_only),
    ] {
        if !files.is_empty() {
            let _ = writeln!(s, "\n{label}: {}", files.join(", "));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_paths_and_options() {
        let (b, c, o) = parse_args(&args(
            "base cand --tolerance 2.5 --diff-dir d --format json --fail-on-change --expected e.json",
        ))
        .unwrap();
        assert_eq!((b, c), (PathBuf::from("base"), PathBuf::from("cand")));
        assert_eq!(
            o,
            Options {
                tolerance_m: 2.5,
                diff_dir: Some(PathBuf::from("d")),
                json: true,
                fail_on_change: true,
                expected: Some(PathBuf::from("e.json")),
            }
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            "only-one",
            "a b c",
            "a b --tolerance",
            "a b --tolerance -1",
            "a b --format xml",
            "a b --bogus",
            "a b --expected",
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad}");
        }
    }

    /// Two run directories: one identical PNG, one changed PNG (in a
    /// subdirectory), one GeoJSON whose contour moved, and one file per side
    /// with no partner.
    #[test]
    fn evaluates_two_directories() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-{}", std::process::id()));
        let (base, cand, diffs) = (root.join("base"), root.join("cand"), root.join("diffs"));
        for d in [&base, &cand] {
            std::fs::create_dir_all(d.join("temp")).unwrap();
        }
        let img = |v: u8| RgbaImage::from_pixel(4, 4, Rgba([v, v, v, 255]));
        img(0).save(base.join("same.png")).unwrap();
        img(0).save(cand.join("same.png")).unwrap();
        img(0).save(base.join("temp/vegetation.png")).unwrap();
        let mut changed = img(0);
        changed.put_pixel(0, 0, Rgba([9, 9, 9, 255]));
        changed.save(cand.join("temp/vegetation.png")).unwrap();
        let contour = |y: f64| {
            format!(
                r#"{{"type":"FeatureCollection","features":[{{"type":"Feature",
                "properties":{{"symbol":"101"}},
                "geometry":{{"type":"LineString","coordinates":[[0,{y}],[10,{y}]]}}}}]}}"#
            )
        };
        std::fs::write(base.join("out.geojson"), contour(0.0)).unwrap();
        std::fs::write(cand.join("out.geojson"), contour(2.0)).unwrap();
        std::fs::write(base.join("gone.geojson"), contour(0.0)).unwrap();
        std::fs::write(cand.join("new.png"), b"not a png").unwrap();

        let opts = Options {
            diff_dir: Some(diffs.clone()),
            ..Options::default()
        };
        let r = evaluate(&base, &cand, &opts).unwrap();

        let Outcome::Ok(same) = &r.rasters["same.png"] else {
            panic!("same.png failed");
        };
        assert_eq!(same.comparison.changed_pixels, 0);
        assert!(same.diff_image.is_none());
        let Outcome::Ok(vege) = &r.rasters["temp/vegetation.png"] else {
            panic!("vegetation.png failed");
        };
        assert_eq!(vege.comparison.changed_pixels, 1);
        assert!(diffs.join("temp/vegetation.png.diff.png").exists());
        let Outcome::Ok(codes) = &r.vectors["out.geojson"] else {
            panic!("out.geojson failed");
        };
        let lines = codes["101"].lines.unwrap();
        assert_eq!((lines.precision, lines.hausdorff_m), (0.0, 2.0));
        assert_eq!(r.baseline_only, ["gone.geojson"]);
        assert_eq!(r.candidate_only, ["new.png"]);

        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["rasters"]["same.png"]["changed_pixels"], 0);
        assert_eq!(
            json["vectors"]["out.geojson"]["101"]["baseline"]["length_m"],
            10.0
        );
        let t = text(&r);
        assert!(t.contains("same.png: identical (4x4)"), "{t}");
        assert!(t.contains("1 of 4x4 pixels changed"), "{t}");
        assert!(t.contains("hausdorff 2.00 m"), "{t}");

        // the JSON report leaves out run-specific paths
        assert!(json.get("baseline").is_none());
        assert!(
            json["rasters"]["temp/vegetation.png"]
                .get("diff_image")
                .is_none()
        );
        assert!(r.has_change());

        // a run compared with itself passes every gate
        let same = evaluate(&base, &base, &Options::default()).unwrap();
        assert!(!same.has_change());
        let dirs = format!("{} {}", base.display(), cand.display());
        assert!(
            run(&args(&format!(
                "{} {} --fail-on-change",
                base.display(),
                base.display()
            )))
            .unwrap()
        );
        assert!(!run(&args(&format!("{dirs} --fail-on-change"))).unwrap());
        // a changed run passes when its report was committed as expected
        let expected = root.join("expected.json");
        std::fs::write(&expected, serde_json::to_string_pretty(&json).unwrap()).unwrap();
        assert!(run(&args(&format!("{dirs} --expected {}", expected.display()))).unwrap());
        std::fs::write(&expected, "{}").unwrap();
        assert!(!run(&args(&format!("{dirs} --expected {}", expected.display()))).unwrap());
        // a missing expected report is an error even when nothing changed
        let missing = format!(
            "{0} {0} --expected {1}",
            base.display(),
            root.join("no.json").display()
        );
        assert!(run(&args(&missing)).is_err());

        // files eval cannot measure are listed but are not a change
        std::fs::create_dir_all(root.join("other")).unwrap();
        let only = evaluate(
            &root.join("base").join("temp"),
            &root.join("other"),
            &Options::default(),
        );
        let only = only.unwrap();
        assert_eq!(only.baseline_only, ["vegetation.png"]);
        assert!(only.has_change());
        std::fs::remove_file(root.join("base/temp/vegetation.png")).unwrap();
        std::fs::write(root.join("base/temp/x.pgw"), "1").unwrap();
        let unmeasured = evaluate(
            &root.join("base/temp"),
            &root.join("other"),
            &Options::default(),
        )
        .unwrap();
        assert_eq!(unmeasured.baseline_only, ["x.pgw"]);
        assert!(!unmeasured.has_change());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_pair_is_reported_not_fatal() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-bad-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a.png"), root.join("b.png"));
        std::fs::write(&a, b"junk").unwrap();
        std::fs::write(&b, b"junk").unwrap();
        let r = evaluate(&a, &b, &Options::default()).unwrap();
        assert!(matches!(r.rasters["b.png"], Outcome::Err { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
