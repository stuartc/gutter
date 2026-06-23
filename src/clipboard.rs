//! OSC 52 clipboard write. Driven by `callbacks::copy_to_clipboard`, which
//! hands `data` still base64-encoded. Reconstruct `ESC ] 52 ; ty ; data BEL`
//! and write it verbatim (no decode/re-encode round-trip) to a separately
//! opened `/dev/tty` — a distinct fd from stdout, no contention. Open
//! `/dev/tty` for read as well so the post-v1 OSC 52 read-response relay isn't
//! foreclosed. See ADR-004.
