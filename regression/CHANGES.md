# Regression baseline

CI (`.github/workflows/regression.yml`) builds the commit named by `ref` in
`baseline.toml` and this branch's HEAD, runs `regression/run.sh` with each
build (the single job and the batch job on the regression tile, each with its
own `pullauta.default.ini` and `osm.txt`), and compares the two with this
branch's `pullauta eval`:

    pullauta eval base/output head/output --format json --fail-on-change
    pullauta eval base/output head/output --format json --expected regression/expected.json

The second form is used when `regression/expected.json` exists. The job
passes when nothing differs (no changed pixel, per-code metric, byte or file),
or when the JSON report equals `expected.json`. It runs the baseline build
twice first and fails if those two runs differ: the gate is only as good as
the pipeline's determinism.

Run it locally the same way:

    regression/run.sh <base-src>/target/release/pullauta <base-src> /tmp/base
    regression/run.sh target/release/pullauta . /tmp/head
    target/release/pullauta eval /tmp/base/output /tmp/head/output --fail-on-change

## Changing output

A branch that changes output (see `CONTEXT.md`: any pixel, debug intermediate
or vector feature on the regression tile) lands in two commits:

1. The change, plus `regression/expected.json`: the report of the change,
   `pullauta eval base/output head/output --format json > regression/expected.json`
   (CI uploads it as `report.json`).
2. "Rebase regression baseline for <slug>": `ref` = the sha of commit 1 and a
   new `note`, `expected.json` deleted, and a dated entry below giving the
   reason, the share of changed pixels (single job and batch job) and the
   per-code deltas.

Commit 1 must pass the gate before commit 2 lands, because commit 2 moves the
baseline past it. A push runs CI only on the branch tip, so:

- run the gate locally on commit 1, with the baseline checkout `<base-src>` at
  the old `ref`, and require exit status 0:

      regression/run.sh <base-src>/target/release/pullauta <base-src> /tmp/base
      regression/run.sh target/release/pullauta . /tmp/head
      target/release/pullauta eval /tmp/base/output /tmp/head/output --format json --expected regression/expected.json

- then push commit 1 alone and wait for a green Regression run on it before
  committing and pushing commit 2.

A branch that renames or drops a key the jobs set writes the setting as
`new|old=value` in `run.sh` so the baseline build still gets it, and drops the
old name when it rebases the baseline.

Branches are never force-pushed, so every `ref` stays reachable and CI can
fetch it.

## Entries

### 2026-09-27: pr/regression-baseline

First baseline: `pr/eval-command-v2` at cd2e55d. No output change; the jobs
replace the release-vs-PR pixel comparison and the `temp/` diff.

### 2026-09-27: pr/worldfile-normalise

Every world file is written by `WorldFile::write`, and the form-line
transform uses one operator order for x and y (ticket 18 items 5-6).
Baseline moves to d6590ca.

- Pixels: 0 changed in the single job and the batch job.
- Per-code metrics: identical for every code in every GeoJSON; code 103
  length delta 0 m. The old and new form-line operator orders differ in
  the last ulp only when `scalefactor` is not a power of two, so form
  lines do not change at the jobs' `scalefactor=1`.
- World-file text, 8 files, origins unchanged: `vegetation.pgw`
  (`temp/`, `temp1/`, `temp_test_file_dir/`) writes `1`, `0`, `0`, `-1`
  for `1.0`, `0.0`, `0.0`, `-1.0` (38 -> 30 bytes); `undergrowth.pgw`
  (same three folders) writes `0.42333332155810494` and
  `-0.42333332155810494` (the f64 reciprocal of the f32 draw factor)
  for the f32-printed `0.42333332` and `-0.42333332`, and `0` for `0.0`
  (52 -> 66 bytes);
  `single/pullautus.pgw` and `pullautus_depr.pgw` write `0` for the
  copied `0.0` rotation lines (70 -> 66 bytes).
- Not produced on the regression tile: `{laz}_undergrowth.pgw` and
  `{laz}_vege.pgw` in `out/` (they need `savetempfiles=1`). They change
  the same way; `_undergrowth.pgw`'s origin takes half its pixel size,
  so it moves in the last digits.
