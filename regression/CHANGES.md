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
