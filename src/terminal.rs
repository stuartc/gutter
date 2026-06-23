//! Outer-terminal lifecycle (Thread 2 only): raw mode, alt screen, eager
//! `EnableMouseCapture`, kitty `PushKeyboardEnhancementFlags` when
//! `supports_keyboard_enhancement()` grants it, and the explicit restore
//! (leave alt screen → pop kitty flags → disable mouse → show cursor →
//! disable raw mode) run BEFORE `process::exit` — `process::exit` does not run
//! destructors, so teardown cannot be a `Drop` guard. See ADR-010.
