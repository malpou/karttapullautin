---
status: accepted (fork)
date: 2026-09-26
---

# Stages pass typed values in memory

## Context

Each tile stage writes its results to named files in `temp/` and the next stage reads them back: 38 file names, carried over from string literals in the Perl script. The protocol looks deliberate but is accidental; `depressions.txt` and `knollheads.txt` are written and never read. The CI regression job diffs the whole `temp/` directory, which makes those files look like a contract.

## Decision

Pipeline stages pass typed values in memory. Intermediates are written only when the `debug_intermediates` ini key is on, and they carry no format guarantee: they may change or disappear in any release. The CI `temp/` diff is replaced by pixel regression on the rendered map plus regression on the GeoJSON output.

## Consequences

- Stages become testable in isolation and can overlap, and stale temp files can no longer leak between runs.
- External scripts that read `temp/` break; the release note points them at the published outputs.
- Regression covers what users receive (the map and its vector features), not the route taken to it.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
