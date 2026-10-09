# otto-flags: vision and scope

**Status:** v1 is built and deployed (see `docs/plans/2026-10-09-build-and-deploy.md`
for what it includes and what was deferred). This document is the standing statement of *why* and *what*;
it should stay accurate as implementation proceeds even as the design doc's specifics
change.

## The problem

Feature flag platforms are built for humans to click through a dashboard. That's a
mismatch now that agentic coding agents are the ones writing the code that checks
flags, shipping the rollouts, and often the first responder when a flag causes an
incident. A human-first UI with an MCP integration bolted on the side still makes the
agent go through a human-shaped interface to do agent-shaped work.

## The vision

otto-flags inverts that: **flag management is something an agent does by calling MCP
tools, not a UI a human clicks through.** Creating a flag, setting targeting rules,
advancing or rolling back a rollout, assessing the risk of shipping at 100%,
correlating a flag with an error spike — all of it is a tool call an agentic coding
assistant makes as a normal part of shipping and operating software.

The one deliberate exception is flag *evaluation* inside a running production app
(`isEnabled()` on every request). That path stays SDK/REST, not MCP, because a
production request can't afford an LLM tool-call round trip. Agents manage flags;
running applications evaluate them.

## Scope: two servers, one product

1. **The MCP server** — the agent-facing surface. CRUD for flags, targeting rules,
   and segments; rollout and rollback tools; risk assessment and incident
   correlation. This is what an agentic coding tool talks to when it's building,
   shipping, or operating a feature behind a flag. See the design doc §6 for the
   current tool-surface sketch.
2. **The evaluation server + SDKs** — what a running production app talks to.
   Client, framework, mobile, and server SDKs (already scaffolded under
   `packages/`) call a REST/WebSocket API to evaluate flags on the hot path, with
   caching and real-time updates so evaluation never depends on an LLM being in
   the loop.

Both are otto-flags. Neither is optional: an MCP server with no evaluation path
can't run in production; an evaluation SDK with no agent-facing management surface
is just another human-operated flag dashboard, which is the thing this project
exists to not be.

## What's out of scope here

- **Identity, auth, billing, tenant console.** These live in the shared
  `otto-platform` substrate that every otto-* service (otto-factory, otto-flags,
  …) builds on. otto-flags depends on it at the network boundary (token
  validation), not as vendored source. See design doc §2–§4.
- **A human dashboard as the primary interface.** A console may exist for
  observability and org administration, but it is not where flags get managed —
  that's the agent's job, via MCP.
- **Migrating savvagent-flags data.** There are no production users on the
  predecessor; this is a clean build, not a migration. See design doc §8.

## How to use this document vs. the design doc

This file answers "what are we building and why" and should rarely change.
`docs/specs/2026-09-15-otto-flags-design.md` answers "how, specifically, right now"
— domain model, MCP tool signatures, repo layout, open questions — and will be
superseded or split into more docs as implementation decisions get made. If the two
ever disagree, this document's framing wins; the design doc's specifics should be
updated to match.
