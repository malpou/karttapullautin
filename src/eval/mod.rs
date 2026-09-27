//! The `eval` command: measure how one run's output differs from another's,
//! or from a reference map.
//!
//! `pullauta eval <baseline> <candidate>` takes two files or two directories.
//! Directories are walked recursively and files are paired by relative path:
//! `*.png` pairs get a pixel comparison ([`raster`]), `*.geojson` pairs get
//! per-symbol-code metrics ([`vector`]), and every other pair (world files,
//! DXF, `.aux.xml`, `.ocdCrt`, ...) is compared byte for byte. Pairing by path
//! covers every layout the pipeline writes: the tables in `temp/`
//! (`contours.geojson`, ...), a batch folder's `<tile>_<table>.geojson` and
//! `merged_<table>.geojson`, and the combined export's `<table>.geojson`.
//!
//! The baseline can be the output of the base branch or a reference map, so
//! the same report serves "what did this change do" and "how close is this to
//! a trusted map". A reference map in one GeoJSON file, holding every table's
//! codes, can be compared with a directory: the directory's `<table>.geojson`
//! files are read as one map.
//!
//! The JSON report is deterministic: keys are sorted, run-specific paths are
//! left out and every measure is rounded to six decimals, so a report can be
//! committed and compared byte for byte. `--fail-on-change` and `--expected`
//! turn it into a gate.

pub mod raster;
pub mod vector;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::geojson;
use crate::io::fs::FileSystem;
use crate::isom::IsomTable;

pub const USAGE: &str = "USAGE: pullauta eval <baseline> <candidate> [--tolerance <metres>] [--diff-dir <dir>] [--format text|json] [--ignore <suffix>]... [--fail-on-change] [--expected <report.json>]

Compares two files, two directories paired by relative path, or one GeoJSON
file with a directory's <table>.geojson files read as one map.
  *.png      changed pixels; per-colour IoU when both hold at most 32 colours
  *.geojson  per symbol code (isom_code): counts, length, area, crossings,
             unmatched properties, line precision/recall/Hausdorff/mean
             distance and point precision/recall/Hausdorff; unknown codes
             are counted
  other      byte comparison
  --tolerance       match distance in metres for GeoJSON geometry (default 1,
                    at least 0.01)
  --diff-dir        write <name>.diff.png for every PNG pair that differs
  --format          text (default) or json
  --ignore          skip files whose relative path ends with this (repeatable),
                    such as log.txt
  --fail-on-change  exit with status 2 when anything differs or cannot be read
  --expected        exit with status 2 when anything cannot be read, or differs
                    and the JSON report is not equal to this file
Exit status: 0 passed (always, without a gate option), 2 the gate failed,
1 an error (bad arguments, nothing to compare, an unreadable directory or
expected report).";

/// Exit status of the `eval` command when the gate fails.
pub const EXIT_CHANGED: i32 = 2;

/// The smallest `--tolerance`: finer than any coordinate the pipeline writes
/// means something, and it bounds the sample count.
pub const MIN_TOLERANCE_M: f64 = 0.01;

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
    /// Relative-path suffixes of files left out of a directory comparison.
    pub ignore: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tolerance_m: 1.0,
            diff_dir: None,
            json: false,
            fail_on_change: false,
            expected: None,
            ignore: Vec::new(),
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

impl<T> Outcome<T> {
    fn is_err(&self) -> bool {
        matches!(self, Outcome::Err { .. })
    }

    /// True when the pair failed to compare or `changed` holds.
    fn changed(&self, changed: impl Fn(&T) -> bool) -> bool {
        match self {
            Outcome::Ok(v) => changed(v),
            Outcome::Err { .. } => true,
        }
    }
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

/// A byte comparison of a pair eval cannot measure otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FileEntry {
    pub identical: bool,
    pub baseline_bytes: u64,
    pub candidate_bytes: u64,
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
    pub vectors: BTreeMap<String, Outcome<vector::VectorComparison>>,
    /// Every other pair, compared byte for byte.
    pub files: BTreeMap<String, Outcome<FileEntry>>,
    /// Files present only in the baseline directory.
    pub baseline_only: Vec<String>,
    /// Files present only in the candidate directory.
    pub candidate_only: Vec<String>,
}

