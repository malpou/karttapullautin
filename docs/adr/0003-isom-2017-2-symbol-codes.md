---
status: accepted (fork)
date: 2026-09-26
---

# ISOM 2017-2 numbers are the only symbol codes

## Context

The codebase mixes standards. `osm.txt` maps OpenStreetMap tags to ISOM 2000 numbers (building 526, settlement 527, parking 529), DXF layers are named with words (`contour`, `formline`, `cliff2`) apart from the numeric `1010`, and the vector export writes ISOM 2017-2 numbers. PNG output hid the inconsistency; vector output exposes it to every downstream tool.

## Decision

ISOM 2017-2 numbers are the only symbol codes: in the code, in DXF layer names and in GeoJSON.

- Plain numbers: `101`, not `101.000`.
- Distinctions a unified code loses are kept as boolean flags: `depression: true` on a depression contour (same 101/102/103 as any contour) and `ugly: true` on a dot knoll or small depression the detector is unsure of.
- `osm.txt` migrates once, using OpenOrienteering Mapper's `ISOM2000-ISOM 2017-2.crt` cross-reference table.
- Hard cut with a release note: no aliases, no deprecation release, no old layer names emitted alongside.

## Consequences

- CRT files, OCAD import templates and scripts that match on the old DXF layer names break once and are updated from the release note.
- A DXF layer name, a GeoJSON symbol code and a map legend name the same symbol with the same number.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
