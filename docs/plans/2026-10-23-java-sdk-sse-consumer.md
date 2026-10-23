# Java SDK Flag-Change Stream Consumer Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `packages/java-server` reads `GET /api/flags/stream` when `enableRealtime` is true (the default) and invalidates cached evaluations.

**Architecture:** see the spec; a package-private `RealtimeStream` built on `okhttp3.sse`, started in the `FlagClient` constructor, stopped in `close()`.

**Tech Stack:** Java 11, OkHttp 4.12 (okhttp-sse, mockwebserver), Gson, SLF4J, JUnit 5.

**Ticket:** savvagent/otto-flags#167 (`Refs`)

**Spec:** docs/specs/2026-10-23-java-sdk-sse-consumer-design.md

## Global Constraints

- Gate (NOT runnable here: no Maven or javac; report as not run): `cd packages/java-server && mvn -B verify`. Task 2 also the Rust workspace gates and `podman build -t otto-flags:check .` for the `crates/` text.
- Evidence: never imply a gate passed that did not run; re-read the code for compile errors.
- Invariants: nothing thrown into the host; key only in the header; no new dependency; no version bump; MIT.
- Worktree only; trunk via PR. Commit format `<type>(<scope>): <imperative, lower-case>`, why-body, `Refs #167`; no attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/RealtimeStream.java` | stream, reconnect, event handling |
| **Create.** `packages/java-server/src/test/java/com/savvagent/ottoflags/sdk/RealtimeStreamTest.java` | tests |
| **Modify.** `FlagClient.java`, `FlagClientConfig.java` (javadoc) | start/stop |
| **Modify.** `packages/java-server/README.md`, `crates/flags-mcp/content/guides/java.md` | docs |

### Task 1: consumer and tests
- [ ] Tests per the spec, then `RealtimeStream`, wiring in `FlagClient`, careful compile re-read, commit `feat(java): consume the flag-change stream in the Java SDK`.

### Task 2: docs
- [ ] README, javadoc, `java.md`; Rust workspace gates + container build; commit `docs(java): describe the Java SDK's real-time updates`.