impl Report {
    /// True when any pair differs or fails to compare, or any file has no
    /// partner.
    pub fn has_change(&self) -> bool {
        !self.baseline_only.is_empty()
            || !self.candidate_only.is_empty()
            || (self.rasters.values()).any(|r| r.changed(|e| e.comparison.changed_pixels > 0))
            || (self.vectors.values()).any(|v| v.changed(vector::VectorComparison::has_change))
            || (self.files.values()).any(|f| f.changed(|f| !f.identical))
    }

    /// True when any pair could not be compared.
    pub fn has_error(&self) -> bool {
        self.rasters.values().any(Outcome::is_err)
            || self.vectors.values().any(Outcome::is_err)
            || self.files.values().any(Outcome::is_err)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Raster,
    Vector,
    Bytes,
}

fn kind(path: &Path) -> Kind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png") => Kind::Raster,
        Some("geojson") => Kind::Vector,
        _ => Kind::Bytes,
    }
}

/// Every file under `dir`, relative to it, with `/` separators. Walks with
/// `std::fs`: [`FileSystem`] cannot tell a directory from a file.
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

/// Read and parse a JSON file.
fn read_json(fs: &impl FileSystem, path: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&fs.read_to_string(path)?)?)
}

fn read_png(fs: &impl FileSystem, path: &Path) -> anyhow::Result<image::RgbaImage> {
    let mut reader = image::ImageReader::new(fs.open(path)?);
    reader.set_format(image::ImageFormat::Png);
    Ok(reader.decode()?.to_rgba8())
}

fn compare_raster(
    fs: &impl FileSystem,
    baseline: &Path,
    candidate: &Path,
    name: &str,
    diff_dir: Option<&Path>,
) -> anyhow::Result<RasterEntry> {
    // errors name the side, not the path, so the JSON report stays the same
    // wherever the runs were written
    let b = read_png(fs, baseline).context("reading the baseline image")?;
    let c = read_png(fs, candidate).context("reading the candidate image")?;
    let comparison = raster::compare(&b, &c)?;
    let mut diff_image = None;
    if let Some(dir) = diff_dir
        && comparison.changed_pixels > 0
    {
        let path = dir.join(format!("{name}.diff.png"));
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        {
            let mut out = fs.create(&path).context("writing the diff image")?;
            raster::diff_image(&b, &c)
                .write_to(&mut out, image::ImageFormat::Png)
                .context("writing the diff image")?;
        }
        diff_image = Some(path);
    }
    Ok(RasterEntry {
        comparison,
        diff_image,
    })
}

