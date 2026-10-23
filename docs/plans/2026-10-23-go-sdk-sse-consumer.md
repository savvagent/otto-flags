# Go SDK Flag-Change Stream Consumer Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `packages/go-server` reads `GET /api/flags/stream` when `EnableRealtime` is true and invalidates cached evaluations.

**Architecture:** see the spec; one goroutine in `realtime.go`, started in `NewClient`, stopped in `Close`.

**Tech Stack:** Go 1.21 stdlib (`net/http`, `bufio`, `httptest`).

**Ticket:** savvagent/otto-flags#167 (`Refs`)

**Spec:** docs/specs/2026-10-23-go-sdk-sse-consumer-design.md

## Global Constraints

- Gates (NOT runnable here: no Go toolchain; report as not run): `cd packages/go-server && go vet ./...`, `go test ./...`. Task 2 also: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- Evidence: gate not run for lack of a toolchain must be said, never implied passed; read the code twice for compile errors instead.
- Invariants: no panics into the host app; key only in the header; no new dependency; no version bump; MIT package.
- Worktree only; trunk only via PR. Commit format `<type>(<scope>): <imperative, lower-case>`, body why, ends `Refs #167`; no attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/go-server/realtime.go` | SSE parser, reconnect delay, stream goroutine |
| **Create.** `packages/go-server/realtime_test.go` | tests above |
| **Modify.** `packages/go-server/client.go`, `types.go` | start/stop, field for test timings, doc comment |
| **Modify.** `packages/go-server/README.md`, `crates/flags-mcp/content/guides/go.md` | accurate real-time docs |

### Task 1: Go consumer and tests
- [ ] Write `realtime_test.go` per the spec's Testing list, see it fail to compile/run (cannot run: reason it through), implement `realtime.go`, wire `NewClient`/`Close`, re-read for compile errors, commit `feat(go): consume the flag-change stream in the Go SDK`.

### Task 2: Docs
- [ ] Update README, `types.go` comment, and `go.md` (say the default is off and `EnableRealtime: true` turns it on); run the Rust workspace gates for the `crates/` text and `podman build -t otto-flags:check .`; commit `docs(go): describe the Go SDK's real-time updates`.
