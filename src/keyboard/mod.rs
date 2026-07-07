//! Keyboard re-encoding and kitty-level tracking.
//!
//! crossterm hands us a decoded `KeyEvent` with no raw bytes, so every key is
//! re-encoded before it reaches the child (ADR-002). [`encode_key`] picks the
//! form from the child's current level: legacy VT bytes when the stack is empty,
//! kitty `CSI ... u` otherwise.
//!
//! [`KittyState`] tracks the child's level as a push/pop stack, clamped to the
//! outer terminal's probed capability (ADR-003).

mod chord;
mod encode;
mod kitty_state;

pub use chord::{parse_chord, KeyChord};
pub use encode::{encode_key, KittyLevel};
pub use kitty_state::{is_kitty_csi, KittyState};
