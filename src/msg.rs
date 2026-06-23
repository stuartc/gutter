//! The merged message enum carried by the single unbounded channel into the
//! render thread. The PTY path is throttled upstream (bounded `sync_channel`),
//! so the merged channel is unbounded and an input `send()` never blocks —
//! a keystroke stays admissible under a multi-MB PTY flood.

// enum Msg {
//     Pty(Vec<u8>),
//     Input(crossterm::event::Event),
//     ChildExited(portable_pty::ExitStatus),
// }
