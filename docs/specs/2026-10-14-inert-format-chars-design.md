# Keep invisible format characters out of reported text shown to agents

**Status:** DRAFT
**Issue:** #47

## 1. Brief

From #47 (found by the security review of #46 (#38); the behaviour predates it):

> `flag_health` shows SDK-reported error text (`error_type`, `last_message`) to agents after passing it
> through `inert` (`crates/flags-core/src/telemetry.rs`), which replaces characters for which
> `char::is_control()` is true. That is Unicode category Cc only. Format characters (category Cf),
> including the bidirectional overrides and isolates (U+202A..U+202E, U+2066..U+2069) and zero-width
> characters (U+200B..U+200F, U+2060, U+FEFF), pass through unchanged.
>
> Anyone holding an app's public `sdk_` client key can report error text, so such characters can reach
> an agent's context, where they can visually reorder or hide parts of the quoted text for any human
> reading the same output. The text is already marked as untrusted data in the tool's schema docs, and
> the cost is low, but `inert`'s stated purpose is to make reported text hard to dress up as anything
> other than a quoted error.
>
> **Acceptance criteria**
> - [ ] `inert` also replaces Unicode format characters (at least the bidi controls and zero-width
>   characters listed above) with a space before flattening whitespace.
> - [ ] The existing unit test `reported_text_is_flattened_and_shortened` (or a sibling) covers a
>   string containing U+202E and U+200B.

## 2. Premise corrections

- **Hiding text from a reader is not only a format-character trick.** Variation selectors
  (U+FE00..U+FE0F, U+E0100..U+E01EF, category Mn) render as nothing on their own and can carry
  arbitrary bytes invisibly after any base character, the same way the Cf tag characters
  (U+E0020..U+E007F) can. A model reads them; a human does not see them. Stripping Cf alone leaves
  that channel open, so `inert` replaces both (§3).
- **Error text is not the only SDK-reported text `flag_health` returns.** `Health.variations` is
  keyed by variation names taken from SDK evaluation telemetry (`EvalEvent.variation`), stored
  through `stored(_, 64)` (length cap and NUL replacement only, in `record_evaluations`) and read back
  from `flag_eval_hourly` unchanged (`health`). They never pass through
  `inert`, so a public `sdk_` key can put control, format, and tag characters into the keys of that
  map today. The issue's premise ("`flag_health` shows SDK-reported error text … after passing it
  through `inert`") is right about error text and incomplete about the tool: the variation keys get
  the same treatment (§6). Nothing else an MCP tool returns is SDK-reported: `rollback_events` and
  `propose_rollback` carry numbers and agent-set environments, and an error's `environment` is not
  returned.

## 3. Assumptions

- **Every Cf character, not only the listed ones.** The issue says "at least" the bidi and
  zero-width characters. The whole category (Unicode 16.0: 21 ranges, among them the soft hyphen,
  the Arabic letter mark, the invisible math operators U+2061..U+2064, the deprecated format
  characters U+206A..U+206F, the interlinear annotation characters U+FFF9..U+FFFB, and the tag
  characters U+E0001, U+E0020..U+E007F) has no visible glyph of its own, and none of it is needed to
  read an error message. A fixed table is used, not a dependency: Rust's standard library exposes no
  general-category lookup, and a new crate for one 21-range table is not worth the review cost.
  U+2065 (unassigned, inside the bidi block) is included so U+2060..U+206F is one range.
- **Default-ignorable code points too** (§2; added after PR review). Unicode's
  Default_Ignorable_Code_Point property (Unicode 16.0, `DerivedCoreProperties.txt`) names the
  characters a renderer shows as nothing: it overlaps Cf and adds the variation selectors (U+FE00..
  U+FE0F, U+E0100..U+E01EF, the Mongolian U+180B..U+180D and U+180F), the combining grapheme joiner
  U+034F, the Hangul and Khmer fillers (U+115F, U+1160, U+3164, U+FFA0, U+17B4, U+17B5), U+FFF0..
  U+FFF8, and the whole tag block U+E0000..U+E0FFF (its unassigned code points too, which Rust
  strings accept). `hidden` is Cc ∪ Cf ∪ Default_Ignorable_Code_Point, with ranges merged where the
  sets meet. The cost: an emoji in an error message may lose its presentation selector, and ZWJ and
  ZWNJ are flattened, splitting an emoji sequence or a Persian or Indic word in two. The message is
  still legible, and it is quoted, not typeset.
- **Replace with a space, then flatten.** As the issue specifies: a format character becomes a
  space, so `split_whitespace` folds it with its neighbours. A zero-width space between two words
  therefore becomes one space, and `"ab\u{200B}cd"` becomes `"ab cd"`; that is a visible mark that
  something was there, which is the point.
