# Android SDK Flag-Change Stream Consumer Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `packages/android-sdk` reads `GET /api/flags/stream` when `enableRealtime` is true and invalidates cached evaluations.

**Architecture:** see the spec; `RealtimeStream.kt` on the client's coroutine scope.

**Tech Stack:** Kotlin 1.9, kotlinx-coroutines, OkHttp 4.12, JUnit 4, MockWebServer.

**Ticket:** savvagent/otto-flags#167 (`Refs`)

**Spec:** docs/specs/2026-10-23-android-sdk-sse-consumer-design.md

## Global Constraints

- No configured gate for this package (no Gradle wrapper). Compile and run what the host allows (Android Studio's kotlinc, the JDK in its jbr, jars in `~/.m2` and `~/.gradle/caches`) and report exactly what ran; Gradle itself is not run unless it works offline.
- Invariants: nothing thrown into the host; key only in the header; no new dependency; no version bump; MIT.
- Worktree only; trunk via PR. Commit format `<type>(<scope>): <imperative, lower-case>`, why-body, `Refs #167`; no attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/android-sdk/src/main/kotlin/com/savvagent/ottoflags/sdk/RealtimeStream.kt` | parser, delay, stream coroutine |
| **Create.** `packages/android-sdk/src/test/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClientRealtimeTest.kt` | tests |
| **Modify.** `OttoFlagsClient.kt` | config field, start/stop, internal hooks |
| **Modify.** `packages/android-sdk/README.md`, `crates/flags-mcp/content/guides/android.md` | docs |

### Task 1: consumer and tests
- [ ] Tests per the spec, then `RealtimeStream.kt` and wiring; compile and run as far as possible; commit `feat(android): consume the flag-change stream in the Android SDK`.

### Task 2: docs
- [ ] README, KDoc, `android.md`; Rust workspace gates + container build; commit `docs(android): describe the Android SDK's real-time updates`.
