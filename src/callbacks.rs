//! The `vt100::Callbacks` impl carried by the render thread's parser.
//!
//! Wired in from this slice (ADR-002/004) so later slices add **behaviour**, not
//! plumbing. In slice 02 every callback is inert:
//!
//! - `copy_to_clipboard` (OSC 52 write → `/dev/tty`) lands in slice 06,
//! - `unhandled_csi` (the kitty keyboard-level watcher) lands in slice 04.
//!
//! Construct the parser with
//! `vt100::Parser::new_with_callbacks(rows, cols, scrollback, GutterCallbacks)`
//! — there is **no** `process_cb` method (that reference in the rough plan is
//! wrong). The callbacks fire on `parser.process(bytes)` on the render thread,
//! which is the only thread that touches the parser, so there is no shared
//! mutable state here.

/// The single callbacks struct the parser owns. Inert in slice 02; slices 04/06
/// add fields and override the relevant trait methods.
#[derive(Debug, Default)]
pub struct GutterCallbacks;

impl vt100::Callbacks for GutterCallbacks {
    // All methods keep the trait's default (no-op) bodies in this slice. The
    // overrides for `copy_to_clipboard` (slice 06) and `unhandled_csi`
    // (slice 04) drop in here without re-plumbing the parser.
}
