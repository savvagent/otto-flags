# Plan: client-key origin allowlist (#143, PR 2)

Spec: `docs/specs/2026-10-23-client-origin-allowlist-design.md`.

- [x] Migration `0011_app_allowed_origins.sql`: `flag_apps.allowed_origins` with a 20-entry CHECK.
- [x] `flags_core::apps`: `FlagApp::allowed_origins`, `normalize_origins`, `origin_allowed`, `set_app_allowed_origins` (idempotent, audited).
- [x] `flags-api`: `ApiError::origin_not_allowed`; `check_origin` in `evaluate` and `list_flags`.
- [x] MCP `update_app`: optional `environments` and `allowedOrigins`; tool descriptions; `llms.txt` (both copies).
- [x] Tests: normalization and matching units; db (default, set, idempotent, refused input, clear); e2e (allowed, other origin, no `Origin`, server key, clear, admin only, bad input, environments keep origins).
- [x] Docs: `CLAUDE.md` client-keys invariant and security focus, `docs/deploy/fly.md`, `docs/SDK-DEVELOPER-GUIDE.md`.
- [ ] Gates: test, clippy, fmt, migrations-append-only, `podman build`.
- [ ] Final commit: spec `**Status:**` becomes `IMPLEMENTED (#<PR>)`; tick these boxes.
