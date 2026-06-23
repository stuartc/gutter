//! Keyboard re-encoding. crossterm 0.29 yields a decoded `KeyEvent` only (no
//! raw-byte accessor), so gutter ALWAYS re-encodes — never verbatim. Each
//! `KeyEvent` becomes the byte form matching the child's negotiated kitty
//! level: legacy bytes with no kitty, kitty `CSI ... u` otherwise
//! (e.g. Shift+Enter → `CSI 13 ; 2 u`), clamped to the outer terminal's
//! capability. See ADR-002/003.
