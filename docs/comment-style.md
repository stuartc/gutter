# Comment style

How we comment in gutter. The goal is comments that read as if a senior engineer
wrote them for a peer who already knows the codebase — plain, specific, and only
where they earn their place. `input.rs`, `waiter.rs` and `msg.rs` are the house
style; everything should converge on them.

The one rule underneath all the others: **a comment should say something the
names, types, and structure can't.** If the code already makes it clear, the
comment is clutter — delete it and, if needed, improve the code instead.

## Write a comment when it carries

- **A why** — the reason behind a non-obvious choice or an unidiomatic line.
- **A constraint** — a limit, an ordering requirement, an external quirk we work
  around.
- **An invariant** — something later code relies on staying true. Group a type's
  invariants in one block near its definition rather than scattering them.
- **Negative information** — "we do NOT do X here, because Y." Code can't say this.
- **A surprising trade-off** — a performance or correctness decision a reader
  would otherwise undo.
- **A `// SAFETY:` note** on every `unsafe` block: name the invariant and say how
  the surrounding code satisfies it.

## Cut or rewrite when it is

- **What-narration** — restates the function name, a parameter, or the next line.
- **History** — "slice 02 wired this in", "this slice adds…". That belongs in the
  commit message, not the code. The code should read as if it always existed.
- **The LLM register** — "it's worth noting", "this ensures that", "importantly",
  or three qualifications stacked into one sentence. Cut the filler; keep the
  content if there is any.
- **Compound coinages** — "count-based delta", "per-frame detection device". Write
  the plain thing: "the count vt100 can't reconstruct once a burst scrolls past a
  screenful".
- **Commented-out code** — that's what git is for.

## Shape

- Length tracks how non-obvious the thing is, not how important it is. Important
  but self-evident code gets no comment; a subtle edge case gets a paragraph.
- The first line of a `///` doc is one present-tense sentence: "Returns…",
  "Holds…", "Spawns…". Ends with a full stop.
- `//!` module docs stay broad — what the module is for and how it fits. Detail
  lives on the items, not in the preamble.
- Inline `//` inside a body is 1–3 lines. Longer rationale moves up to the item's
  doc comment, into an ADR, or gets the line extracted into a well-named helper.
- Reference a design decision by its ADR: `// See ADR-013`. The rationale lives in
  `docs/adr/`, not duplicated inline.

## Tone

- Plain declarative sentences. Active voice. One idea per sentence — if it has a
  colon, a semicolon and two "which" clauses, split it.
- Specific over vague: "skips the fast path for inputs over 1 MB" beats "may skip
  certain optimisations for large inputs". Drop "various", "as appropriate".
- An honest "probably" or "I think" is fine — that's real uncertainty, and it
  reads as human. False precision reads as generated.
- If a sentence needs re-reading to parse, it cost more than it saved.

## Reviewer checklist

**Defect if absent**
- [ ] Every `unsafe` block has a `// SAFETY:` comment naming the invariant.
- [ ] First line of each `///` is one present-tense sentence ending in a full stop.
- [ ] `//!` appears only at a crate root or module file.

**Strong preference**
- [ ] Inline comments explain why, not what.
- [ ] The comment says something not inferable from names, types, and structure.
- [ ] Body comments are short; long rationale lives in the item doc or an ADR.
- [ ] Design decisions are cited by ADR, not re-explained inline.

**Smells — investigate**
- [ ] Comment restates a name, parameter, or return type.
- [ ] Hedge openers: "it's worth noting", "this ensures that", "importantly".
- [ ] A body comment longer than the code it sits beside.
- [ ] Change-history narration ("slice NN", "this slice…").
- [ ] Compound-coinage jargon where plain words would do.
- [ ] A comment that goes stale the moment its line is refactored.
