//! Band width `W` resolution. `--width N` is absolute (constant for the
//! session); `--width Npct` / `--width N%` is proportional — recomputed from
//! the current `real_cols` on every resize, floored at `MIN_W` (20) and capped
//! at `real_cols`. See ADR-011.

// const MIN_W: u16 = 20;
//
// enum Width { Absolute(u16), Proportional(u16) }
//
// fn resolve_width(spec: Width, real_cols: u16) -> u16 { unimplemented!() }
