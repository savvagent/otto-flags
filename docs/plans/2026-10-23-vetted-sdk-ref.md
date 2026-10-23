# Vetted SDK Ref Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [x]`) syntax for tracking.

**Goal:** `sdk_guide`, `AGENTS.md` and `llms.txt` pin SDK installs to one maintainer-chosen commit
(`SDK_REF`) instead of telling the agent to pick a SHA and have a person approve it.

**Architecture:** A `pub const SDK_REF` in `crates/flags-mcp/src/guide.rs` is substituted for a new
`{{SDK_REF}}` placeholder in `render`. The 17 guides replace `<commit>` with the placeholder and drop
the `ls-remote`/approve text. `AGENTS.md` and `llms.txt` (two byte-identical copies) are static, so
they carry the literal SHA and tests tie them to the constant.

**Tech Stack:** Rust (edition 2021), Markdown content compiled in with `include_str!`.

**Ticket:** savvagent/otto-flags#184

**Spec:** `docs/specs/2026-10-23-vetted-sdk-ref-design.md`

## Global Constraints

- Gates (run from the worktree, exact commands):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
    (during a task the targeted form is `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-mcp --lib guide`)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` must print nothing
  - `podman build -t otto-flags:check .` (crates/** changes)
- Evidence: report pass counts per test binary. `cargo test` without `DATABASE_URL` panics; that is a
  missing variable, not a failure. `DATABASE_URL` points only at the local Postgres on 15434. A
  doc-test link failure under `/tmp` tmpfs: set `TMPDIR` to a directory on disk.
- Invariants that apply: no SQL, auth or SDK-path change; `crates/` and repo-level files are
  AGPL-3.0-or-later; `sdk_guide` stays free, read-only, static. No migration.
- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/feat-184-vetted-sdk-ref`); trunk changes only through the PR.
- Commit format: conventional commits, `<type>(<scope>): <imperative, lower-case>`; body explains
  why and ends `Fixes #184`; no attribution of any kind (no Co-Authored-By, no "Generated with", no
  credit markers).
