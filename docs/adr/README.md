# Architecture decision records

The decisions behind gutter's design. Code comments reference these by number
(`// See ADR-007`) rather than re-explaining the rationale inline.

| # | Decision |
|---|----------|
| [001](0001-two-emulator-equivalence-gate.md) | Two-emulator equivalence gate |
| [002](0002-keyboard-always-re-encoded.md) | Keyboard always re-encoded |
| [003](0003-two-independent-kitty-states.md) | Two independent kitty states |
| [004](0004-osc52-clipboard-separate-tty.md) | OSC-52 clipboard to a separate `/dev/tty` |
| [005](0005-mouse-eager-capture-poll-gate.md) | Mouse eager capture and poll-diff gate |
| [006](0006-band-fit-margin-rule.md) | Band-fit margin rule |
| [007](0007-coalescing-loop.md) | 60fps coalescing loop |
| [008](0008-resize-ordering.md) | Resize ordering |
| [009](0009-merged-unbounded-channel.md) | Merged unbounded channel |
| [010](0010-ordered-teardown.md) | Explicit ordered teardown |
| [011](0011-width-resolution.md) | Width resolution |
| [012](0012-screen-mode-mirroring.md) | Screen-mode mirroring |
| [013](0013-inline-anchor-scroll-paint.md) | Inline anchor, scroll-aware paint, and hand-back |
| [014](0014-row-run-self-containment.md) | Row-run self-containment at offset |
| [015](0015-modal-resize.md) | Modal resize |
| [016](0016-uniform-margin-management.md) | Uniform margin management |

## Format

Each record is short: Context (the forces), Decision (what we chose, in plain
language), Consequences (what the rest of the code must do to honour it), and Code
anchors (where it lives). Status stays `Accepted` unless a decision is superseded,
in which case the new record links back and this one is marked `Superseded by ADR-NNN`.