- **Clean the view, not the store.** `inert` is applied where `flag_health` builds its answer
  (`top_errors` and now the variation keys, §2). What is stored is unchanged: the raw text stays in
  `flag_errors` and `flag_eval_hourly` as reported (minus NUL, #38). Cleaning at record time would
  merge buckets irreversibly and is not needed: no other path shows this text to an agent.
- **Variation keys that clean to the same string are merged.** Two reported variations differing
  only in hidden characters (`"a\u{202E}b"`, `"a\u{200B}b"`) both read `"a b"`; their counts are
  summed under that key, which is what the map already does for repeated keys. A variation that
  cleans to `""` is dropped, as an empty variation already is. The length cap stays 64 (the stored
  cap, named once as `MAX_VARIATION`), so `inert` never adds an ellipsis to a stored variation.
- **Nothing beyond those three sets.** Characters that merely look blank but are not
  default-ignorable (U+2800 BRAILLE PATTERN BLANK, spaces of unusual width) render as space or as a
  glyph; they can pad but not hide or reorder text. Recorded in §12.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

Error text reported through an SDK reaches an agent as plain, visibly ordered text with nothing
hidden in it.

- `inert` replaces every Cf character and every variation selector with a space before flattening.
- `flag_health`'s `variations` keys pass through `inert`; a reported variation containing U+202E
  shows as the cleaned string, its count merged with any variation that cleans to the same.
- `"boom\u{202E}gnp.exe"` comes out as `"boom gnp.exe"`; `"a\u{200B}b"` as `"a b"`.
- A unit test covers U+202E and U+200B (the AC), plus the tag characters and a variation selector.
- Nothing else about `inert` changes: Cc still becomes a space, whitespace still flattens, the
  length cap and ellipsis are unchanged.

## 5. Scope

**In:** `inert`, `health`'s variation keys, and their tests, in `crates/flags-core/src/telemetry.rs`
and `crates/flags-core/tests/db.rs`.

Also `HEALTH_PROVENANCE` (`crates/flags-mcp/src/tools/out.rs`), which tells the agent to treat
error types and messages as quoted data: it names variation names too, since they are the same kind
of text.

**Out:** what `record_errors` and `record_evaluations` store; any other text an agent sees (flag
names, descriptions, rollback reasons are written by authenticated agents, not public keys).

## 6. Architecture

`crates/flags-core/src/telemetry.rs`:

- New private `fn hidden(c: char) -> bool`: true for `char::is_control` (Cc), every Cf range of
  Unicode 16.0, and every Default_Ignorable_Code_Point range, as one `matches!` over `char` ranges.
- `inert` maps `c` to `' '` when `hidden(c)` instead of when `c.is_control()`.
  Its doc comment says what is replaced and why.
- `health` keys `variations` by `inert(&variation, MAX_VARIATION)` and skips
  a key that cleans to `""`.
- New `const MAX_VARIATION: usize = 64;` beside the other caps, used by `record_evaluations`
  (replacing the literal) and by `health`.

No type or wire-shape changes: `inert` is private, and `Health` keeps its shape. What
`flag_health` returns changes in content only (cleaned text, merged variation counts), in the MCP
view, not the SDK REST contract. The `flag_health` tool description and `HEALTH_PROVENANCE` name
variation names alongside error text.

## 7. Invariants impact

- **SDK-reported text is untrusted when an MCP tool returns it:** strengthened; `flag_health` still
  returns it as data, now without characters that change how it reads.
- **Evaluations are counted, not stored / error reports raw and truncated:** unchanged; storage is
  untouched.
- **SDK REST contract:** unchanged; only the MCP view changes.
- No SQL, migration, RLS, scope, or metering change.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- A string made only of hidden characters flattens to `""`, as a string of only control characters
  does today.
- A hidden character at the cut point: replacement and flattening run before truncation, so the
  cap counts only visible characters and spaces, as before.
- Combining marks other than variation selectors (accents) are kept: they are visible and part of
  ordinary text.

## 11. Testing approach

Unit tests beside the code (pure logic, per `CLAUDE.md` > Notes):

- Extend `reported_text_is_flattened_and_shortened` with U+202E and U+200B.
- Add `reported_text_loses_invisible_characters`: bidi isolates, U+FEFF, a tag-character run
  (`"\u{E0001}\u{E0069}\u{E0067}"`), a variation selector, the soft hyphen; and that an ordinary
  accented letter (`"é"` as `e\u{0301}`) is kept.
- A hidden character at the truncation boundary: `inert(&format!("{}\u{200B}{}", "x".repeat(298), "yy"), 300)`
  is `"x"*298 + " y…"`, pinning that truncation counts the cleaned text.

DB test in `crates/flags-core/tests/db.rs` (it needs `flag_eval_hourly`), beside
`telemetry_rolls_up_and_feeds_health`: `health_shows_variation_names_cleaned` records enabled
evaluations with variations `"a\u{202E}b"` (2), `"a\u{200B}b"` (3), `"\u{200B}"` (1), and `"on"` (4)
and asserts `variations == {"a b": 5, "on": 4}` and `evaluations.enabled == 10`.

Gates: test, lint, format, extra, and the `crates/**` podman build.

## 12. Risks and open questions

- **Future Unicode versions add Cf or default-ignorable characters** the fixed table will not
  know. The table names its Unicode version; a later update is a one-line change, and a unit test
  checks every range bound and its visible neighbours.
- **Look-alike blanks** (U+2800, unusual spaces) still pass through; they can pad, not hide.
- **`top_errors` is grouped before it is cleaned**, so two groups can read alike; and the number of
  distinct variation names a client key can create is not bounded. Both predate this change and
  are filed as #59.
