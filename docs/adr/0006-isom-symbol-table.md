---
status: accepted (fork)
date: 2026-09-26
---

# A generated ISOM 2017-2 symbol table

## Context

ADR-0003 makes ISOM 2017-2 numbers the only symbol codes, but the numbers, names and geometry types are scattered as literals across the renderer, the DXF writer, the GeoJSON schema and `osm.txt`. OpenOrienteering Mapper maintains the ISOM 2017-2 symbol set as machine-readable XML (`.omap`, GPL-3, the same licence as Karttapullautin); transcribing it by hand invites errors.

## Decision

The fork carries a machine-readable ISOM 2017-2 symbol table: for each symbol its code, name, geometry type (point, line or area), colour, ISOM 2000 predecessor and key dimensions. A converter script generates it from the vendored OpenOrienteering Mapper `.omap` into YAML with a JSON schema. A Rust `Symbol` type is generated from the table, the way the GeoJSON types are generated from their schema, and feeds the GeoJSON symbol code enum, the `osm.txt` mapping and the DXF layer names.

## Consequences

- One source for every symbol code; a symbol the table does not know cannot be emitted.
- Upstream symbol set fixes arrive by re-running the converter, not by editing literals.
- The `.omap` is vendored into the repository with its provenance and licence.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
