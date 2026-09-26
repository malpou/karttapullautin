---
status: accepted (fork)
date: 2026-09-26
---

# A vendored isom-maplibre symbol table

## Context

ADR-0003 makes ISOM 2017-2 codes the only symbol codes, but the codes, names and geometry types are scattered as literals across the renderer, the DXF writer, the GeoJSON schema and `osm.txt`. The fork's vector output is drawn by the MapLibre style [MetsaApp/isom-maplibre](https://github.com/MetsaApp/isom-maplibre) (MIT). Its `isom.yaml`, validated by `isom.schema.json`, lists every symbol the style draws: the "NNN.NNN" code, the table the feature is read from, and the drawing in ISOM dimensions, cross-checked against OpenOrienteering Mapper's ISOM 2017-2 symbol set. A code it does not list stays invisible.

## Decision

The fork's symbol table is a vendored copy of isom-maplibre's `isom.yaml` and `isom.schema.json`, pinned to one commit whose SHA is recorded beside the copy.

- `scripts/sync-isom-table.sh` refreshes the copy from a given commit and records the new SHA; the copy is never edited by hand.
- `build.rs` generates a Rust `IsomCode` enum and table from the copy at build time, the way the GeoJSON types are generated from their schema: pure Rust, offline. It feeds the GeoJSON `isom_code` values, the table each feature is written to, the `osm.txt` mapping and the DXF layer names.
- OpenOrienteering Mapper's `.omap` and `ISOM2000-ISOM 2017-2.crt` are a migration reference only, for what `isom.yaml` does not carry (symbol names, ISOM 2000 predecessors), such as the one-time `osm.txt` migration. They are not converted into the table.

Amended 2026-09-26: the first version generated the table from a vendored OOM `.omap` with a converter script. Matching the style that draws the output replaced that.

## Consequences

- One source for every symbol code; a code the table does not list cannot be emitted, and a conformance test holds every emitted code to it.
- Style changes arrive by re-running the sync script at a new commit; the diff of the vendored copy shows what moved.
- A feature KP produces that the style does not draw gets no code until the symbol is added to isom-maplibre.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
