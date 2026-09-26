---
status: accepted (fork)
date: 2026-09-26
---

# Fidelity to the Perl program is history, not a requirement

## Context

The Rust port was checked against Perl Karttapullautin while it was being written. The default ini still says its values are "meant to be fidel to original Perl Karttapullautin", and the CI regression job fails on any pixel that differs from the latest release. Nobody wrote down whether that was a porting aid or a product promise, so every algorithm change has been blocked by default. The first real case: fixing two ray-casts that skip one ring edge each and the slot-0 sentinel in `join_polylines` moves 0.04 % of pixels on the regression tile (19 606 of 50.3 M single-tile, 18 762 of 50.2 M batch) and drops 43 knoll heads (1 993 to 1 950). No refactor keeps the old output, because the old output is the bug.

## Decision

Fidelity to the Perl program was a porting aid and is now history. Output (rendered pixels, temp files, vector output) may change when the change is justified. Every branch that changes output measures the change, states it in its PR description, and rebases the regression baseline with a note:

- pixel-diff % on the regression tile, single-tile and batch;
- the score from the `eval` command, once that command exists.

There is no `classic` profile that preserves today's output.

## Consequences

- The ini banner claiming fidelity goes when the first output-changing branch lands.
- A byte-identical refactor stays the norm; an output change is a reviewed decision with numbers, not a regression.
- Temp files carry no fidelity promise (see ADR-0004).
- Accepted for the fork; whether upstream adopts it is the maintainers' call.
