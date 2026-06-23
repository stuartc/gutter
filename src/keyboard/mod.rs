//! Keyboard re-encoding — the real two-state kitty negotiation (slice 04).
//!
//! crossterm 0.29 yields a decoded `KeyEvent` only (no raw-byte accessor), so
//! gutter ALWAYS re-encodes; "verbatim" is dead (ADR-002). The slice-01
//! throwaway encoder is gone — this is the single keyboard encoding path.
//!
//! Two independent kitty states (ADR-003):
//!
//! - **Outer terminal capability** (fixed for the session): probed once at
//!   startup via `supports_keyboard_enhancement()`, surfaced as the
//!   `outer_supports` bool fed to [`KittyState`]. It is the inbound `CSI ? u`
//!   grant `vt100` cannot observe.
//! - **Child's negotiated level** (dynamic): tracked by [`KittyState`] as a
//!   push/pop stack, driven from the `Callbacks::unhandled_csi` watcher
//!   ([`is_kitty_csi`] recognises the family, [`KittyState::apply_csi`] mutates
//!   the stack), clamped to the outer capability.
//!
//! [`encode_key`] turns each `KeyEvent` into bytes at the child's
//! [`KittyState::current`] level — legacy bytes when the stack is empty, kitty
//! `CSI ... u` form otherwise.

mod encode;
mod kitty_state;

pub use encode::{encode_key, KittyLevel};
pub use kitty_state::{is_kitty_csi, KittyState};