- Guide text must not mention a registry or publication (`no_guide_mentions_a_registry`).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-mcp/src/guide.rs` | `SDK_REF` constant, `{{SDK_REF}}` substitution, module docs, tests |
| **Modify.** `crates/flags-mcp/content/guides/*.md` (17 files) | Replace `<commit>` with `{{SDK_REF}}`; drop `ls-remote` and approve-the-SHA text |
| **Modify.** `crates/flags-mcp/content/llms.txt` and `llms.txt` (identical) | Literal SHA, no `<commit>`, no approve sentence |
| **Modify.** `AGENTS.md` | Same, in "Install from a pinned commit" |

## Task Order & Rationale

Task 1 adds the constant, substitution and its own tests without touching content, so it ends green.
Task 2 changes the guides and the guide-text tests together. Task 3 changes the two static files and
their agreement test. Each task ends with every gate that applies passing.

---

### Task 1: SDK_REF constant and substitution

**Files:**
- Modify: `crates/flags-mcp/src/guide.rs` (module docs near line 12-26, `render` near line 225, tests module)

**Interfaces:** produces `pub const SDK_REF: &str` and the `{{SDK_REF}}` placeholder used by Tasks 2-3.

**Out-of-band / invariants:** none.

**Model:** sonnet — contained Rust change with an exact spec.

- [x] **Step 1: Write the failing tests** (append inside `mod tests`, next to `no_rendering_contains_a_server_key_prefix`)

```rust
    #[test]
    fn sdk_ref_is_a_full_lowercase_sha() {
        assert_eq!(SDK_REF.len(), 40, "SDK_REF must be a full 40-character SHA");
        assert!(
            SDK_REF
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "SDK_REF must be lowercase hex: {SDK_REF}"
        );
    }
```

(The substitution itself is proven end to end by Task 2's `every_rendering_pins_the_sdk_ref`; until
then `render`'s new `.replace` has no placeholder to act on, and the existing
`placeholders_are_filled_from_the_app_or_left_as_angle_brackets` guards against a stray `{{`.)

- [x] **Step 2: Run to verify they fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-mcp --lib guide::tests::sdk_ref_is_a_full_lowercase_sha`
Expected: FAIL to compile (`SDK_REF` not found).

- [x] **Step 3: Implement**

Add above `pub fn render` (after `environments_text`):

```rust
/// The commit of `savvagent/otto-flags` every SDK install in the guides, `AGENTS.md` and
/// `llms.txt` is pinned to, so a consuming developer does not have to review a SHA an agent
/// picked: the maintainers choose it here, once.
///
/// It must be a full 40-character lowercase commit SHA already on `main`. A PR cannot name its own
/// squash-merge commit, so bump it in a separate small PR after a merge that changes SDK code or a
/// guide's claims about SDK behavior. Bumping means changing this constant and the literal copies
/// in `AGENTS.md` and `llms.txt` (both `llms.txt` files); the tests below fail until all agree.
pub const SDK_REF: &str = "4c5afb7f5efabfbf32dad22e6252aa6f26e59a66";
```

In `render`, add `.replace("{{SDK_REF}}", SDK_REF)` to the chain after `.replace("{{ENVIRONMENTS}}", ...)`:

```rust
        .replace("{{ENVIRONMENTS}}", &environments_text(app))
        .replace("{{SDK_REF}}", SDK_REF);
```

Update the module-doc placeholder table: change "A guide or the recipe may carry four placeholders"
to "five placeholders" and add the row

```
//! | `{{SDK_REF}}`      | [`SDK_REF`]          | [`SDK_REF`]      |
```

and after the table add the sentence: `{{SDK_REF}}` does not depend on the app.
Update the first doc paragraph's "Install routes are commit-pinned source routes (`<commit>`)" to
"(`{{SDK_REF}}`)".

- [x] **Step 4: Run to verify they pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-mcp --lib guide`
Expected: PASS, none skipped; `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean.

- [x] **Step 5: Commit**

```bash
git add crates/flags-mcp/src/guide.rs
git commit -m "feat(mcp): add the sdk ref constant and its placeholder

The guides will pin installs to a maintainer-chosen commit instead of one the
agent picks. This adds the constant and the substitution.

Fixes #184"
```

### Task 2: Pin the guides to SDK_REF

**Files:**
- Modify: all 17 files in `crates/flags-mcp/content/guides/` (android, angular, astro, go, ios, java, nextjs, node, python, react, remix, rust, solid, svelte, sveltekit, typescript, vue)
- Modify: `crates/flags-mcp/src/guide.rs` (tests)
- Modify: `crates/flags-server/tests/e2e.rs` (line ~1704, `sdk_guide_renders_a_guide_and_personalizes_it_with_the_apps_public_key`)

**Interfaces:** consumes `SDK_REF` and the placeholder from Task 1.

**Out-of-band / invariants:** keep the pnpm paragraph containing "runs the install scripts of this repository's dependencies" and every `--ignore-scripts`; add no registry words.

**Model:** sonnet — mechanical edits, 17 files, exact rules; tests catch misses.

- [x] **Step 1: Write the failing tests / update the checks in `guide.rs`**

Replace `every_guide_has_the_commit_pin_and_the_standard_sections`' first assertion:

```rust
            assert!(l.md.contains("{{SDK_REF}}"), "{}: no pinned ref", l.name);
            assert!(!l.md.contains("<commit>"), "{}: still has <commit>", l.name);
```

Delete the `REVIEW_SHA` constant, `every_guide_tells_the_agent_to_have_the_sha_reviewed` and
`agents_md_and_llms_txt_tell_the_agent_to_have_the_sha_reviewed` (keep the `flat` helper, it is used
elsewhere). Add:

```rust
    /// #184: the guide supplies the ref, so nothing may still ask a person to approve a SHA or tell
    /// the agent to go and find one.
    #[test]
    fn no_guide_asks_a_person_to_approve_a_sha_or_to_find_one() {
        for l in LANGUAGES {
            let text = flat(l.md);
            for gone in [
                "review and approve the SHA",
                "the approved SHA",
                "git ls-remote",
                "Find the current tip",
            ] {
                assert!(!text.contains(gone), "{}: still says {gone:?}", l.name);
            }
            assert!(
                text.contains("not a fork") || text.contains("from a fork"),
                "{}: lost the not-a-fork rule",
                l.name
            );
        }
    }

    #[test]
    fn every_rendering_pins_the_sdk_ref() {
        let app = sample_app();
        for l in LANGUAGES {
            for app in [None, Some(&app)] {
                let text = render(l.name, app).unwrap().1;
                assert!(text.contains(SDK_REF), "{}: ref not in rendering", l.name);
                assert!(!text.contains("<commit>"), "{}: <commit> left", l.name);
                assert!(!text.contains("{{SDK_REF}}"), "{}: unfilled", l.name);
            }
        }
    }
```

In `install_line_problems`, the install-route tests and the `MANIFESTS` table, replace every
`<commit>` with `{{SDK_REF}}` (lines near 690, 708, 780, 786, 996-1005, 1034, 1065-1070 of the
current file; use `grep -n '<commit>' crates/flags-mcp/src/guide.rs` and change them all, including
all of them except the `"<commit>"` needle in `agents_md_points_agents_at_the_guide`, which Task 3 removes). After this step `grep -n '<commit>' crates/flags-mcp/src/guide.rs` must list only that needle.

In `crates/flags-server/tests/e2e.rs`, replace the assertion `assert!(text.contains("<commit>"), "{text}");` (python guide, near line 1704) with:

```rust
    assert!(text.contains(flags_mcp::guide::SDK_REF), "{text}");
    assert!(!text.contains("<commit>"), "{text}");
```

- [x] **Step 2: Run to verify they fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-mcp --lib guide`
Expected: FAIL (guides still contain `<commit>` and the approve sentence).

- [x] **Step 3: Edit the 17 guides**

Rule, applied to the `## Install` section of each guide:

1. Delete the sentence `Find the current tip of \`main\` with:` and the following ```` ```bash ```` block containing `git ls-remote https://github.com/savvagent/otto-flags main`. The sentence that precedes it ("Install from source, pinned to a full commit SHA of `savvagent/otto-flags`." and its variants) stays, ending in a full stop.
2. Replace the paragraph that begins `Ask a person to review and approve the SHA before you pin it` and ends `...in place of \`<commit>\`` (plus whatever continues that same sentence, such as `. Build and pack each package in dependency order ...` or `:`) with this paragraph, keeping the continuing text as its own following sentence or the colon as the lead-in to the code block:

   > Pin to `{{SDK_REF}}`, a commit on `main` of `savvagent/otto-flags` that the maintainers have reviewed. Use it exactly: do not substitute a branch, a newer commit, or a commit from a fork, and use the same commit everywhere in one project.

3. Replace every remaining `<commit>` with `{{SDK_REF}}`.
4. Reword any remaining "approved SHA" / "the SHA" prose so it no longer implies a person approves it (for example `Pin a SHA, not a branch, so a build is reproducible.` stays; `keep \`<commit>\` recorded in your build` becomes `keep \`{{SDK_REF}}\` recorded in your build`).

Worked example (python.md, Install):

````markdown
Install from source, pinned to a full commit SHA of `savvagent/otto-flags`. Pin to `{{SDK_REF}}`, a
commit on `main` of `savvagent/otto-flags` that the maintainers have reviewed. Use it exactly: do not
substitute a branch, a newer commit, or a commit from a fork, and use the same commit everywhere in
one project.

```bash
pip install "otto-flags @ git+https://github.com/savvagent/otto-flags@{{SDK_REF}}#subdirectory=packages/python-server"
```
````

Worked example (node.md, which continues the sentence): the paragraph becomes the pin paragraph
above, then a new sentence `Build and pack each package in dependency order (\`@otto-flags/sdk\`, \`@otto-flags/node-server\`), because ...` unchanged from the existing text.

Placement differs by guide: in `android.md` and `java.md` the `Find the current tip of `main`` sentence sits mid-line after the Maven-local/Gradle sentence and wraps onto the next line; in `rust.md` keep the full stop of the sentence before it. The rule's result is the same in every guide.

Leave the pnpm install-scripts paragraph and every `--ignore-scripts` line unchanged. Keep
Markdown hard-wrapped near 100 columns like the surrounding text.

- [x] **Step 4: Run to verify they pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`, then `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`.
Expected: PASS (none skipped), including `js_guides_skip_dependency_install_scripts_and_say_why`,
`no_guide_mentions_a_registry`, `every_rendering_pins_the_sdk_ref`. Also run
`grep -rn '<commit>' crates/flags-mcp/content/guides` (expect no output) and read the rendered Install
section of three guides (`python`, `node`, `go`) to confirm they read naturally.

- [x] **Step 5: Commit**

```bash
git add crates/flags-mcp/src/guide.rs crates/flags-mcp/content/guides crates/flags-server/tests/e2e.rs
git commit -m "feat(mcp): pin every sdk guide to the maintainer-chosen ref

An agent no longer picks a SHA and asks the developer to approve it; the guide
supplies the commit the maintainers reviewed.

Fixes #184"
```

### Task 3: AGENTS.md and llms.txt carry the same ref

**Files:**
- Modify: `AGENTS.md` (section "Install from a pinned commit", around lines 22-35)
- Modify: `crates/flags-mcp/content/llms.txt` and `llms.txt` (identical; line 10 and the Python quick start near line 67)
- Modify: `crates/flags-mcp/src/guide.rs` (tests)

**Interfaces:** consumes `SDK_REF`.

**Out-of-band / invariants:** root `llms.txt` stays byte-identical to the content copy; `/llms.txt` e2e test in `crates/flags-server` follows the file.

**Model:** sonnet.

- [x] **Step 1: Write the failing tests** (in `guide.rs`)

Replace the `"<commit>"` needle in `agents_md_points_agents_at_the_guide` with nothing (remove it from the list), and add:

```rust
    #[test]
    fn agents_md_and_llms_txt_pin_the_sdk_ref() {
        let agents = std::fs::read_to_string(repo("AGENTS.md")).unwrap();
        for (what, text) in [("AGENTS.md", agents.as_str()), ("llms.txt", crate::LLMS_TXT)] {
            assert!(
                text.contains(SDK_REF),
                "{what} must carry SDK_REF {SDK_REF}; bump it with the constant"
            );
            assert!(!text.contains("<commit>"), "{what}: <commit> left");
            let flat = flat(text);
            for gone in ["review and approve the SHA", "git ls-remote"] {
                assert!(!flat.contains(gone), "{what}: still says {gone:?}");
            }
        }
    }
```

- [x] **Step 2: Run to verify it fails**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-mcp --lib guide::tests::agents_md_and_llms_txt_pin_the_sdk_ref`
Expected: FAIL.

- [x] **Step 3: Edit the files**

`AGENTS.md`: retitle the section `## Install from a pinned commit`, keep it, and replace its body's
first paragraph through "Use the same SHA everywhere in one project." with:

```markdown
SDKs are installed from a commit of this repository, never from a floating branch. The commit is
`<SDK_REF literal>`, a commit on `main` of `savvagent/otto-flags` that the maintainers have reviewed;
`sdk_guide` pins its installs to the same commit. Use it exactly: do not substitute a branch, a newer
commit, or a commit from a fork, and use the same commit everywhere in one project.
```

(where `<SDK_REF literal>` is the 40-character value of `SDK_REF`, written out). Keep the pnpm paragraph after it.

`llms.txt` (both copies, identical edit): replace the bullet beginning `- Every SDK install is pinned to a commit of savvagent/otto-flags. Find one with ...` with

```
- Every SDK install is pinned to commit <SDK_REF literal> of savvagent/otto-flags, a commit on `main` that the maintainers have reviewed; `sdk_guide` pins its installs to the same commit. Use it exactly: do not substitute a branch, a newer commit, or a commit from a fork.
```

and replace `@<commit>` in the Python quick start with `@<SDK_REF literal>`.

- [x] **Step 4: Run to verify it passes**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
Expected: PASS, none skipped, per test binary counts reported (includes flags-server's `/llms.txt`
test and `llms_txt_is_one_definition_and_covers_every_tool_and_language`). Then
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the migrations
`extra` gate (no output), and `podman build -t otto-flags:check .`. Also `diff llms.txt crates/flags-mcp/content/llms.txt` prints nothing and `grep -rn '<commit>' AGENTS.md llms.txt crates/flags-mcp` prints nothing.

- [x] **Step 5: Commit**

```bash
git add AGENTS.md llms.txt crates/flags-mcp/content/llms.txt crates/flags-mcp/src/guide.rs
git commit -m "docs(mcp): pin agents.md and llms.txt to the sdk ref

They are static, so they carry the literal commit and a test ties them to the
constant the guides use.

Fixes #184"
```
