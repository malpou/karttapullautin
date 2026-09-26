---
status: accepted (fork)
date: 2026-09-26
---

# No system libraries, not even behind a feature

## Context

Karttapullautin ships one self-contained binary for each of six release targets (Linux, macOS and Windows on amd64 and arm64). The closest Rust peer, Cassini, links PDAL and GDAL and asks users to set up a conda environment. Pure-Rust crates exist for the formats on the roadmap (GeoTIFF, FlatGeobuf, COPC), but they are younger than their C counterparts, so linking GDAL for one exotic format behind an optional cargo feature is tempting.

## Decision

No dependency may link a system library, in any build, on any target. An optional cargo feature is no exception: every feature combination builds with nothing installed but the Rust toolchain. When a format has no pure-Rust implementation, the fork writes one or waits.

## Consequences

- Install instructions stay "download and run" on all six targets.
- New formats cost more to add: a young pure-Rust crate, or code of our own, instead of a GDAL driver.
- Crates that compile C sources they vendor themselves and link statically, with no system library involved, pass this rule; today that is the allocator (`mimalloc`) and the zstd codec behind `zip`. Replacing them with pure Rust is welcome but not required.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
