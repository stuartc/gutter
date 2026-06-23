//! Mouse forwarding (SGR 1006 only). Outer capture is enabled eagerly at
//! startup (removes the dropped-first-click race). Each render cycle polls
//! `mouse_protocol_mode()` / `mouse_protocol_encoding()` as the forwarding
//! gate: forward only when `mode != None && encoding == Sgr`; swallow when
//! `None`; bail loud on a non-Sgr encoding. Down-filter to the child's
//! granularity (PressRelease / ButtonMotion / AnyMotion, tracking button-held
//! state) and translate coords (`col - left_margin`, discard outside `[0, W)`),
//! re-encoding SGR as `CSI < b ; col+1 ; row+1 M/m`. See ADR-005.
