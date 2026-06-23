//! The single `vt100::Callbacks` impl, owned by the render thread and attached
//! via `Parser::new_with_callbacks(rows, cols, scrollback, cb)` (there is NO
//! `process_cb`). It carries two responsibilities:
//!
//! - `unhandled_csi` — watches the kitty keyboard family (`c == 'u'`,
//!   `i1 ∈ {>, =, <, ?}`) as a push/pop stack tracking the child's negotiated
//!   level (clamped to the outer terminal's capability). See ADR-002/003.
//! - `copy_to_clipboard` — OSC 52 write interception; hands off to `clipboard`.
//!   vte (under vt100) handles OSC termination/abort/splitting. See ADR-004.
