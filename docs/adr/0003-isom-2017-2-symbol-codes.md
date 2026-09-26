---
status: accepted (fork)
date: 2026-09-26
---

# ISOM 2017-2 numbers are the only symbol codes

## Context

The codebase mixes standards. `osm.txt` maps OpenStreetMap tags to ISOM 2000 numbers (building 526, settlement 527, parking 529), DXF layers are named with words (`contour`, `formline`, `cliff2`) apart from the numeric `1010`, and the vector export writes ISOM 2017-2 numbers. PNG output hid the inconsistency; vector output exposes it to every downstream tool. The downstream tool the fork targets is the MapLibre style [MetsaApp/isom-maplibre](https://github.com/MetsaApp/isom-maplibre), which selects each feature's symbol by a code string.

## Decision

ISOM 2017-2 numbers are the only symbol codes: in the code, in DXF layer names and in GeoJSON.

- The format is the one isom-maplibre reads: an "NNN.NNN" string, `"101.000"`, in the GeoJSON property `isom_code`. A symbol variant takes the non-zero suffix the style defines: slope line `"101.001"`, large building `"521.001"`.
- DXF layer names use the same code string.
- Distinctions a unified code loses are kept as boolean flags: `depression: true` on a depression contour (same 101/102/103 as any contour) and `ugly: true` on a dot knoll or small depression the detector is unsure of.
- `osm.txt` migrates once to the codes of the symbol table (ADR-0006), using OpenOrienteering Mapper's `ISOM2000-ISOM 2017-2.crt` cross-reference table for its ISOM 2000 numbers.
- Hard cut with a release note: no aliases, no deprecation release, no old layer names emitted alongside.

Amended 2026-09-26: the first version chose plain numbers (`101`) in a `symbol` property; matching isom-maplibre replaced both.

## Consequences

- CRT files, OCAD import templates and scripts that match on the old DXF layer names break once and are updated from the release note.
- A DXF layer name, a GeoJSON `isom_code` and a map legend name the same symbol with the same code.
- GeoJSON output renders in isom-maplibre without a translation step.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
