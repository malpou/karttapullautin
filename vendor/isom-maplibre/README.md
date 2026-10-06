# Vendored isom-maplibre symbol table

`isom.yaml` and `isom.schema.json` are copied unchanged from
[MetsaApp/isom-maplibre](https://github.com/MetsaApp/isom-maplibre) (MIT, see `LICENSE`) at commit
[`b35eabf94c99f4652b4ae336d6a5cee8dcbed030`](https://github.com/MetsaApp/isom-maplibre/tree/b35eabf94c99f4652b4ae336d6a5cee8dcbed030) (release [`v0.1.1`](https://github.com/MetsaApp/isom-maplibre/releases/tag/v0.1.1)).

`build.rs` generates `IsomCode` and `IsomTable` (`src/isom.rs`) from `isom.yaml`
at build time, offline; a test validates `isom.yaml` against `isom.schema.json`.
Never edit these files by hand: refresh them with

    scripts/sync-isom-table.sh <commit SHA> [release tag]

which re-fetches all three files at that commit and rewrites this README.
