# Vendored isom-maplibre symbol table

`isom.yaml` and `isom.schema.json` are copied unchanged from
[MetsaApp/isom-maplibre](https://github.com/MetsaApp/isom-maplibre) (MIT, see `LICENSE`) at commit
[`e2e782d380d48d376601527e16ce1a98e3c76b0e`](https://github.com/MetsaApp/isom-maplibre/tree/e2e782d380d48d376601527e16ce1a98e3c76b0e).

`build.rs` generates `IsomCode` and `IsomTable` (`src/isom.rs`) from `isom.yaml`
at build time, offline; a test validates `isom.yaml` against `isom.schema.json`.
Never edit these files by hand: refresh them with

    scripts/sync-isom-table.sh <commit SHA>

which re-fetches all three files at that commit and rewrites this README.
