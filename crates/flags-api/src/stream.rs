//! `GET /api/flags/stream`: server-sent flag-change events for one app.
//!
//! Events, as `docs/SDK-DEVELOPER-GUIDE.md` names them: `connected` once,
//! `heartbeat` every 30 s, and `flag.created` / `flag.updated` /
//! `flag.deleted` (archived) carrying the flag's new state. A subscriber that
//! falls behind skips what it missed (the SDKs also poll), and the stream ends
//! when the key stops resolving, so a rotated or deleted key cannot keep
//! listening.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use flags_core::flags::{FlagsExt, SdkFlag};
use flags_core::keys;
use flags_core::notify::FlagChange;
use flags_core::platform_events::begin_live;
use futures::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use crate::error::ApiResult;
use crate::sdk::Sdk;
use crate::AppState;

pub async fn stream(
    State(state): State<AppState>,
    sdk: Sdk,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let owner = sdk.0;
    let sdk_for_stream = sdk.clone();
    let connected = Event::default()
        .event("connected")
        .json_data(serde_json::json!({
            "message": "connected to otto-flags",
            "organization_id": owner.org_id.to_string(),
            "application_id": owner.app_id.to_string(),
        }));
    let first = stream::iter(connected.ok().map(Ok));

    let rx = state.listener.subscribe();
    let heartbeat = state.heartbeat;
    let changes = stream::unfold(
        (rx, state, sdk_for_stream),
        move |(mut rx, state, sdk)| async move {
            loop {
                let change: FlagChange = match tokio::time::timeout(heartbeat, rx.recv()).await {
                    Err(_) => {
                        // Quiet period: confirm the key still opens this app, then
                        // send a heartbeat. A key that stopped resolving ends the
                        // stream.
                        if !still_valid(&state, &sdk).await {
                            return None;
                        }
                        let ev = Event::default().event("heartbeat").data("ping");
                        return Some((Ok(ev), (rx, state, sdk)));
                    }
                    Ok(Err(RecvError::Lagged(n))) => {
                        tracing::debug!(skipped = n, "flag stream subscriber fell behind");
                        continue;
                    }
                    Ok(Err(RecvError::Closed)) => return None,
                    Ok(Ok(c)) => c,
                };
                if change.org_id != owner.org_id || change.app_id != owner.app_id {
                    continue;
                }
                match notification(&state, &sdk, &change).await {
                    Some(ev) => return Some((Ok(ev), (rx, state, sdk))),
                    None => continue,
                }
            }
        },
    );

    Ok(Sse::new(first.chain(changes)).keep_alive(KeepAlive::default()))
}

async fn still_valid(state: &AppState, sdk: &Sdk) -> bool {
    // A database error keeps the stream open: an outage should not disconnect
    // every SDK at once. Only a definite "this key is gone" ends it.
    !matches!(keys::resolve_hash(&state.db, &sdk.1).await, Ok(None))
}

async fn notification(state: &AppState, sdk: &Sdk, change: &FlagChange) -> Option<Event> {
    let mut tx = begin_live(&state.db, change.org_id, None).await.ok()?;
    let flag = tx.find_flag(change.app_id, &change.key).await.ok()??;
    tx.commit().await.ok()?;
    // The state is read after the notification, so it may be newer than the
    // version announced; send what is current.
    let view = SdkFlag::from_flag(&flag, "", sdk.sees_rules());
    Event::default()
        .event(change.kind.event_name())
        .json_data(serde_json::json!({
            "type": change.kind.event_name(),
            "key": flag.key,
            "organization_id": flag.org_id.to_string(),
            "application_id": flag.app_id.to_string(),
            "environments": view.environments,
            "variations": view.variations,
            "configuration": view.configuration,
            "status": flag.status,
            "version": flag.version,
            "realtime_enabled": true,
        }))
        .ok()
}
