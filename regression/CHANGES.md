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

### 2026-09-27: pr/ring-quirks

The knoll detector's elevation pass and the knoll lift use `Ring::contains`
instead of ray casts that skipped the closing and the first edge,
`join_polylines` lets slot 0 be a join partner, and the knoll-lift
smoothing skips every cell the lift already raised (ticket 18 items 1-4).
The last is the intended rule: the old guard skipped a lifted cell only at
whole-number coordinates, an artefact of the string-keyed lookup it
replaced. Baseline moves to ddd4d06.

- Pixels: single job 41 020 of 50 310 649 (0.0815 %), batch job 39 373 of
  50 239 744 (0.0784 %), on every rendered PNG. The ring and join fixes
  alone account for 19 606 / 18 762 px, the smoothing fix for the rest.
- `detected.*`, `pins.bin` and `contours03.*` are identical: the change
  enters at `xyz_knolls.hmap`. `knollheads.txt` goes from 1 993 to 1 941
  lines.
- Per code (batch job, `merged_*.geojson`):
  101 features 844 -> 812, length 160 153.7 -> 159 340.0 m, precision
  0.9994, recall 0.9958;
  102 features 169 -> 167, length 34 228.1 -> 34 120.6 m, precision
  0.9997, recall 0.9982;
  103 features 572 -> 570, length 141 199.1 -> 140 997.9 m, precision
  0.9998, recall 0.9986;
  109 points 625 -> 660, precision 0.900, recall 0.950;
  111 points 400, 2 with changed properties (299 -> 300 points in the
  combined export's `knolls_points.geojson`).
  Every other code is identical.
