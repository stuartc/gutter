# Capturing a replay recording

A replay recording is one real session: the child's output plus every resize made by hand. It is captured once and checked in; the child's output is not reproducible, so a recapture replaces the snapshots that go with it.

Run from an empty scratch directory so nothing private reaches the screen, and read the recording before checking it in.

## Claude Code, inline on the primary screen

```bash
GUTTER_RECORD=claude-code-resize.rec gutter --width 80pct claude
```

Start with the terminal about 140 columns by 40 rows. Paste the prompt below, then:

1. Wait for the reply to finish. It runs to more than two screens, so it scrolls.
2. Drag the window narrower, to roughly half its width, in one slow drag. Wait two seconds.
3. Drag it wider than it started. Wait two seconds.
4. Drag it shorter by about ten rows, then back. Wait two seconds.
5. Send `Now say only: done` and wait for the reply.
6. `/exit`.

The two-second waits matter: the replay takes its snapshots where the output went quiet.

### Prompt

```text
Do not use any tools. Reply with exactly the following sections, in this order, and nothing else.

1. A markdown table with four columns (Language, Greeting, Script, Notes) and six rows: English, Japanese, Chinese, Korean, Arabic, Hindi. Put the greeting in its native script.
2. A nested list three levels deep, with at least two items at each level, mixing numbered and bulleted levels.
3. One line of emoji: a plain face, a thumbs-up with a skin tone, a family joined with zero-width joiners, two flags, and a keycap digit.
4. One paragraph of at least sixty words with no line breaks, containing bold, italic, strikethrough and inline code.
5. A Rust code block of about fifteen lines.
6. A blockquote two lines long.
7. Three markdown links: https://example.com/a as "first link", https://example.com/b as "second link", and one whose text is the long URL https://example.com/a/very/long/path/that/keeps/going/and/going/until/it/has/to/wrap/across/the/band/edge itself.
8. The numbers 1 to 60, one per line.
```

## nvim, on the alt screen

```bash
GUTTER_RECORD=nvim-resize.rec gutter --width 80pct nvim -u NONE src/render.rs
```

1. Page down three times (`Ctrl-F`).
2. `:vsplit`, then `:set number`.
3. Drag the window narrower, wait two seconds, drag it wider, wait two seconds.
4. `:qa`.

This one covers a resize while the child is on the alt screen, and what the primary screen looks like when the child comes back to it.
