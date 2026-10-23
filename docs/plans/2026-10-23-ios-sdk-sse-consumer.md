# iOS SDK Flag-Change Stream Consumer Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `packages/ios-sdk` reads `GET /api/flags/stream` when `enableRealtime` is true and invalidates cached evaluations.

**Architecture:** see the spec; `RealtimeStream.swift` on a `URLSessionDataDelegate`, started in `OttoFlagsClient.init`, stopped in `close()`/`deinit`.

**Tech Stack:** Swift 5.9, Foundation (`URLSession`), XCTest.

**Ticket:** savvagent/otto-flags#167 (`Refs`)

**Spec:** docs/specs/2026-10-23-ios-sdk-sse-consumer-design.md

## Global Constraints

- Gate (NOT runnable here: no Swift toolchain; also known red per #119): `cd packages/ios-sdk && swift test`. Report as not run. Task 2 also the Rust workspace gates and `podman build -t otto-flags:check .` for the `crates/` text.
- Evidence: never imply a gate passed that did not run; re-read the code for compile errors.
- Invariants: nothing thrown into the host; key only in the header; no new dependency; no version bump; MIT; deployment targets unchanged.
- Worktree only; trunk via PR. Commit format `<type>(<scope>): <imperative, lower-case>`, why-body, `Refs #167`; no attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/ios-sdk/Sources/OttoFlagsSDK/RealtimeStream.swift` | parser, delay, delegate, reconnect |
| **Create.** `packages/ios-sdk/Tests/OttoFlagsSDKTests/RealtimeStreamTests.swift` | tests |
| **Modify.** `OttoFlagsClient.swift` | config field, `close()`, start/stop |
| **Modify.** `packages/ios-sdk/README.md`, `crates/flags-mcp/content/guides/ios.md` | docs |

### Task 1: consumer and tests
- [ ] Tests per the spec, then `RealtimeStream.swift` and wiring; careful compile re-read; commit `feat(ios): consume the flag-change stream in the iOS SDK`.

### Task 2: docs
- [ ] README, doc comments, `ios.md`; Rust workspace gates + container build; commit `docs(ios): describe the iOS SDK's real-time updates`.