fn compare_bytes(
    fs: &impl FileSystem,
    baseline: &Path,
    candidate: &Path,
) -> anyhow::Result<FileEntry> {
    let size = |p: &Path, side: &str| {
        fs.file_size(p)
            .with_context(|| format!("reading the {side} file"))
    };
    let (baseline_bytes, candidate_bytes) =
        (size(baseline, "baseline")?, size(candidate, "candidate")?);
    let mut identical = baseline_bytes == candidate_bytes;
    if identical {
        let (mut b, mut c) = (fs.open(baseline)?, fs.open(candidate)?);
        let (mut bb, mut cb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
        loop {
            let n = b.read(&mut bb)?;
            if n == 0 {
                break;
            }
            c.read_exact(&mut cb[..n])?;
            if bb[..n] != cb[..n] {
                identical = false;
                break;
            }
        }
    }
    Ok(FileEntry {
        identical,
        baseline_bytes,
        candidate_bytes,
    })
}

/// One side of a GeoJSON pair: a file, or a directory whose tables are read
/// as one map.
enum VectorSide<'a> {
    File(&'a Path),
    Tables(&'a Path),
}

impl VectorSide<'_> {
    fn load(&self, fs: &impl FileSystem, side: &str) -> anyhow::Result<vector::MapGeometry> {
        let read = |path: &Path| read_json(fs, path).and_then(|v| vector::group_by_code(&v));
        match self {
            VectorSide::File(path) => {
                read(path).with_context(|| format!("reading the {side} GeoJSON"))
            }
            VectorSide::Tables(dir) => {
                let mut all = vector::MapGeometry::default();
                let mut found = false;
                for &table in IsomTable::ALL {
                    let path = dir.join(geojson::file_name(table));
                    if fs.exists(&path) {
                        let map = read(&path).with_context(|| {
                            format!("reading the {side} {}", geojson::file_name(table))
                        })?;
                        all.append(map);
                        found = true;
                    }
                }
                ensure!(found, "the {side} directory holds no <table>.geojson");
                Ok(all)
            }
        }
    }
}

fn compare_vector(
    fs: &impl FileSystem,
    baseline: VectorSide,
    candidate: VectorSide,
    tolerance: f64,
) -> anyhow::Result<vector::VectorComparison> {
    let b = baseline.load(fs, "baseline")?;
    let c = candidate.load(fs, "candidate")?;
    vector::compare(&b, &c, tolerance)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Compare `baseline` with `candidate` (two files, two directories, or a
/// GeoJSON file and a directory of tables).
pub fn evaluate(
    fs: &impl FileSystem,
    baseline: &Path,
    candidate: &Path,
    opts: &Options,
) -> anyhow::Result<Report> {
    let mut report = Report {
        baseline: baseline.to_path_buf(),
        candidate: candidate.to_path_buf(),
        tolerance_m: opts.tolerance_m,
        rasters: BTreeMap::new(),
        vectors: BTreeMap::new(),
        files: BTreeMap::new(),
        baseline_only: Vec::new(),
        candidate_only: Vec::new(),
    };
    // (report key, kind, baseline file, candidate file)
    let pairs: Vec<(String, Kind, PathBuf, PathBuf)> = match (baseline.is_dir(), candidate.is_dir())
    {
        (true, true) => {
            let kept = |files: BTreeSet<String>| -> BTreeSet<String> {
                files
                    .into_iter()
                    .filter(|f| !opts.ignore.iter().any(|s| f.ends_with(s.as_str())))
                    .collect()
            };
            let (b, c) = (kept(walk(baseline)?), kept(walk(candidate)?));
            // a run that crashed before writing any map output must not pass
            ensure!(
                b.union(&c).any(|f| kind(Path::new(f)) != Kind::Bytes),
                "nothing to compare: neither directory holds a .png or .geojson file"
            );
            report.baseline_only = b.difference(&c).cloned().collect();
            report.candidate_only = c.difference(&b).cloned().collect();
            b.intersection(&c)
                .map(|f| {
                    (
                        f.clone(),
                        kind(Path::new(f)),
                        baseline.join(f),
                        candidate.join(f),
                    )
                })
                .collect()
        }
        (false, false) => {
            let k = kind(baseline);
            if k == Kind::Bytes || k != kind(candidate) {
                bail!("both files must be .png or both .geojson");
            }
            vec![(
                file_name(candidate),
                k,
                baseline.to_path_buf(),
                candidate.to_path_buf(),
            )]
        }
        // one GeoJSON map against a directory's tables, keyed by the file
        (baseline_is_dir, _) => {
            let (file, b, c) = if baseline_is_dir {
                (
                    candidate,
                    VectorSide::Tables(baseline),
                    VectorSide::File(candidate),
                )
            } else {
                (
                    baseline,
                    VectorSide::File(baseline),
                    VectorSide::Tables(candidate),
                )
            };
            if kind(file) != Kind::Vector {
                bail!("a file compared with a directory must be .geojson");
            }
            let entry = compare_vector(fs, b, c, opts.tolerance_m);
            report.vectors.insert(file_name(file), entry.into());
            return Ok(report);
        }
    };
    for (name, k, b, c) in pairs {
        match k {
            Kind::Raster => {
                let entry = compare_raster(fs, &b, &c, &name, opts.diff_dir.as_deref());
                report.rasters.insert(name, entry.into());
            }
            Kind::Vector => {
                let entry = compare_vector(
                    fs,
                    VectorSide::File(&b),
                    VectorSide::File(&c),
                    opts.tolerance_m,
                );
                report.vectors.insert(name, entry.into());
            }
            Kind::Bytes => {
                report.files.insert(name, compare_bytes(fs, &b, &c).into());
            }
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
                    .filter(|t: &f64| *t >= MIN_TOLERANCE_M && t.is_finite())
                    .with_context(|| {
                        format!(
                            "--tolerance must be a number of at least {MIN_TOLERANCE_M} m, got {v}"
                        )
                    })?;
            }
            "--diff-dir" => opts.diff_dir = Some(PathBuf::from(value()?)),
            "--expected" => opts.expected = Some(PathBuf::from(value()?)),
            "--ignore" => opts.ignore.push(value()?.clone()),
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

/// True when a JSON report records a pair that could not be compared.
fn records_error(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("error") || m.values().any(records_error),
        Value::Array(a) => a.iter().any(records_error),
        _ => false,
    }
}

/// Entry point for `pullauta eval ...`: prints the report to stdout and
/// returns whether the gate passed (always true without a gate option).
pub fn run(args: &[String]) -> anyhow::Result<bool> {
    let fs = crate::io::fs::local::LocalFileSystem;
    let (baseline, candidate, opts) =
        parse_args(args).map_err(|e| anyhow::anyhow!("{e:#}\n\n{USAGE}"))?;
    let report = evaluate(&fs, &baseline, &candidate, &opts)?;
    // read the expected report first, so a bad path fails even without change
    let expected = match &opts.expected {
        Some(path) => {
            let v = read_json(&fs, path).with_context(|| format!("reading {}", path.display()))?;
            ensure!(
                !records_error(&v),
                "{} records a pair that could not be compared; an expected report cannot accept an error",
                path.display()
            );
            Some(v)
        }
        None => None,
    };
    let json = serde_json::to_value(&report)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print!("{}", text(&report));
    }
    let gated = opts.fail_on_change || expected.is_some();
    if gated && report.has_error() {
        eprintln!("eval: a pair could not be compared");
        return Ok(false);
    }
    if !report.has_change() {
        if let Some(path) = &opts.expected {
            eprintln!(
                "eval: warning: nothing changed, so {} is stale; delete it",
                path.display()
            );
        }
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
fn before_after(b: f64, c: f64, decimals: usize) -> String {
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
                for (colour, class) in c.classes.iter().flatten() {
                    let _ = writeln!(
                        s,
                        "  {colour}  px {:>10}  IoU {:.4}",
                        before_after(class.baseline_px as f64, class.candidate_px as f64, 0),
                        class.iou
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
        let v = match entry {
            Outcome::Err { error } => {
                let _ = writeln!(s, "  error: {error}");
                continue;
            }
            Outcome::Ok(v) => v,
        };
        if v.collection_changed {
            let _ = writeln!(s, "  collection members (such as crs) differ");
        }
        for (code, u) in &v.unknown_codes {
            let _ = writeln!(
                s,
                "  unknown isom_code {code}: features {}",
                before_after(u.baseline as f64, u.candidate as f64, 0)
            );
        }
        for (code, cmp) in &v.codes {
            let (b, c) = (&cmp.baseline, &cmp.candidate);
            let _ = write!(
                s,
                "  {:<8} features {}",
                code.as_str(),
                before_after(b.features as f64, c.features as f64, 0)
            );
            if b.points + c.points > 0 {
                let _ = write!(
                    s,
                    "  points {}",
                    before_after(b.points as f64, c.points as f64, 0)
                );
            }
            if b.length_m + c.length_m > 0.0 {
                let _ = write!(s, "  length {} m", before_after(b.length_m, c.length_m, 1));
            }
            if b.polygons + c.polygons > 0 {
                let _ = write!(s, "  area {} m2", before_after(b.area_m2, c.area_m2, 1));
            }
            if b.crossings + c.crossings > 0 {
                let _ = write!(
                    s,
                    "  crossings {}",
                    before_after(b.crossings as f64, c.crossings as f64, 0)
                );
            }
            if cmp.properties_unmatched > 0 {
                let _ = write!(s, "  unmatched properties {}", cmp.properties_unmatched);
            }
            let _ = writeln!(s);
            if let Some(l) = &cmp.lines {
                let _ = writeln!(
                    s,
                    "           lines  precision {:.4}  recall {:.4}  hausdorff {:.2} m  mean {:.3} / {:.3} m",
                    l.precision, l.recall, l.hausdorff_m, l.mean_distance_m, l.mean_distance_back_m
                );
            }
            if let Some(l) = &cmp.boundaries {
                let _ = writeln!(
                    s,
                    "           rings  precision {:.4}  recall {:.4}  hausdorff {:.2} m  mean {:.3} / {:.3} m",
                    l.precision, l.recall, l.hausdorff_m, l.mean_distance_m, l.mean_distance_back_m
                );
            }
            if let Some(p) = &cmp.points {
                let _ = writeln!(
                    s,
                    "           points precision {:.4}  recall {:.4}",
                    p.precision, p.recall
                );
            }
        }
    }
    let mut identical = 0;
    for (name, entry) in &r.files {
        match entry {
            Outcome::Ok(f) if f.identical => identical += 1,
            Outcome::Ok(f) => {
                let _ = writeln!(
                    s,
                    "\n{name}: bytes differ ({} -> {} bytes)",
                    f.baseline_bytes, f.candidate_bytes
                );
            }
            Outcome::Err { error } => {
                let _ = writeln!(s, "\n{name}: error: {error}");
            }
        }
    }
    if identical > 0 {
        let _ = writeln!(s, "\n{identical} other files identical");
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
    use crate::io::fs::local::LocalFileSystem;
    use crate::isom::IsomCode;
    use image::{Rgba, RgbaImage};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_paths_and_options() {
        let (b, c, o) = parse_args(&args(
            "base cand --tolerance 2.5 --diff-dir d --format json --fail-on-change --expected e.json --ignore log.txt --ignore .ini",
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
                ignore: vec!["log.txt".into(), ".ini".into()],
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
            "a b --tolerance 0.001",
            "a b --tolerance inf",
            "a b --ignore",
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
                "properties":{{"isom_code":"101.000"}},
                "geometry":{{"type":"LineString","coordinates":[[0,{y}],[10,{y}]]}}}}]}}"#
            )
        };
        std::fs::write(base.join("temp/contours.geojson"), contour(0.0)).unwrap();
        std::fs::write(cand.join("temp/contours.geojson"), contour(2.0)).unwrap();
        std::fs::write(base.join("cliffs.geojson"), contour(0.0)).unwrap();
        std::fs::write(cand.join("new.png"), b"not a png").unwrap();

        let opts = Options {
            diff_dir: Some(diffs.clone()),
            ..Options::default()
        };
        let r = evaluate(&LocalFileSystem, &base, &cand, &opts).unwrap();

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
        let Outcome::Ok(contours) = &r.vectors["temp/contours.geojson"] else {
            panic!("contours.geojson failed");
        };
        let lines = contours.codes[&IsomCode::C101_000].lines.unwrap();
        assert_eq!((lines.precision, lines.hausdorff_m), (0.0, 2.0));
        assert_eq!(r.baseline_only, ["cliffs.geojson"]);
        assert_eq!(r.candidate_only, ["new.png"]);

        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["rasters"]["same.png"]["changed_pixels"], 0);
        assert_eq!(
            json["vectors"]["temp/contours.geojson"]["codes"]["101.000"]["baseline"]["length_m"],
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
        let same = evaluate(&LocalFileSystem, &base, &base, &Options::default()).unwrap();
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

        // an unpaired file of any kind is a change
        std::fs::create_dir_all(root.join("other")).unwrap();
        let only = evaluate(
            &LocalFileSystem,
            &root.join("base").join("temp"),
            &root.join("other"),
            &Options::default(),
        );
        let only = only.unwrap();
        assert_eq!(only.baseline_only, ["contours.geojson", "vegetation.png"]);
        assert!(only.has_change());
        std::fs::write(root.join("base/temp/x.pgw"), "1").unwrap();
        std::fs::write(root.join("other/x.pgw"), "1").unwrap();
        std::fs::write(root.join("other/y.pgw"), "1").unwrap();
        let pgw = evaluate(
            &LocalFileSystem,
            &root.join("base/temp"),
            &root.join("other"),
            &Options::default(),
        )
        .unwrap();
        assert!(matches!(
            pgw.files["x.pgw"],
            Outcome::Ok(FileEntry {
                identical: true,
                ..
            })
        ));
        assert_eq!(pgw.candidate_only, ["y.pgw"]);

        // nothing to compare is an error, not a pass: a run that crashed
        std::fs::remove_file(root.join("base/temp/vegetation.png")).unwrap();
        std::fs::remove_file(root.join("base/temp/contours.geojson")).unwrap();
        let crashed = evaluate(
            &LocalFileSystem,
            &root.join("base/temp"),
            &root.join("other"),
            &Options::default(),
        );
        assert!(crashed.is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// World files and other outputs are compared byte for byte; `--ignore`
    /// leaves files out.
    #[test]
    fn other_files_are_compared_byte_for_byte() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-bytes-{}", std::process::id()));
        let (base, cand) = (root.join("base"), root.join("cand"));
        for d in [&base, &cand] {
            std::fs::create_dir_all(d).unwrap();
            RgbaImage::new(2, 2).save(d.join("map.png")).unwrap();
            std::fs::write(d.join("same.dxf"), "0\nEOF\n").unwrap();
        }
        std::fs::write(base.join("map.pgw"), "1.0\n0\n").unwrap();
        std::fs::write(cand.join("map.pgw"), "1.5\n0\n").unwrap();
        std::fs::write(base.join("log.txt"), "12:00").unwrap();
        std::fs::write(cand.join("log.txt"), "12:01").unwrap();
        let r = evaluate(&LocalFileSystem, &base, &cand, &Options::default()).unwrap();
        let Outcome::Ok(pgw) = r.files["map.pgw"] else {
            panic!("map.pgw failed");
        };
        assert_eq!(
            pgw,
            FileEntry {
                identical: false,
                baseline_bytes: 6,
                candidate_bytes: 6
            }
        );
        assert!(r.has_change());
        assert!(text(&r).contains("map.pgw: bytes differ"), "{}", text(&r));

        let opts = Options {
            ignore: vec!["log.txt".into(), ".pgw".into()],
            ..Options::default()
        };
        let r = evaluate(&LocalFileSystem, &base, &cand, &opts).unwrap();
        assert_eq!(r.files.keys().collect::<Vec<_>>(), ["same.dxf"]);
        assert!(!r.has_change());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An unreadable pair fails every gate, and an expected report cannot
    /// accept one.
    #[test]
    fn errors_fail_the_gates() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-err-{}", std::process::id()));
        let (base, cand) = (root.join("base"), root.join("cand"));
        for d in [&base, &cand] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("broken.geojson"), "{").unwrap();
        }
        let dirs = format!("{} {}", base.display(), cand.display());
        assert!(
            run(&args(&dirs)).unwrap(),
            "without a gate eval only reports"
        );
        assert!(!run(&args(&format!("{dirs} --fail-on-change"))).unwrap());
        let r = evaluate(&LocalFileSystem, &base, &cand, &Options::default()).unwrap();
        let expected = root.join("expected.json");
        let json = serde_json::to_value(&r).unwrap();
        std::fs::write(&expected, json.to_string()).unwrap();
        assert!(run(&args(&format!("{dirs} --expected {}", expected.display()))).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A reference map in one file against a run's tables: the tables are
    /// read as one map, whichever side the file is on.
    #[test]
    fn reference_file_against_a_directory_of_tables() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-ref-{}", std::process::id()));
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        let feature = |code: &str, y: f64| {
            format!(
                r#"{{"type":"Feature","properties":{{"isom_code":"{code}"}},
                "geometry":{{"type":"LineString","coordinates":[[0,{y}],[10,{y}]]}}}}"#
            )
        };
        let fc = |features: &[String]| {
            format!(
                r#"{{"type":"FeatureCollection","features":[{}]}}"#,
                features.join(",")
            )
        };
        let reference = root.join("reference.geojson");
        std::fs::write(
            &reference,
            fc(&[
                feature("101.000", 0.0),
                feature("201.000", 5.0),
                feature("777", 0.0),
            ]),
        )
        .unwrap();
        std::fs::write(run.join("contours.geojson"), fc(&[feature("101.000", 0.0)])).unwrap();
        std::fs::write(run.join("cliffs.geojson"), fc(&[feature("201.000", 5.5)])).unwrap();
        // not a table of the combined layout: not read
        std::fs::write(
            run.join("tile_cliffs.geojson"),
            fc(&[feature("201.000", 9.0)]),
        )
        .unwrap();

        for (b, c) in [(&reference, &run), (&run, &reference)] {
            let r = evaluate(&LocalFileSystem, b, c, &Options::default()).unwrap();
            let Outcome::Ok(v) = &r.vectors["reference.geojson"] else {
                panic!("reference.geojson failed");
            };
            let contours = &v.codes[&IsomCode::C101_000];
            assert!(!contours.has_change());
            let cliffs = v.codes[&IsomCode::C201_000].lines.unwrap();
            assert_eq!(
                (cliffs.precision, cliffs.recall, cliffs.hausdorff_m),
                (1.0, 1.0, 0.5)
            );
            assert_eq!(v.codes.len(), 2);
            assert_eq!(v.unknown_codes.len(), 1);
            assert!(r.has_change());
        }

        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let r = evaluate(&LocalFileSystem, &reference, &empty, &Options::default()).unwrap();
        assert!(matches!(
            r.vectors["reference.geojson"],
            Outcome::Err { .. }
        ));
        let png = root.join("map.png");
        assert!(evaluate(&LocalFileSystem, &png, &run, &Options::default()).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_pair_is_reported_not_fatal() {
        let root = std::env::temp_dir().join(format!("pullauta-eval-bad-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a.png"), root.join("b.png"));
        std::fs::write(&a, b"junk").unwrap();
        std::fs::write(&b, b"junk").unwrap();
        let r = evaluate(&LocalFileSystem, &a, &b, &Options::default()).unwrap();
        assert!(matches!(r.rasters["b.png"], Outcome::Err { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
