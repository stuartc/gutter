//! Command-line parsing: `gutter --width <N|Npct> [--center|--left] <cmd> [args...]`.
//! Produces the resolved config (the raw `--width` form, alignment, child
//! command + args). Width interpretation itself lives in `width`.
