# gutter

Run a terminal program inside a narrower column, while the program still
believes it owns the whole terminal.

![Neovim running inside a centred gutter band, with empty margins either side of the editor](docs/screenshot.png)

`gutter` starts a command in its own pseudo-terminal, renders the output into a
fixed-width band — centred or left-aligned — and passes your keyboard, mouse,
clipboard and window resizes straight through. The program wraps its lines,
positions its cursor and draws full-screen UIs as if the band were the entire
screen, because as far as it knows, it is.

It was built to wrap [Claude Code](https://claude.ai/code) at a comfortable
reading width on a wide monitor, but nothing about it is specific to that. It
works just as well with `vim`, `bash`, `htop`, a REPL, or anything else that
draws to a terminal.

## Why

Monitors are wide; a lot of terminal software reads better in a narrow column —
prose, a code review, a REPL, an AI coding agent that emits long lines. The
usual workarounds are clumsy: shrink the whole window, split a tmux pane and
leave half of it empty, or pipe through something that prefixes each line with
spaces (which falls apart the moment a program tries to move its own cursor).

gutter does the honest thing instead: it hands the program a real terminal that
genuinely is the width you asked for, then paints that terminal wherever you
want it to sit.

## How it works

Think of it as a single-pane tmux whose only job is to reflow your program into
a column you choose:

- The child's pseudo-terminal is sized to the band width `W`, not the real
  terminal width, so the child wraps and lays out at `W` columns naturally.
- gutter runs a terminal emulator (`vt100`) over the child's byte stream,
  keeping a `W`-column virtual screen in sync with what the child is drawing.
- Each frame it repaints that screen on your real terminal, shifted right by a
  margin so the column lands where you asked — centred or hard left.
- Input flows the other way: keystrokes, mouse events (coordinates adjusted for
  the margin), clipboard requests and resizes are translated and forwarded to
  the child.

The embedded emulator is the whole point. Because the child is given a real,
correctly-sized terminal rather than a padded illusion, everything that depends
on knowing the screen geometry — line wrapping, alternate-screen apps, cursor
positioning, scrollback — simply works.

## Install

### Prebuilt binary

Every [release](https://github.com/stuartc/gutter/releases/latest) publishes a
Linux x86_64 binary and a universal macOS one (Apple Silicon + Intel), each with
a SHA-256 checksum beside it. To fetch the latest and put it on your `$PATH`:

```sh
# macOS (universal: Apple Silicon + Intel)
curl -fsSL https://github.com/stuartc/gutter/releases/latest/download/gutter-universal-apple-darwin.tar.gz \
  | tar xzf - gutter
xattr -c gutter
sudo mv gutter /usr/local/bin/
```

```sh
# Linux x86_64
curl -fsSL https://github.com/stuartc/gutter/releases/latest/download/gutter-x86_64-unknown-linux-gnu.tar.gz \
  | tar xzf - gutter
sudo mv gutter /usr/local/bin/
```

The macOS binary is signed ad-hoc rather than with a Developer ID, so Gatekeeper
gets mad if it arrives carrying a quarantine flag. So you need to clear the flag
manually.

Or more manually if you want to check it first:

```sh
base=https://github.com/stuartc/gutter/releases/latest/download
target=universal-apple-darwin              # or x86_64-unknown-linux-gnu
curl -fsSL -O $base/gutter-$target.tar.gz -O $base/gutter-$target.sha256
shasum -a 256 -c gutter-$target.sha256     # sha256sum -c on Linux
tar xzf gutter-$target.tar.gz gutter
xattr -c gutter                            # macOS only
sudo mv gutter /usr/local/bin/
```

### From source

You'll need Rust 1.97.1. It's pinned in `.tool-versions`, so
[asdf](https://asdf-vm.com/) / [mise](https://mise.jdx.dev/) will pick the right
version up automatically. With a toolchain in place:

```sh
cargo install --git https://github.com/stuartc/gutter --locked
```

That drops a `gutter` binary on your `$PATH` (`~/.cargo/bin` by default).
Alternatively, clone the repo and `cargo build --release` to run
`target/release/gutter` directly.

## Usage

```
gutter [--width <N|Npct|full>] [--center|--left] [--resize-key <chord>] <cmd> [args...]
gutter --version
```

```sh
gutter --width 80 --center claude     # 80-column band, centred
gutter --width 100 vim notes.md       # absolute width, centred
gutter --width 50pct --left htop      # half the terminal, hugged to the left
gutter bash                           # no --width: 100-column band, centred (the default)
gutter --width full bash              # full width, pure passthrough
gutter --version                      # print the build stamp and exit
```

Flags:

- `--width N` — absolute band width in columns, fixed for the session.
- `--width Npct` (or `N%`, or the `--width=N` form) — proportional width,
  recomputed every time you resize the terminal.
- `--width full` — an alias for `--width 100%`: the band always matches the real
  terminal width, i.e. a transparent passthrough.
- `--center` / `--centre` — centre the band. This is the default.
- `--left` — pin the band to the left edge.
- `--resize-key <chord>` — the key that toggles resize mode, `ctrl-\` by
  default. Write it as `[<mod>-]...<key>`: modifiers are `ctrl`/`c`,
  `alt`/`meta`/`m` and `shift`/`s`, and the key is a single character or one of
  `esc`, `tab`, `enter`, `space`, `f1`–`f12`.
- `--version` — print the build stamp on stdout and exit, without starting
  anything.

A few things worth knowing:

- Flags are only read _before_ the command. Anything after the command name
  belongs to the child, so `gutter vim --width 100` passes `--width 100` to vim,
  not to gutter.
- There is no `--help`. Apart from `--version`, an unrecognised leading token is
  taken to be the command you want to run.
- The version stamp is `git describe --tags --dirty --always` from the build, or
  the crate version where there was no checkout to describe. Treat the `-dirty`
  suffix as a hint in both directions: cargo only reruns the build script when
  `HEAD` or the branch ref moves, so a stamp can keep saying `-dirty` after the
  tree was cleaned, and can leave it off a tree that is dirty now.
- With no `--width`, gutter defaults to a 100-column band, centred (clamped to
  the real width on narrower terminals). Use `--width full` (or `--width 100%`)
  for a transparent passthrough at the real terminal width.

## Changing the width while it's running

Press `ctrl-\` (or whatever you set `--resize-key` to) and gutter enters resize
mode: faint rails appear in the margins with the current width printed beside
them. From there:

- `h` / `l`, `-` / `+`, or the left and right arrows step the band by one.
- `H` / `L` step by ten.
- The chord again, or Escape, leaves. So does three seconds of not touching
  anything.

The step follows whatever unit you asked for on the command line — columns for
`--width 80`, percentage points for `--width 50pct` — and it won't take the band
below 20 columns or past the width of the real terminal. The width you land on
lasts for the session; nothing is written to disk.

While resize mode is up, gutter keeps every keystroke for itself. The child sees
nothing at all until you leave.

## What passes through

- **Keyboard**, byte for byte: whatever your terminal sends is what the child
  receives. Only the resize chord is held back — and, while resize mode is up,
  everything. The child's protocol requests go back out to your terminal, so the
  two negotiate directly and whatever your terminal supports is what the child
  gets: function keys, Home and End, PageUp and PageDown, Insert, Delete,
  Shift+Tab, Alt+key and modified arrows all arrive as the child expects them.
- **Mouse**, with coordinates translated into the band.
- **Clipboard**, via OSC 52.
- **Resizes** — the child is told its new size, and proportional widths
  re-resolve on the spot.
- **Ctrl-Z and `fg`** — suspending gutter puts the terminal back the way it was
  and stops the child along with it; `fg` restores raw mode, the band and the
  keyboard modes the child had negotiated, then continues the child.

## The terminal gutter talks to

gutter draws on your terminal, not on stdout — the same terminal it reads the
keyboard from. Like `tmux`, it needs one to run, and once it is running a child
it ignores where descriptor 1 points (`--version` is the exception: it prints on
stdout and exits before any of this happens):

- `gutter claude > session.log` paints on the screen as normal and leaves
  `session.log` empty. The child's output goes to its own pseudo-terminal and
  reaches you as band paint; it is not teed into the file. There is no capture
  mode — if you want a transcript, ask the program for one.
- Started without a controlling terminal but with one on its standard
  descriptors — `setsid gutter claude`, or a launcher that hands over a
  pseudo-terminal without making it the controlling one — gutter names that
  terminal, reopens it, and paints there. Window resizes never reach a process
  in that position, so the band and the child stay at the size they started at.
- With no terminal on any of those routes — cron, CI, a `systemd` unit — gutter
  prints `gutter: no controlling terminal: …` and exits 1 without starting the
  child. A pipe or a redirect on stdout is not that case: your terminal is still
  there, so gutter runs and paints on it, and the pipe gets nothing.

## Recording a session

Set `GUTTER_RECORD` to a file name and gutter writes down what it took in while
it ran: the terminal size it started at, every chunk of output from the child,
and every resize, each with a timestamp.

```sh
GUTTER_RECORD=session.rec gutter claude
```

The file is plain text, one event per line, and is overwritten if it exists.
gutter's own tests replay these recordings to check what a terminal ends up
showing, so one is the most useful thing to attach to a rendering bug report.

**A recording holds everything the child put on screen.** That includes what
you typed wherever the child echoed it, file contents, tokens a tool printed,
and anything the child copied to the clipboard. Read it before you share it.
What you typed is not recorded directly, and neither is anything that was never
drawn.

Two limits: suspending with Ctrl-Z is not recorded, so a session that was
suspended will not replay as it ran; and the file is written as gutter goes, so
a path on a slow or stalled filesystem slows the painting down with it.

## Working on it

```sh
cargo build
cargo test                       # unit + PTY integration tests
cargo clippy --all-targets
```

The integration tests drive a real PTY and assert on what the outer terminal
actually displays, not on gutter's internals, so they need a valid `TERM` (CI
uses `xterm-256color`).

There's an optional equivalence gate behind the `oracle` feature. It replays
recorded VT byte streams through both gutter's emulator and a second,
independent one (wezterm's) and diffs the resulting cells, so rendering
regressions get caught early. It pulls in a heavier dependency tree, so it's off
by default:

```sh
cargo test --features oracle
cargo clippy --all-targets --features oracle
```

CI runs all five gates: `build`, `test`, `test --features oracle`, and both
clippy passes.

## Licence

gutter is released under the [GNU General Public License v3.0](./LICENSE).
