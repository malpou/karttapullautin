---
status: accepted (fork)
date: 2026-09-26
---

# Ground model cell size follows pulse density

## Context

The ground model cell size is fixed at `2.0 * scalefactor` metres whatever the input. That suited sparse early surveys; national datasets now reach 10 pulses/m², so a 2 m cell throws most of the resolution away, while very sparse data still gets streaky interpolation from the same cell.

## Decision

By default the cell size is derived from the tile's measured pulse density and clamped to a configured minimum and maximum. An ini override forces a fixed cell size; setting it to `2.0 * scalefactor` reproduces today's behaviour.

## Consequences

- Dense surveys get sharper relief; sparse surveys get larger cells instead of invented detail.
- Output changes for most inputs, and the switching branch measures it under ADR-0001.
- Parameters tuned for 2 m cells (contour resolution, vegetation block size, cliff bin width) need re-checking at other sizes.
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
