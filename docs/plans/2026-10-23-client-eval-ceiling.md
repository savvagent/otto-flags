# Plan: client-key evaluation ceiling (#143, PR 1)

Spec: `docs/specs/2026-10-23-client-eval-ceiling-design.md`.

- [x] Migration `0010_client_eval_ceiling.sql`: `usage_meter_hourly.served_client`; `usage_meter_daily` with RLS.
- [x] `EvalCounter`: `served_client`, `Counts::served_client`, flush UPSERT with the clamp.
- [x] `CLOSE`: ceiling bind, ceiled quantity, `usage_meter_daily` upsert, clipped totals and rows; `close_due_at`; `run` takes the ceiling; WARN per clipped hour.
- [x] `evaluate`: count only the app's own environments; client keys through `served_client`.
- [x] Config `FLAGS_CLIENT_EVAL_CEILING_PER_HOUR` (default 500000) in `config.rs`, `fly.toml`, `.env.example`, `docs/deploy/fly.md`.
- [x] `MeteringExt::evaluation_split`; MCP `usage` gains `evaluations`.
- [x] Sweep (90 days) and org purge cover `usage_meter_daily`; isolation test lists it.
- [x] Tests: counter, flush, close (clipping, telemetry-billed, blocked, saturated, day accumulation), split query, sweep, purge, e2e (environment, ceiling and `usage`).
- [x] Docs: `CLAUDE.md` Metering invariant, `docs/deploy/fly.md`, `docs/SDK-DEVELOPER-GUIDE.md`, module docs.
- [x] Gates: test, clippy, fmt, migrations-append-only, `podman build`.
- [x] Final commit: spec `**Status:**` becomes `IMPLEMENTED (#<PR>)`; tick these boxes.
