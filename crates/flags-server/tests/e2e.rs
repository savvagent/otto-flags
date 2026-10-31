//! The whole server, end to end: an agent manages a flag over MCP (bearer
//! token introspected by a mock platform), an app evaluates it over the SDK
//! API, telemetry flows back into `flag_health`, and the platform's webhooks
//! clean up; an auto-rollback policy turns a failing flag off by itself.
//! Needs `DATABASE_URL` (see `compose.yaml`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use flags_core::apps::AppsExt;
use flags_core::flags::FlagsExt;
use flags_core::metering::{self, EvalCounter};
use flags_core::notify::{FlagChange, Listener};
use flags_core::platform_events::begin_live;
use flags_core::rollback::{CheckerConfig, Hints, Mode, RollbackExt, Thresholds};
use flags_server::{platform_client, router, spawn_checker, Config};
use http::{Request, StatusCode};
use savvagent_resource::{
    IntrospectionResponse, MemberInfo, MeterDeclarationBatch, MeterDeclarationReceipt, OrgInfo,
    RejectedEvent, Role, TokenKind, UsageBatch, UsageEvent, UsageReceipt, UserInfo,
};
use savvagent_tenant::ids::{OrgId, UserId};
use savvagent_tenant::Db;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const RESOURCE: &str = "https://flags.example.com/mcp";

// ------------------------------------------------------------- mock platform

#[derive(Default)]
struct Platform {
    tokens: HashMap<String, (Uuid, Uuid, Role, Vec<String>)>,
    members: HashMap<(Uuid, Uuid), Role>,
    shipped: usize,
    /// Orgs whose plan includes auto-rollback.
    entitled: HashSet<Uuid>,
    /// Orgs answered as a pre-0.5.0 platform does: no `features` key at all.
    legacy: HashSet<Uuid>,
    /// Orgs whose usage status the platform cannot serve (503).
    unavailable: HashSet<Uuid>,
    /// Meters, as the platform reports them in the usage status, per org.
    /// Orgs absent here are answered as a platform that predates meters does:
    /// no `meters`, `budget` or `blocked_reason` keys.
    meters: HashMap<Uuid, Value>,
    /// The budget object in the usage status, per org.
    budget: HashMap<Uuid, Value>,
    /// The `blocked_reason` in the usage status, per org.
    blocked_reason: HashMap<Uuid, String>,
    /// Usage-status requests received, per org.
    usage_status_hits: HashMap<Uuid, usize>,
    /// When set, every shipped usage event is rejected with this reason.
    reject_with: Option<String>,
    /// When set, answered verbatim to every usage batch, ahead of `reject_with`.
    receipt: Option<UsageReceipt>,
    /// When set, each usage batch hands the test a sender and waits until the
    /// test fires it before answering, so the test can hold shippers mid-call.
    usage_gate: Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>>,
    /// When set, every usage batch is answered 502.
    fail_usage: bool,
    /// How the next `PUT /internal/meters` calls are answered, front first;
    /// once empty, a declaration is accepted.
    meter_modes: std::collections::VecDeque<MeterMode>,
    /// Every request to the meter and usage routes, in arrival order.
    calls: Vec<&'static str>,
    /// Keys of every accepted declaration.
    declared: Vec<String>,
    /// Every usage event received, in order.
    events: Vec<UsageEvent>,
    /// When set, a metered event is rejected as `unknown_meter` unless its
    /// meter is in `declared`, as the real platform does.
    enforce_meters: bool,
    /// The `aud` introspection reports; [`RESOURCE`] when unset.
    aud: Option<String>,
}

/// How the mock answers one meter declaration.
#[derive(Clone, Copy)]
enum MeterMode {
    /// The platform has no slug for this service: `409 service_not_set`.
    ServiceNotSet,
    /// `200` with every meter in `rejected`.
    Rejected,
}

type Shared = Arc<Mutex<Platform>>;

async fn introspect(
    State(p): State<Shared>,
    Form(f): Form<HashMap<String, String>>,
) -> Json<IntrospectionResponse> {
    let p = p.lock().unwrap();
    Json(match p.tokens.get(&f["token"]) {
        Some((org, user, role, scopes)) => IntrospectionResponse {
            active: true,
            sub: Some(*user),
            org_id: Some(*org),
            role: Some(*role),
            scope: Some(scopes.join(" ")),
            aud: Some(p.aud.clone().unwrap_or_else(|| RESOURCE.into())),
            exp: Some(chrono::Utc::now().timestamp() + 3600),
            client_id: Some("test-client".into()),
            token_type: Some("Bearer".into()),
            token_kind: Some(TokenKind::Oauth),
            jti: Some(Uuid::new_v4()),
        },
        None => IntrospectionResponse::inactive(),
    })
}

/// Built by hand rather than from `UsageStatus` so the `features` key can be
/// left out, as a platform that predates it does.
async fn usage_status(
    State(p): State<Shared>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, StatusCode> {
    let mut p = p.lock().unwrap();
    *p.usage_status_hits.entry(org).or_default() += 1;
    if p.unavailable.contains(&org) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let mut status = json!({
        "org_id": org,
        "plan": "team",
        "period_start": chrono::Utc::now().date_naive(),
        "billable_count": 0,
        "total_count": 0,
        "included_ops": 1000,
        "hard_stop": false,
    });
    if let Some(m) = p.meters.get(&org) {
        status["meters"] = m.clone();
    }
    if let Some(b) = p.budget.get(&org) {
        status["budget"] = b.clone();
    }
    if let Some(r) = p.blocked_reason.get(&org) {
        status["blocked_reason"] = json!(r);
    }
    if !p.legacy.contains(&org) {
        status["features"] = if p.entitled.contains(&org) {
            json!({"auto_rollback": true})
        } else {
            json!({})
        };
    }
    Ok(Json(status))
}

async fn declare_meters(
    State(p): State<Shared>,
    Json(b): Json<MeterDeclarationBatch>,
) -> Result<Json<MeterDeclarationReceipt>, (StatusCode, Json<Value>)> {
    let mut p = p.lock().unwrap();
    p.calls.push("meters");
    match p.meter_modes.pop_front() {
        Some(MeterMode::ServiceNotSet) => Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "service_not_set"})),
        )),
        Some(MeterMode::Rejected) => Ok(Json(MeterDeclarationReceipt {
            declared: vec![],
            rejected: b
                .meters
                .iter()
                .map(|m| savvagent_resource::RejectedMeter {
                    key: m.key.clone(),
                    reason: "outside this service".into(),
                })
                .collect(),
        })),
        None => {
            let keys: Vec<String> = b.meters.iter().map(|m| m.key.clone()).collect();
            p.declared.extend(keys.clone());
            Ok(Json(MeterDeclarationReceipt {
                declared: keys,
                rejected: vec![],
            }))
        }
    }
}

async fn ship(
    State(p): State<Shared>,
    Json(b): Json<UsageBatch>,
) -> Result<Json<UsageReceipt>, StatusCode> {
    {
        let mut p = p.lock().unwrap();
        p.calls.push("usage");
        p.events.extend(b.events.iter().cloned());
    }
    let gate = p.lock().unwrap().usage_gate.clone();
    if let Some(gate) = gate {
        let (release, released) = tokio::sync::oneshot::channel();
        gate.send(release).unwrap();
        released.await.unwrap();
    }
    let mut p = p.lock().unwrap();
    if p.fail_usage {
        return Err(StatusCode::BAD_GATEWAY);
    }
    if let Some(r) = &p.receipt {
        return Ok(Json(r.clone()));
    }
    if let Some(reason) = &p.reject_with {
        return Ok(Json(UsageReceipt {
            accepted: 0,
            duplicates: 0,
            rejected: b
                .events
                .iter()
                .map(|e| RejectedEvent {
                    event_id: e.event_id,
                    reason: reason.clone(),
                })
                .collect(),
        }));
    }
    let unknown: Vec<RejectedEvent> = if p.enforce_meters {
        b.events
            .iter()
            .filter_map(|e| {
                let m = e.meter.as_ref()?;
                (!p.declared.contains(m)).then(|| RejectedEvent {
                    event_id: e.event_id,
                    reason: format!("unknown_meter: {m}"),
                })
            })
            .collect()
    } else {
        vec![]
    };
    let accepted = b.events.len() - unknown.len();
    p.shipped += accepted;
    Ok(Json(UsageReceipt {
        accepted: accepted as u32,
        duplicates: 0,
        rejected: unknown,
    }))
}

async fn member(
    State(p): State<Shared>,
    Path((org, user)): Path<(Uuid, Uuid)>,
) -> Result<Json<MemberInfo>, StatusCode> {
    let p = p.lock().unwrap();
    let role = *p.members.get(&(org, user)).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(MemberInfo {
        user: UserInfo {
            id: user,
            email: Some("dev@example.com".into()),
            name: None,
        },
        org: OrgInfo {
            id: org,
            slug: "acme".into(),
            name: "Acme".into(),
            plan: "team".into(),
        },
        role,
    }))
}

async fn start_platform() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/oauth/introspect", post(introspect))
        .route("/internal/orgs/{org}/usage-status", get(usage_status))
        .route("/internal/usage", post(ship))
        .route("/internal/meters", axum::routing::put(declare_meters))
        .route("/internal/orgs/{org}/members/{user}", get(member))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

fn token(p: &Shared, org: Uuid, user: Uuid, role: Role, scopes: &[&str]) -> String {
    token_with_prefix(p, "sav_at_", org, user, role, scopes)
}

/// A token the mock platform will introspect as active, with the given
/// prefix in front of a well-formed 43-character body.
fn token_with_prefix(
    p: &Shared,
    prefix: &str,
    org: Uuid,
    user: Uuid,
    role: Role,
    scopes: &[&str],
) -> String {
    let t = format!(
        "{prefix}{}",
        "a".repeat(11) + &Uuid::new_v4().simple().to_string()
    );
    assert!(savvagent_resource::looks_like_token(&t), "{t}");
    let mut p = p.lock().unwrap();
    p.tokens.insert(
        t.clone(),
        (
            org,
            user,
            role,
            scopes.iter().map(|s| s.to_string()).collect(),
        ),
    );
    p.members.insert((org, user), role);
    t
}

// --------------------------------------------------------------- the server

struct Server {
    app: Router,
    db: Db,
    platform: Arc<savvagent_resource::PlatformClient>,
    evaluations: Arc<EvalCounter>,
}

async fn server(pool: PgPool, platform_url: &str) -> Server {
    let mut config = Config::for_test();
    config.platform_url = platform_url.into();
    let db = Db::from_pool(pool);
    let platform = platform_client(&config).unwrap();
    let evaluations = Arc::new(EvalCounter::default());
    Server {
        app: router(
            db.clone(),
            platform.clone(),
            Listener::detached(),
            Hints::detached(),
            evaluations.clone(),
            &config,
        ),
        db,
        platform,
        evaluations,
    }
}

/// The background half of a running server: the `LISTEN` connection and the
/// rollback checker, stopped by [`Live::stop`].
struct Live {
    listener: Listener,
    shutdown: tokio::sync::watch::Sender<bool>,
    checker: tokio::task::JoinHandle<()>,
}

impl Live {
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.checker)
            .await
            .expect("the checker stops on shutdown")
            .unwrap();
    }
}

/// [`server`], with a real flag-change listener and a rollback checker that
/// acts on a hint within 50 ms.
async fn live_server(pool: PgPool, platform_url: &str) -> (Server, Live) {
    let mut config = Config::for_test();
    config.platform_url = platform_url.into();
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let listener = Listener::spawn(pool.clone(), rx.clone());
    let db = Db::from_pool(pool);
    let platform = platform_client(&config).unwrap();
    let (hints, checker) = spawn_checker(
        db.clone(),
        platform.clone(),
        &config,
        CheckerConfig {
            debounce: Duration::from_millis(50),
            backstop_every: Duration::from_secs(3600),
            ..Default::default()
        },
        rx,
    );
    let evaluations = Arc::new(EvalCounter::default());
    let server = Server {
        app: router(
            db.clone(),
            platform.clone(),
            listener.clone(),
            hints,
            evaluations.clone(),
            &config,
        ),
        db,
        platform,
        evaluations,
    };
    (
        server,
        Live {
            listener,
            shutdown,
            checker,
        },
    )
}

/// Poll `check` every 50 ms until it holds or `within` passes; whether it held.
async fn eventually<F, Fut>(within: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait up to `within` for a change to `key` at `version` on `rx`.
async fn heard(
    rx: &mut tokio::sync::broadcast::Receiver<FlagChange>,
    key: &str,
    version: i64,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Ok(c) if c.key == key && i64::from(c.version) == version => return,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("the flag-change subscription closed: {e}"),
            }
        }
    })
    .await
    .is_ok()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One tool call, the way an agent makes it. Returns `result` (structured
/// content) or the JSON-RPC `error`.
async fn tool(app: &Router, token: &str, name: &str, args: Value) -> Result<Value, Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": args}});
    let (status, v) = send(
        app,
        Request::post("/mcp")
            .header("host", "flags.example.com")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "{name}: HTTP {status} {v}");
    if let Some(e) = v.get("error") {
        return Err(e.clone());
    }
    let result = &v["result"];
    if result["isError"] == true {
        return Err(result.clone());
    }
    Ok(result["structuredContent"].clone())
}

async fn sdk(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "flags.example.com")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    send(app, req).await
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_agent_ships_a_flag_and_an_app_evaluates_it(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let org = Uuid::new_v4();
    let admin = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Admin,
        &["flags:read", "flags:write", "apps:admin"],
    );
    let member = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Member,
        &["flags:read", "flags:write", "apps:admin"],
    );
    let reader = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Member,
        &["flags:read"],
    );

    let who = tool(&s.app, &admin, "whoami", json!({})).await.unwrap();
    assert_eq!(who["org"]["slug"], "acme");
    assert_eq!(who["role"], "admin");

    // Only an owner or admin may create apps, whatever the token's scopes.
    let err = tool(&s.app, &member, "create_app", json!({"name": "web"}))
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "forbidden", "{err}");

    let created = tool(
        &s.app,
        &admin,
        "create_app",
        json!({"name": "web", "environments": ["production", "staging"]}),
    )
    .await
    .unwrap();
    let server_key = created["keys"]["serverKey"].as_str().unwrap().to_string();
    let client_key = created["keys"]["clientKey"].as_str().unwrap().to_string();
    assert!(server_key.starts_with("srv_") && client_key.starts_with("sdk_"));

    // A read-only token cannot write.
    let err = tool(
        &s.app,
        &reader,
        "create_flag",
        json!({"app": "web", "key": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "insufficient_scope", "{err}");

    let flag = tool(
        &s.app,
        &member,
        "create_flag",
        json!({
            "app": "web", "key": "new-checkout",
            "variations": {"control": {}, "treatment": {"configuration": {"color": "blue"}}},
            "reason": "launching the new checkout"
        }),
    )
    .await
    .unwrap();
    assert_eq!(flag["flag"]["version"], 1);

    // Off until turned on.
    let (status, off) = sdk(
        &s.app,
        "POST",
        "/api/flags/new-checkout/evaluate",
        &server_key,
        Some(json!({"context": {"user_id": "u1", "environment": "production"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(off["enabled"], false);

    let on = tool(&s.app, &member, "set_flag_environment", json!({
        "app": "web", "key": "new-checkout", "environment": "production",
        "enabled": true, "rolloutPercentage": 0, "expectedVersion": 1,
        "rules": [{"attribute": "email", "operator": "ends_with", "values": ["@acme.com"], "variation": "treatment"}],
        "reason": "staff first"
    })).await.unwrap();
    assert_eq!(on["flag"]["version"], 2);

    // A stale version is refused with a code the agent can branch on.
    let err = tool(
        &s.app,
        &member,
        "set_flag_environment",
        json!({
            "app": "web", "key": "new-checkout", "environment": "production",
            "rolloutPercentage": 50, "expectedVersion": 1
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "version_conflict", "{err}");

    let dry = tool(
        &s.app,
        &reader,
        "evaluate_flag",
        json!({
            "app": "web", "key": "new-checkout", "environment": "production",
            "context": {"user_id": "u9", "attributes": {"email": "ada@acme.com"}}
        }),
    )
    .await
    .unwrap();
    assert_eq!(dry["evaluation"]["enabled"], true);
    assert_eq!(dry["evaluation"]["reason"]["kind"], "rule");
    // The dry run changes no flag but is recorded as unbilled usage, as its
    // description says (#89).
    let billable: Vec<bool> =
        sqlx::query_scalar("SELECT billable FROM usage_outbox WHERE tool = 'evaluate_flag'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(billable, vec![false]);

    // The SDK sees the same answer, on both route spellings.
    for path in [
        "/api/flags/new-checkout/evaluate",
        "/api/evaluate/new-checkout",
    ] {
        let (_, e) = sdk(&s.app, "POST", path, &client_key, Some(json!({"context": {
            "user_id": "u9", "environment": "production", "attributes": {"email": "ada@acme.com"}
        }}))).await;
        assert_eq!(e["enabled"], true, "{path}: {e}");
        assert_eq!(e["variation"], "treatment");
        assert_eq!(e["configuration"], json!({"color": "blue"}));
    }
    let (status, _) = sdk(
        &s.app,
        "POST",
        "/api/flags/nope/evaluate",
        &client_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = sdk(
        &s.app,
        "GET",
        "/api/sdk/flags",
        &format!("srv_{}", "0".repeat(64)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The public key lists flags without their targeting rules; the server key sees them.
    let (_, public) = sdk(
        &s.app,
        "GET",
        "/api/sdk/flags?environment=production",
        &client_key,
        None,
    )
    .await;
    assert_eq!(public["count"], 1);
    assert_eq!(public["flags"][0]["enabled"], true);
    assert!(!public.to_string().contains("@acme.com"), "{public}");
    let (_, private) = sdk(
        &s.app,
        "GET",
        "/api/sdk/flags?environment=production",
        &server_key,
        None,
    )
    .await;
    assert!(private.to_string().contains("@acme.com"));
    let (_, ent) = sdk(
        &s.app,
        "GET",
        "/api/sdk/enterprise-flags",
        &client_key,
        None,
    )
    .await;
    assert_eq!(ent["count"], 0);

    // Telemetry flows into flag_health.
    let evals: Vec<Value> = (0..200).map(|i| json!({
        "flag_key": "new-checkout", "result": i % 2 == 0, "user_id": format!("u{i}"),
        "context": {"environment": "production"}, "timestamp": chrono::Utc::now().timestamp()
    })).collect();
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/evaluations",
        &client_key,
        Some(json!({"evaluations": evals})),
    )
    .await;
    assert_eq!(
        (status, r["accepted"].clone()),
        (StatusCode::OK, json!(200))
    );
    let errors: Vec<Value> = (0..30)
        .map(|_| {
            json!({
                "flag_key": "new-checkout", "flag_enabled": true, "error_type": "TypeError",
                "error_message": "cart is undefined", "context": {"environment": "production"},
                "timestamp": chrono::Utc::now().timestamp()
            })
        })
        .collect();
    let (status, _) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/errors",
        &client_key,
        Some(json!({"errors": errors})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let health = tool(
        &s.app,
        &reader,
        "flag_health",
        json!({"app": "web", "key": "new-checkout", "environment": "production"}),
    )
    .await
    .unwrap();
    assert_eq!(health["health"]["evaluations"]["enabled"], 100);
    assert_eq!(health["health"]["errors"]["enabled"], 30);
    // No variations were reported: the declared ones at zero and nothing
    // summed, under the wire names agents read.
    assert_eq!(
        health["health"]["variations"],
        json!({"control": 0, "treatment": 0})
    );
    assert_eq!(health["health"]["otherVariations"], 0);
    assert_eq!(health["health"]["topErrors"][0]["reporter"], "client");
    assert!(
        health["health"]["assessment"]
            .as_str()
            .unwrap()
            .contains("Consider rolling back"),
        "{health}"
    );

    let back = tool(
        &s.app,
        &member,
        "rollback_flag",
        json!({"app": "web", "key": "new-checkout", "reason": "TypeErrors in checkout"}),
    )
    .await
    .unwrap();
    assert_eq!(back["restoredFrom"], 1);
    let (_, e) = sdk(
        &s.app,
        "POST",
        "/api/flags/new-checkout/evaluate",
        &client_key,
        Some(json!({"context": {
            "user_id": "u9", "environment": "production", "attributes": {"email": "ada@acme.com"}
        }})),
    )
    .await;
    assert_eq!(e["enabled"], false);

    let history = tool(
        &s.app,
        &reader,
        "flag_history",
        json!({"app": "web", "key": "new-checkout"}),
    )
    .await
    .unwrap();
    assert_eq!(history["versions"][0]["change"], "rollback");
    assert_eq!(
        history["versions"][0]["reason"],
        "rollback to v1: TypeErrors in checkout"
    );

    // Rotating the server key retires the old one immediately.
    let rotated = tool(
        &s.app,
        &admin,
        "rotate_app_keys",
        json!({"app": "web", "which": "server"}),
    )
    .await
    .unwrap();
    assert!(rotated["keys"]["clientKey"].is_null());
    let (status, _) = sdk(&s.app, "GET", "/api/sdk/flags", &server_key, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Each successful tool call was recorded once (the refused ones rolled back
    // with their transaction); the shipper delivers them to the platform.
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 9, "whoami, create_app, create_flag, set_flag_environment, evaluate_flag, flag_health, rollback_flag, flag_history, rotate_app_keys");
    assert_eq!(platform.lock().unwrap().shipped, n);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_unknown_app_lists_the_real_ones_and_other_orgs_are_invisible(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let (acme, globex) = (Uuid::new_v4(), Uuid::new_v4());
    let a = token(
        &platform,
        acme,
        Uuid::new_v4(),
        Role::Owner,
        flags_core::scopes::KNOWN,
    );
    let g = token(
        &platform,
        globex,
        Uuid::new_v4(),
        Role::Owner,
        flags_core::scopes::KNOWN,
    );
    tool(&s.app, &a, "create_app", json!({"name": "web"}))
        .await
        .unwrap();
    tool(
        &s.app,
        &a,
        "create_flag",
        json!({"app": "web", "key": "secret-project"}),
    )
    .await
    .unwrap();

    let err = tool(&s.app, &a, "list_flags", json!({"app": "wbe"}))
        .await
        .unwrap_err();
    assert!(err["message"].as_str().unwrap().contains("web"), "{err}");

    let apps = tool(&s.app, &g, "list_apps", json!({})).await.unwrap();
    assert_eq!(apps["apps"], json!([]));
    let err = tool(
        &s.app,
        &g,
        "get_flag",
        json!({"app": "web", "key": "secret-project"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "app_not_found");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn platform_webhooks_must_be_signed_and_org_deleted_revokes_everything(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let t = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Owner,
        flags_core::scopes::KNOWN,
    );
    let created = tool(&s.app, &t, "create_app", json!({"name": "web"}))
        .await
        .unwrap();
    let key = created["keys"]["serverKey"].as_str().unwrap().to_string();

    let body = savvagent_resource::webhook::body(
        Uuid::new_v4(),
        "org.deleted",
        chrono::Utc::now(),
        &json!({"org_id": org}),
    );
    let deliver = |sig: String| {
        Request::post("/platform/webhooks")
            .header("host", "flags.example.com")
            .header(savvagent_resource::webhook::SIGNATURE_HEADER, sig)
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let forged = savvagent_resource::webhook::sign(
        "otto_whsec_wrong",
        chrono::Utc::now().timestamp(),
        &body,
    );
    let (status, _) = send(&s.app, deliver(forged)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let good =
        savvagent_resource::webhook::sign("otto_whsec_test", chrono::Utc::now().timestamp(), &body);
    let (status, outcome) = send(&s.app, deliver(good)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome["result"], "applied");

    let (status, _) = sdk(&s.app, "GET", "/api/sdk/flags", &key, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The org's tokens are refused at once, without waiting for the
    // platform's introspection cache to expire.
    let (status, body) = send(
        &s.app,
        Request::post("/mcp")
            .header("host", "flags.example.com")
            .header("authorization", format!("Bearer {t}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_apps", "arguments": {}}})
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// ------------------------------------------------------------ auto-rollback

/// How close to the next hour an auto-rollback test may start: more than any
/// of them takes to run.
const HOUR_MARGIN_SECS: f64 = 15.0;

/// A rollback sample starts at the top of the current hour (the database's
/// clock), so telemetry reported just before an hour turns and judged just
/// after it falls out of the window. If the database clock is within
/// [`HOUR_MARGIN_SECS`] of the next hour, wait until that hour has begun.
async fn clear_of_the_hour(db: &Db) {
    let left: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM date_trunc('hour', now()) + interval '1 hour' - now())::float8",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    if left < HOUR_MARGIN_SECS {
        tokio::time::sleep(Duration::from_secs_f64(left + 1.0)).await;
    }
}

/// An owner's token for a fresh org, with an app `web` (production and
/// staging) holding flag `key`, clear of the top of the hour (see
/// [`clear_of_the_hour`]). Returns the token, the user, and the app's server
/// and client keys.
async fn org_with_flag(
    s: &Server,
    platform: &Shared,
    org: Uuid,
    key: &str,
) -> (String, Uuid, String, String) {
    clear_of_the_hour(&s.db).await;
    let user = Uuid::new_v4();
    let t = token(platform, org, user, Role::Owner, flags_core::scopes::KNOWN);
    let created = tool(
        &s.app,
        &t,
        "create_app",
        json!({"name": "web", "environments": ["production", "staging"]}),
    )
    .await
    .unwrap();
    tool(&s.app, &t, "create_flag", json!({"app": "web", "key": key}))
        .await
        .unwrap();
    let keys = &created["keys"];
    (
        t,
        user,
        keys["serverKey"].as_str().unwrap().to_string(),
        keys["clientKey"].as_str().unwrap().to_string(),
    )
}

async fn set_enabled(s: &Server, t: &str, key: &str, environment: &str, on: bool) -> Value {
    tool(
        &s.app,
        t,
        "set_flag_environment",
        json!({"app": "web", "key": key, "environment": environment, "enabled": on,
               "reason": "ship it"}),
    )
    .await
    .unwrap()
}

async fn enable(s: &Server, t: &str, key: &str, environment: &str) -> Value {
    set_enabled(s, t, key, environment, true).await
}

async fn flag(s: &Server, t: &str, key: &str) -> Value {
    tool(&s.app, t, "get_flag", json!({"app": "web", "key": key}))
        .await
        .unwrap()["flag"]
        .clone()
}

fn enabled_in(flag: &Value, environment: &str) -> bool {
    flag["environments"][environment]["enabled"] == true
}

/// `on` and `off` evaluations in production, then `errors` errors with the
/// flag on and no environment (which counts toward production), all through
/// `sdk_key`.
async fn report(s: &Server, sdk_key: &str, key: &str, on: usize, off: usize, errors: usize) {
    let now = chrono::Utc::now().timestamp();
    let evals: Vec<Value> = (0..on + off)
        .map(|i| {
            json!({"flag_key": key, "result": i < on, "user_id": format!("u{i}"),
                   "environment": "production", "timestamp": now})
        })
        .collect();
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/evaluations",
        sdk_key,
        Some(json!({"evaluations": evals})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    let errs: Vec<Value> = (0..errors)
        .map(|_| {
            json!({"flag_key": key, "flag_enabled": true, "error_type": "TypeError",
                   "error_message": "cart is undefined", "timestamp": now})
        })
        .collect();
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/errors",
        sdk_key,
        Some(json!({"errors": errs})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
}

/// NUL is valid in JSON but not in Postgres text; it used to fail the whole
/// batch with an error instead of the receipt (#38).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn telemetry_with_nul_text_is_accepted(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    // The public client key: anyone can send this.
    let (_, _, _, client_key) = org_with_flag(&s, &platform, Uuid::new_v4(), "checkout").await;
    let now = chrono::Utc::now().timestamp();

    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/errors",
        &client_key,
        Some(json!({"errors": [
            {"flag_key": "checkout", "flag_enabled": true, "environment": "prod\u{0}x",
             "error_type": "Type\u{0}Error", "error_message": "bad\u{0}message",
             "stack_trace": "at\u{0}main", "timestamp": now},
            {"flag_key": "checkout\u{0}", "flag_enabled": true, "error_type": "E",
             "error_message": "m", "timestamp": now},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r, json!({"accepted": 1, "unknown_flags": 1}));

    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/evaluations",
        &client_key,
        Some(json!({"evaluations": [
            {"flag_key": "checkout", "result": true, "environment": "stag\u{0}ing",
             "variation": "v\u{0}one", "timestamp": now},
            {"flag_key": "checkout\u{0}", "result": true, "environment": "production",
             "timestamp": now},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r, json!({"accepted": 1, "unknown_flags": 1}));
}

/// NUL is valid in a path but not in Postgres text; it used to fail the lookup
/// with a server error instead of not-found (#45).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn evaluating_a_key_with_nul_is_not_found(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    // The public client key: anyone can send this.
    let (t, _, _, client_key) = org_with_flag(&s, &platform, Uuid::new_v4(), "checkout").await;

    for path in [
        "/api/flags/checkout%00/evaluate",
        "/api/evaluate/checkout%00",
    ] {
        let (status, r) = sdk(&s.app, "POST", path, &client_key, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {r}");
        assert_eq!(r["error"], "flag_not_found", "{path}: {r}");
        assert!(
            !r["error_description"].as_str().unwrap().contains('\0'),
            "{path}: {r}"
        );
    }
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/flags/checkout/evaluate",
        &client_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");

    let err = tool(
        &s.app,
        &t,
        "get_flag",
        json!({"app": "web", "key": "checkout\u{0}"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "flag_not_found", "{err}");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn mcp_arguments_with_nul_are_not_internal_errors(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let (t, _, _, _) = org_with_flag(&s, &platform, Uuid::new_v4(), "checkout").await;

    let err = tool(
        &s.app,
        &t,
        "get_flag",
        json!({"app": "web\u{0}", "key": "checkout"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "app_not_found", "{err}");

    let r = tool(
        &s.app,
        &t,
        "remove_rollback_policy",
        json!({"app": "web", "key": "checkout", "environment": "production\u{0}"}),
    )
    .await
    .unwrap();
    assert_eq!(r["removed"], false, "{r}");
    let err = tool(
        &s.app,
        &t,
        "propose_rollback",
        json!({"app": "web", "key": "checkout", "environment": "production\u{0}"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "invalid_argument", "{err}");
    let r = tool(
        &s.app,
        &t,
        "flag_health",
        json!({"app": "web", "key": "checkout", "environment": "production\u{0}"}),
    )
    .await
    .unwrap();
    // Named as telemetry stores it: no raw NUL in the answer.
    assert_eq!(r["health"]["environment"], "production\u{FFFD}", "{r}");

    for (name, args) in [
        (
            "create_flag",
            json!({"app": "web", "key": "other", "reason": "r\u{0}"}),
        ),
        (
            "update_flag",
            json!({"app": "web", "key": "checkout", "configuration": {"k": "\u{0}"}}),
        ),
        (
            "set_flag_environment",
            json!({"app": "web", "key": "checkout", "environment": "production",
                   "rules": [{"attribute": "plan", "operator": "equals", "values": ["a\u{0}"]}]}),
        ),
    ] {
        let err = tool(&s.app, &t, name, args).await.unwrap_err();
        assert_eq!(err["data"]["code"], "invalid_argument", "{name}: {err}");
        assert!(
            err["message"].as_str().unwrap().contains("NUL"),
            "{name}: {err}"
        );
    }
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn auto_rollback_needs_a_plan_that_includes_it(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    // One org per answer: the platform client caches a success for 60 s.
    let (denied, legacy, down) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    {
        let mut p = platform.lock().unwrap();
        p.legacy.insert(legacy);
        p.unavailable.insert(down);
    }
    let upgrade = format!("{url}/settings/billing");
    let arm = json!({"app": "web", "key": "risky", "environment": "production"});

    let mut denied_owner = None;
    for org in [denied, legacy, down] {
        let (t, user, _, _) = org_with_flag(&s, &platform, org, "risky").await;
        let err = tool(&s.app, &t, "set_rollback_policy", arm.clone())
            .await
            .unwrap_err();
        if org == down {
            assert_eq!(err["data"]["code"], "platform_unavailable", "{err}");
            assert_eq!(err["data"]["retriable"], true, "{err}");
        } else {
            assert_eq!(err["data"]["code"], "feature_not_in_plan", "{err}");
            assert_eq!(err["data"]["retriable"], false, "{err}");
            assert!(err["message"].as_str().unwrap().contains(&upgrade), "{err}");
        }
        let listed = tool(&s.app, &t, "list_rollback_policies", json!({"app": "web"}))
            .await
            .unwrap();
        assert_eq!(listed["policies"], json!([]), "{listed}");
        if org == denied {
            denied_owner = Some((t, user));
        }
    }

    // A policy created before a downgrade can still be removed: removal is
    // never plan-gated. Made directly so the 60 s plan cache is not in the way.
    let (t, user) = denied_owner.unwrap();
    let mut tx = begin_live(&s.db, OrgId::from(denied), None).await.unwrap();
    let app = tx.resolve_app("web").await.unwrap();
    let f = tx.require_flag(&app, "risky").await.unwrap();
    tx.set_policy(
        &app,
        &f,
        "production",
        Mode::Enforce,
        Thresholds::default(),
        UserId::from(user),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let listed = tool(&s.app, &t, "list_rollback_policies", json!({"app": "web"}))
        .await
        .unwrap();
    assert_eq!(listed["policies"].as_array().unwrap().len(), 1, "{listed}");

    let removed = tool(&s.app, &t, "remove_rollback_policy", arm)
        .await
        .unwrap();
    assert_eq!(removed["removed"], true, "{removed}");
    let listed = tool(&s.app, &t, "list_rollback_policies", json!({"app": "web"}))
        .await
        .unwrap();
    assert_eq!(listed["policies"], json!([]), "{listed}");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn set_rollback_policy_warns_when_only_max_error_rate_could_trip(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    let (t, _, _, _) = org_with_flag(&s, &platform, org, "risky").await;

    let without = tool(
        &s.app,
        &t,
        "set_rollback_policy",
        json!({"app": "web", "key": "risky", "environment": "production"}),
    )
    .await
    .unwrap();
    assert!(
        without["warning"]
            .as_str()
            .is_some_and(|w| w.contains("maxErrorRate")),
        "{without}"
    );

    let with = tool(
        &s.app,
        &t,
        "set_rollback_policy",
        json!({"app": "web", "key": "risky", "environment": "production", "maxErrorRate": 0.2}),
    )
    .await
    .unwrap();
    assert!(with["warning"].is_null(), "{with}");
    assert_eq!(with["policy"]["maxErrorRate"], 0.2, "{with}");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_policy_turns_a_failing_flag_off_and_tells_the_stream(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let (s, live) = live_server(pool, &url).await;
    let mut changes = live.listener.subscribe();
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    let (t, _, server_key, client_key) = org_with_flag(&s, &platform, org, "risky").await;

    enable(&s, &t, "risky", "production").await;
    // `Listener::spawn` starts LISTEN in the background: a notification sent
    // before it is listening is lost. Hear one of our own changes first, so
    // the rollback's cannot be lost to that race. Each attempt turns staging
    // on (off again first after a miss), so every attempt is a real change
    // and staging ends on. Each creates a version: the assertions below use
    // versions read back from get_flag, never a count of attempts.
    let mut listening = false;
    for attempt in 0..10 {
        if attempt > 0 {
            set_enabled(&s, &t, "risky", "staging", false).await;
        }
        let staged = enable(&s, &t, "risky", "staging").await;
        let version = staged["flag"]["version"].as_i64().unwrap();
        if heard(&mut changes, "risky", version, Duration::from_millis(500)).await {
            listening = true;
            break;
        }
    }
    assert!(listening, "the listener never heard a flag change");

    let armed = tool(
        &s.app,
        &t,
        "set_rollback_policy",
        json!({"app": "web", "key": "risky", "environment": "production",
               "minEvaluations": 20, "minErrors": 5}),
    )
    .await
    .unwrap();
    assert_eq!(armed["policy"]["state"], "armed", "{armed}");

    report(&s, &server_key, "risky", 20, 20, 6).await;

    let off = eventually(Duration::from_secs(5), || async {
        !enabled_in(&flag(&s, &t, "risky").await, "production")
    })
    .await;
    assert!(off, "the policy did not turn production off");
    let after = flag(&s, &t, "risky").await;
    assert!(enabled_in(&after, "staging"), "{after}");
    let version = after["version"].as_i64().unwrap();
    assert!(
        heard(&mut changes, "risky", version, Duration::from_secs(5)).await,
        "the stream was not told about v{version}"
    );

    let history = tool(
        &s.app,
        &t,
        "flag_history",
        json!({"app": "web", "key": "risky"}),
    )
    .await
    .unwrap();
    let newest = &history["versions"][0];
    assert_eq!(newest["version"], version, "{history}");
    assert_eq!(newest["change"], "auto_rollback", "{history}");
    assert!(newest["actorUserId"].is_null(), "{history}");

    let events = tool(
        &s.app,
        &t,
        "rollback_events",
        json!({"app": "web", "key": "risky"}),
    )
    .await
    .unwrap();
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "rolled_back");
    assert_eq!(events[0]["flagVersion"], version);
    assert_eq!(events[0]["environment"], "production");

    // Client keys are public: their reports cannot trip a default policy.
    tool(
        &s.app,
        &t,
        "create_flag",
        json!({"app": "web", "key": "forged"}),
    )
    .await
    .unwrap();
    enable(&s, &t, "forged", "production").await;
    tool(
        &s.app,
        &t,
        "set_rollback_policy",
        json!({"app": "web", "key": "forged", "environment": "production"}),
    )
    .await
    .unwrap();
    report(&s, &client_key, "forged", 150, 150, 30).await;
    let last_checked = || async {
        let listed = tool(
            &s.app,
            &t,
            "list_rollback_policies",
            json!({"app": "web", "key": "forged"}),
        )
        .await
        .unwrap();
        listed["policies"][0]["lastCheckedAt"].clone()
    };
    // Client-key telemetry does not even wake the checker for a policy that
    // ignores it: 1 s is ~20x the 50 ms debounce.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        last_checked().await.is_null(),
        "client telemetry hinted the checker"
    );
    // Server-key telemetry does; once the checker has judged the flag, with
    // the forged reports in the window, production is still on.
    report(&s, &server_key, "forged", 1, 0, 0).await;
    let judged = eventually(Duration::from_secs(5), || async {
        !last_checked().await.is_null()
    })
    .await;
    assert!(judged, "the checker never judged the forged flag");
    assert!(
        enabled_in(&flag(&s, &t, "forged").await, "production"),
        "client-key reports tripped a default policy"
    );
    // The reports arrived, and would have tripped had they counted.
    let counted = tool(
        &s.app,
        &t,
        "propose_rollback",
        json!({"app": "web", "key": "forged", "environment": "production",
               "includeClientReports": true}),
    )
    .await
    .unwrap();
    assert_eq!(counted["wouldTrip"], true, "{counted}");
    let events = tool(
        &s.app,
        &t,
        "rollback_events",
        json!({"app": "web", "key": "forged"}),
    )
    .await
    .unwrap();
    assert_eq!(events["events"], json!([]), "{events}");

    live.stop().await;
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn propose_rollback_changes_nothing(pool: PgPool) {
    let (url, platform) = start_platform().await;
    // No checker: only the dry run could act on the telemetry.
    let s = server(pool.clone(), &url).await;
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    let (t, _, server_key, _) = org_with_flag(&s, &platform, org, "risky").await;
    enable(&s, &t, "risky", "production").await;
    tool(
        &s.app,
        &t,
        "set_rollback_policy",
        json!({"app": "web", "key": "risky", "environment": "production",
               "minEvaluations": 20, "minErrors": 5}),
    )
    .await
    .unwrap();
    report(&s, &server_key, "risky", 20, 20, 6).await;
    let before = flag(&s, &t, "risky").await;

    let outbox = |tool: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM usage_outbox WHERE $1::text IS NULL OR tool = $1",
            )
            .bind(tool)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let rows = outbox(None).await;

    let proposal = tool(
        &s.app,
        &t,
        "propose_rollback",
        json!({"app": "web", "key": "risky", "environment": "production"}),
    )
    .await
    .unwrap();
    assert_eq!(proposal["wouldTrip"], true, "{proposal}");
    assert_eq!(
        proposal["observation"]["sample"]["errorsOn"], 6,
        "{proposal}"
    );

    assert_eq!(outbox(None).await, rows, "the dry run was metered");
    assert_eq!(outbox(Some("propose_rollback")).await, 0);
    let after = flag(&s, &t, "risky").await;
    assert_eq!(after["version"], before["version"], "{after}");
    assert!(enabled_in(&after, "production"), "{after}");
    let events = tool(
        &s.app,
        &t,
        "rollback_events",
        json!({"app": "web", "key": "risky"}),
    )
    .await
    .unwrap();
    assert_eq!(events["events"], json!([]), "{events}");
}

/// Counting public client-key reports lets anyone holding that key steer the
/// rollback, so opting in takes an owner or admin, checked at the platform
/// now, as creating an app does.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn counting_client_reports_needs_an_owner_or_admin(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    let (_, _, _, _) = org_with_flag(&s, &platform, org, "risky").await;
    let member = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Member,
        flags_core::scopes::KNOWN,
    );
    let admin = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Admin,
        flags_core::scopes::KNOWN,
    );
    let opt_in = json!({"app": "web", "key": "risky", "environment": "production",
                        "includeClientReports": true});

    let err = tool(&s.app, &member, "set_rollback_policy", opt_in.clone())
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "forbidden", "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("includeClientReports"),
        "{err}"
    );
    let listed = tool(
        &s.app,
        &member,
        "list_rollback_policies",
        json!({"app": "web"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["policies"], json!([]), "{listed}");

    // A member may still arm a policy that counts server reports only.
    let plain = tool(
        &s.app,
        &member,
        "set_rollback_policy",
        json!({"app": "web", "key": "risky", "environment": "production"}),
    )
    .await
    .unwrap();
    assert_eq!(plain["policy"]["includeClientReports"], false, "{plain}");
    // Turning it on for an existing policy is refused the same way.
    let err = tool(&s.app, &member, "set_rollback_policy", opt_in.clone())
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "forbidden", "{err}");

    let set = tool(&s.app, &admin, "set_rollback_policy", opt_in)
        .await
        .unwrap();
    assert_eq!(set["policy"]["includeClientReports"], true, "{set}");
}

/// A token without the scope a rollback tool needs is refused with the
/// scope error, before anything else is looked at.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rollback_tools_check_their_scopes(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    org_with_flag(&s, &platform, org, "risky").await;
    let reader = token(&platform, org, Uuid::new_v4(), Role::Owner, &["flags:read"]);
    let writer = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Owner,
        &["flags:write"],
    );
    let target = json!({"app": "web", "key": "risky", "environment": "production"});

    for (t, name, args) in [
        (&reader, "set_rollback_policy", target.clone()),
        (&reader, "remove_rollback_policy", target.clone()),
        (&writer, "list_rollback_policies", json!({"app": "web"})),
        (&writer, "propose_rollback", target.clone()),
        (&writer, "rollback_events", json!({"app": "web"})),
    ] {
        let err = tool(&s.app, t, name, args).await.unwrap_err();
        assert_eq!(err["data"]["code"], "insufficient_scope", "{name}: {err}");
    }
}

/// Both sign-in token prefixes the Savvagent platform mints (`sav_at_`,
/// `sav_pat_`) are accepted. The `otto_` ones no longer are:
/// `savvagent_resource::looks_like_token` refuses them since the platform's
/// 0.18.0 (savvagent/otto-platform#128), which retired every token minted
/// under them.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn sav_and_sav_pat_tokens_are_both_accepted(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    for prefix in ["sav_at_", "sav_pat_"] {
        let t = token_with_prefix(
            &platform,
            prefix,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Role::Member,
            &["flags:read"],
        );
        let who = tool(&s.app, &t, "whoami", json!({}))
            .await
            .unwrap_or_else(|e| panic!("{prefix}: {e}"));
        assert_eq!(who["role"], "member", "{prefix}: {who}");
    }
}

/// Rebrand phase 3 (#194, savvagent/otto-platform#128): configured as
/// `fly.toml` configures production, the service names itself Savvagent Flags
/// at https://flags.savvagent.com/mcp. The legacy otto-flags.savvagent.com host
/// (#135) redirects everything but the SDK API, which still serves SDKs built
/// before the rebrand.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_savvagent_host_is_canonical_and_the_legacy_host_redirects_all_but_the_sdk_api(
    pool: PgPool,
) {
    const NEW: &str = "https://flags.savvagent.com/mcp";
    let (url, platform) = start_platform().await;
    platform.lock().unwrap().aud = Some(NEW.into());
    let mut config = Config::for_test();
    config.platform_url = url;
    config.public_url = "https://flags.savvagent.com".into();
    config.resource_uri = NEW.into();
    config.extra_allowed_hosts = vec!["otto-flags.fly.dev".into()];
    config.redirect_hosts = vec!["otto-flags.savvagent.com".into()];
    let platform_client = platform_client(&config).unwrap();
    let app = router(
        Db::from_pool(pool),
        platform_client,
        Listener::detached(),
        Hints::detached(),
        Arc::new(EvalCounter::default()),
        &config,
    );

    // Protected-resource metadata names the new resource and its name.
    let (status, meta) = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource")
            .header("host", "flags.savvagent.com")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    assert_eq!(meta["resource"], NEW);
    assert_eq!(meta["resource_name"], "Savvagent Flags");

    let admin = token(
        &platform,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Role::Admin,
        &["flags:read", "flags:write", "apps:admin"],
    );
    let mcp = |host: &'static str, body: Value| {
        Request::post("/mcp")
            .header("host", host)
            .header("authorization", format!("Bearer {admin}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // The MCP server introduces itself as savvagent-flags.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "e2e", "version": "0"}}});
    let (status, v) = send(&app, mcp("flags.savvagent.com", init)).await;
    assert!(status.is_success(), "HTTP {status} {v}");
    assert_eq!(v["result"]["serverInfo"]["name"], "savvagent-flags", "{v}");
    assert!(v["result"]["instructions"]
        .as_str()
        .unwrap()
        .starts_with("Savvagent Flags manages feature flags."));

    // whoami answers on the new host and the fly.dev name.
    let whoami = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                        "params": {"name": "whoami", "arguments": {}}});
    for host in ["flags.savvagent.com", "otto-flags.fly.dev"] {
        let (status, v) = send(&app, mcp(host, whoami.clone())).await;
        assert!(status.is_success(), "{host}: HTTP {status} {v}");
        assert_eq!(
            v["result"]["structuredContent"]["role"], "admin",
            "{host}: {v}"
        );
    }
    // The legacy host does not serve /mcp: it redirects, path kept.
    let res = app
        .clone()
        .oneshot(mcp("otto-flags.savvagent.com", whoami.clone()))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(res.headers()["location"], NEW);
    // Any other host is still refused.
    let (status, _) = send(&app, mcp("evil.example.com", whoami.clone())).await;
    assert!(
        !status.is_success(),
        "an unlisted host was served: {status}"
    );

    // An SDK built against the legacy host keeps reading flags there.
    let created = tool_at(
        &app,
        "flags.savvagent.com",
        &admin,
        "create_app",
        json!({"name": "web"}),
    )
    .await;
    let client_key = created["keys"]["clientKey"].as_str().unwrap();
    for host in ["otto-flags.savvagent.com", "flags.savvagent.com"] {
        let (status, v) = send(
            &app,
            Request::get("/api/sdk/flags")
                .header("host", host)
                .header("authorization", format!("Bearer {client_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host}: {v}");
    }
}

/// What the legacy host does (#135), through the whole router: everything
/// outside `/api/` is a bodiless 308 to the public origin with path and query
/// kept, whatever the method; the SDK API is served as on the new host; the new
/// host redirects nothing.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_legacy_host_redirects_everything_but_the_sdk_api(pool: PgPool) {
    const OLD: &str = "otto-flags.savvagent.com";
    let (url, platform) = start_platform().await;
    let mut config = Config::for_test();
    config.platform_url = url;
    config.public_url = "https://flags.savvagent.com".into();
    config.resource_uri = "https://flags.savvagent.com/mcp".into();
    config.redirect_hosts = vec![OLD.into()];
    platform.lock().unwrap().aud = Some(config.resource_uri.clone());
    let platform_client = platform_client(&config).unwrap();
    let app = router(
        Db::from_pool(pool),
        platform_client,
        Listener::detached(),
        Hints::detached(),
        Arc::new(EvalCounter::default()),
        &config,
    );
    let call = |method: &str, host: &str, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host)
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap()
    };

    // Redirected: path and query kept, same method class, no body.
    for (method, uri, to) in [
        ("POST", "/mcp", "https://flags.savvagent.com/mcp"),
        (
            "GET",
            "/mcp?a=1&b=%20x",
            "https://flags.savvagent.com/mcp?a=1&b=%20x",
        ),
        ("GET", "/", "https://flags.savvagent.com/"),
        (
            "GET",
            "/?utm_source=old",
            "https://flags.savvagent.com/?utm_source=old",
        ),
        (
            "GET",
            "/.well-known/oauth-protected-resource",
            "https://flags.savvagent.com/.well-known/oauth-protected-resource",
        ),
        ("GET", "/llms.txt", "https://flags.savvagent.com/llms.txt"),
        (
            "POST",
            "/platform/webhooks",
            "https://flags.savvagent.com/platform/webhooks",
        ),
        ("GET", "/nope", "https://flags.savvagent.com/nope"),
    ] {
        let res = app.clone().oneshot(call(method, OLD, uri)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::PERMANENT_REDIRECT,
            "{method} {uri}"
        );
        assert_eq!(res.headers()["location"], to, "{method} {uri}");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(body.is_empty(), "{method} {uri}");
    }
    // Case and port in the Host header do not dodge it; a hostile
    // X-Forwarded-Host does not steer it.
    for host in ["OTTO-FLAGS.savvagent.com", "otto-flags.savvagent.com:443"] {
        let mut req = call("GET", host, "/mcp");
        req.headers_mut()
            .insert("x-forwarded-host", "evil.example.com".parse().unwrap());
        req.headers_mut()
            .insert("x-forwarded-proto", "http".parse().unwrap());
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT, "{host}");
        assert_eq!(res.headers()["location"], "https://flags.savvagent.com/mcp");
    }

    // Not redirected on the new host.
    let (status, _) = send(&app, call("GET", "flags.savvagent.com", "/mcp?a=1")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, meta) = send(
        &app,
        call(
            "GET",
            "flags.savvagent.com",
            "/.well-known/oauth-protected-resource",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    let (status, _) = send(&app, call("GET", "flags.savvagent.com", "/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The SDK API on the old host: served, not redirected: a POST evaluate with
    // a server key and a flags fetch with a client key are 200s, and a call
    // without a key is the API's own 401.
    let admin = token(
        &platform,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Role::Admin,
        &["flags:read", "flags:write", "apps:admin"],
    );
    let created = tool_at(
        &app,
        "flags.savvagent.com",
        &admin,
        "create_app",
        json!({"name": "web"}),
    )
    .await;
    let client_key = created["keys"]["clientKey"].as_str().unwrap();
    let server_key = created["keys"]["serverKey"].as_str().unwrap();
    tool_at(
        &app,
        "flags.savvagent.com",
        &admin,
        "create_flag",
        json!({"app": "web", "key": "legacy-sdk"}),
    )
    .await;
    for host in [OLD, "flags.savvagent.com"] {
        let (status, v) = send(
            &app,
            Request::post("/api/flags/legacy-sdk/evaluate")
                .header("host", host)
                .header("authorization", format!("Bearer {server_key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"context": {"user_id": "u1", "environment": "production"}}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host}: {v}");
        let (status, _) = send(
            &app,
            Request::get("/api/sdk/flags")
                .header("host", host)
                .header("authorization", format!("Bearer {client_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host}");
        let (status, _) = send(&app, call("GET", host, "/api/sdk/flags")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{host}: a key is required, not a redirect"
        );
    }
}

/// [`tool`] on a given `Host`, for a call that must succeed.
async fn tool_at(app: &Router, host: &str, token: &str, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": args}});
    let (status, v) = send(
        app,
        Request::post("/mcp")
            .header("host", host)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "{name}: HTTP {status} {v}");
    assert!(v["result"]["isError"] != true, "{name}: {v}");
    v["result"]["structuredContent"].clone()
}

/// Connector clients (Claude, ChatGPT) read these hints off `tools/list` to
/// decide what runs without asking (#77): every tool carries them, on the
/// wire, in MCP's camelCase.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn tools_list_carries_annotations(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let t = token(
        &platform,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Role::Member,
        &["flags:read"],
    );
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let (status, v) = send(
        &s.app,
        Request::post("/mcp")
            .header("host", "flags.example.com")
            .header("authorization", format!("Bearer {t}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "HTTP {status} {v}");
    let tools = v["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), flags_mcp::tools::NAMES.len());
    let mut read_only = Vec::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let a = &tool["annotations"];
        match a["readOnlyHint"].as_bool() {
            Some(true) => read_only.push(name),
            Some(false) => {
                assert!(a["destructiveHint"].is_boolean(), "{name}: {a}");
                assert!(a["idempotentHint"].is_boolean(), "{name}: {a}");
            }
            None => panic!("{name} has no readOnlyHint: {tool}"),
        }
    }
    read_only.sort();
    let mut expected = flags_mcp::tools::READ_ONLY.to_vec();
    expected.sort();
    assert_eq!(read_only, expected);
    let hint = |name: &str, key: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"][key].clone()
    };
    assert_eq!(hint("set_flag_environment", "destructiveHint"), true);
    assert_eq!(hint("create_flag", "destructiveHint"), false);
    assert_eq!(hint("remove_rollback_policy", "idempotentHint"), true);
    assert_eq!(hint("sdk_guide", "readOnlyHint"), true);
    // evaluate_flag writes an unbilled usage row (#89), so its description,
    // read next to the read-only hint, must not claim it records nothing.
    let evaluate = tools.iter().find(|t| t["name"] == "evaluate_flag").unwrap()["description"]
        .as_str()
        .unwrap();
    assert!(!evaluate.contains("records nothing"), "{evaluate}");
    assert!(evaluate.contains("not billed"), "{evaluate}");
}

/// `sdk_guide` renders a language's guide, filled in with a named app's id and
/// public client key and never its server key; it checks `flags:read` first,
/// rejects an unknown language before recording anything, and records one free
/// usage row per successful call.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn sdk_guide_renders_a_guide_and_personalizes_it_with_the_apps_public_key(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let org = Uuid::new_v4();
    let (_, _, server_key, client_key) = org_with_flag(&s, &platform, org, "k").await;
    let reader = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Member,
        &["flags:read"],
    );

    // No app named, and the organization has exactly one: it is used, and the result says so.
    // Nothing names a server key.
    let plain = tool(&s.app, &reader, "sdk_guide", json!({"language": "python"}))
        .await
        .unwrap();
    assert_eq!(plain["language"], "python", "{plain}");
    let text = plain["guide"].as_str().unwrap();
    assert!(text.starts_with("# Integrate end to end"), "{text}");
    assert!(text.contains("otto_flags"), "{text}");
    assert!(text.contains(flags_mcp::guide::SDK_REF), "{text}");
    assert!(!text.contains("<commit>"), "{text}");
    assert!(!plain.to_string().contains("srv_"), "{plain}");
    assert_eq!(plain["app"]["name"], "web", "{plain}");
    assert_eq!(plain["appDefaulted"], true, "{plain}");
    assert_eq!(plain["chooseFrom"], json!([]), "{plain}");
    assert!(text.contains("`production`, `staging`"), "{text}");
    let supported: Vec<&str> = plain["supported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(supported.contains(&"python"), "{supported:?}");

    // A named app (through an alias): its id and public key, never the server key.
    let apps = tool(&s.app, &reader, "list_apps", json!({})).await.unwrap();
    let app_id = apps["apps"][0]["id"].as_str().unwrap().to_string();
    let named = tool(
        &s.app,
        &reader,
        "sdk_guide",
        json!({"language": "py", "app": "web"}),
    )
    .await
    .unwrap();
    assert_eq!(named["language"], "python", "{named}");
    assert_eq!(named["app"]["name"], "web", "{named}");
    assert_eq!(named["appDefaulted"], false, "{named}");
    assert_eq!(named["app"]["id"], app_id.as_str(), "{named}");
    assert_eq!(named["app"]["clientKey"], client_key.as_str(), "{named}");
    let text = named["guide"].as_str().unwrap();
    assert!(text.contains(&app_id), "{text}");
    let whole = named.to_string();
    assert!(!whole.contains("srv_"), "{whole}");
    assert!(!whole.contains(&server_key), "{whole}");

    // An unknown language lists what is supported and echoes at most 64 chars.
    let long = format!("COBOL{}", "x".repeat(200));
    let err = tool(&s.app, &reader, "sdk_guide", json!({"language": long}))
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "invalid_argument", "{err}");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("python"), "{message}");
    assert!(message.contains("COBOL"), "{message}");
    assert!(!message.contains(&"x".repeat(60)), "{message}");

    // An unknown app.
    let err = tool(
        &s.app,
        &reader,
        "sdk_guide",
        json!({"language": "python", "app": "nope"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "app_not_found", "{err}");

    // flags:read is required.
    let writer = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Owner,
        &["flags:write"],
    );
    let err = tool(&s.app, &writer, "sdk_guide", json!({"language": "python"}))
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "insufficient_scope", "{err}");

    // One free row per successful call; the rejected language and the
    // refused scope recorded nothing. The unknown-app call's transaction rolled
    // back with its row.
    let rows: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT tool, billable, count(*) FROM usage_outbox \
         WHERE org_id = $1 AND tool = 'sdk_guide' GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![("sdk_guide".to_string(), false, 2)]);
}

/// With no `app`, `sdk_guide` uses the organization's only app, lists its apps when it has
/// several so the agent can choose and call again, and leaves placeholders when it has none.
/// Every result starts with the integration recipe, and the app's environments fill the guide.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn sdk_guide_without_an_app_defaults_to_the_only_app_or_lists_the_choices(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;

    // No apps: the placeholders stay and nothing is offered.
    let empty_org = Uuid::new_v4();
    let owner = token(
        &platform,
        empty_org,
        Uuid::new_v4(),
        Role::Owner,
        flags_core::scopes::KNOWN,
    );
    let none = tool(&s.app, &owner, "sdk_guide", json!({"language": "python"}))
        .await
        .unwrap();
    assert!(none["app"].is_null(), "{none}");
    assert_eq!(none["appDefaulted"], false, "{none}");
    assert_eq!(none["chooseFrom"], json!([]), "{none}");
    let text = none["guide"].as_str().unwrap();
    assert!(text.starts_with("# Integrate end to end"), "{text}");
    assert!(
        text.contains("<app-id>") && text.contains("<environments>"),
        "{text}"
    );
    assert!(text.contains("Hand off the server key"), "{text}");

    // Several apps: none is guessed, and each is offered by name and id, in name order.
    let web = tool(&s.app, &owner, "create_app", json!({"name": "web"}))
        .await
        .unwrap();
    let api = tool(
        &s.app,
        &owner,
        "create_app",
        json!({"name": "api", "environments": ["production", "staging", "dev"]}),
    )
    .await
    .unwrap();
    let many = tool(&s.app, &owner, "sdk_guide", json!({"language": "react"}))
        .await
        .unwrap();
    assert!(many["app"].is_null(), "{many}");
    assert_eq!(many["appDefaulted"], false, "{many}");
    let offered = many["chooseFrom"].as_array().unwrap();
    assert_eq!(offered.len(), 2, "{many}");
    assert_eq!(offered[0]["name"], "api", "{many}");
    assert_eq!(offered[0]["id"], api["app"]["id"], "{many}");
    assert_eq!(offered[1]["name"], "web", "{many}");
    assert_eq!(offered[1]["id"], web["app"]["id"], "{many}");
    let text = many["guide"].as_str().unwrap();
    assert!(text.contains("<environments>"), "{text}");
    assert!(text.contains("needs no handoff"), "{text}");
    assert!(!many.to_string().contains("srv_"), "{many}");

    // Choosing one fills the guide with its own environments.
    let chosen = tool(
        &s.app,
        &owner,
        "sdk_guide",
        json!({"language": "react", "app": "api"}),
    )
    .await
    .unwrap();
    assert_eq!(chosen["app"]["name"], "api", "{chosen}");
    assert_eq!(chosen["chooseFrom"], json!([]), "{chosen}");
    let text = chosen["guide"].as_str().unwrap();
    assert!(text.contains("`production`, `staging`, `dev`"), "{text}");
    assert!(!text.contains("<environments>"), "{text}");
}

/// Every tool is free: setting a policy records a row with `billable = false`
/// like the rest, except the dry run, which records none.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn setting_a_policy_is_recorded_as_not_billable(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let org = Uuid::new_v4();
    platform.lock().unwrap().entitled.insert(org);
    let (t, _, _, _) = org_with_flag(&s, &platform, org, "risky").await;
    let target = json!({"app": "web", "key": "risky", "environment": "production"});

    // Refused for bad input, inside the transaction that would have billed.
    for bad in [
        json!({"app": "web", "key": "risky", "environment": "production", "windowHours": 99}),
        json!({"app": "web", "key": "risky", "environment": "qa"}),
    ] {
        let err = tool(&s.app, &t, "set_rollback_policy", bad)
            .await
            .unwrap_err();
        assert_eq!(err["data"]["code"], "invalid_argument", "{err}");
    }

    tool(&s.app, &t, "set_rollback_policy", target.clone())
        .await
        .unwrap();
    tool(&s.app, &t, "list_rollback_policies", json!({"app": "web"}))
        .await
        .unwrap();
    tool(&s.app, &t, "propose_rollback", target.clone())
        .await
        .unwrap();
    tool(&s.app, &t, "rollback_events", json!({"app": "web"}))
        .await
        .unwrap();
    tool(&s.app, &t, "remove_rollback_policy", target)
        .await
        .unwrap();

    let rows: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT tool, billable, count(*) FROM usage_outbox \
         WHERE org_id = $1 AND tool LIKE '%rollback%' GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("list_rollback_policies".to_string(), false, 1),
            ("remove_rollback_policy".to_string(), false, 1),
            ("rollback_events".to_string(), false, 1),
            ("set_rollback_policy".to_string(), false, 1),
        ]
    );
}

/// `whoami` and `usage` report the platform's meters, budget and blocked reason,
/// and every write tool records a free row. A platform that predates meters
/// still answers: the new fields are empty, the legacy ones are filled.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn whoami_and_usage_report_meters_and_budget(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let (org, legacy_org) = (Uuid::new_v4(), Uuid::new_v4());
    {
        let mut p = platform.lock().unwrap();
        p.meters.insert(
            org,
            json!([{"meter": "otto-flags/evaluations", "display_name": "Evaluations",
                    "unit": "evaluation", "quantity": 1200}]),
        );
        p.budget.insert(
            org,
            json!({"monthly_budget_cents": 5000, "spend_cents": 5000, "percent_used": 100,
                   "thresholds_reached": [50, 80, 100], "hard_stop": true, "blocked": true,
                   "reason": "budget_exceeded"}),
        );
        p.blocked_reason.insert(org, "budget_exceeded".into());
    }
    let (t, _, _, _) = org_with_flag(&s, &platform, org, "risky").await;

    // A write tool is recorded, free, even while the platform reports blocked.
    let billable: Vec<bool> =
        sqlx::query_scalar("SELECT billable FROM usage_outbox WHERE tool = 'create_flag'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(billable, vec![false]);

    let who = tool(&s.app, &t, "whoami", json!({})).await.unwrap();
    let usage = tool(&s.app, &t, "usage", json!({})).await.unwrap();
    for status in [&who["usage"], &usage["usage"]] {
        assert_eq!(status["meters"][0]["meter"], "otto-flags/evaluations");
        assert_eq!(status["meters"][0]["displayName"], "Evaluations");
        assert_eq!(status["meters"][0]["quantity"], 1200);
        assert_eq!(status["budget"]["monthlyBudgetCents"], 5000);
        assert_eq!(status["budget"]["reason"], "budget_exceeded");
        assert_eq!(status["blockedReason"], "budget_exceeded");
        assert_eq!(status["enforced"], true);
    }

    // The legacy shape: no meters, no budget, nothing blocked.
    let (lt, _, _, _) = {
        platform.lock().unwrap().legacy.insert(legacy_org);
        org_with_flag(&s, &platform, legacy_org, "risky").await
    };
    let usage = tool(&s.app, &lt, "usage", json!({})).await.unwrap();
    assert_eq!(usage["usage"]["meters"], json!([]));
    assert_eq!(usage["usage"]["budget"], Value::Null);
    assert_eq!(usage["usage"]["blockedReason"], Value::Null);
    assert_eq!(usage["usage"]["includedOps"], 1000);
}

/// A failed plan lookup is remembered for a few seconds, so an outage costs
/// one platform request per org, not one per check.
#[tokio::test]
async fn a_failed_entitlement_lookup_is_not_repeated_at_once() {
    use flags_core::entitlements::{Entitlement, EntitlementSource, Entitlements, AUTO_ROLLBACK};
    let (url, platform) = start_platform().await;
    let mut config = Config::for_test();
    config.platform_url = url.clone();
    let client = platform_client(&config).unwrap();
    let org = Uuid::new_v4();
    platform.lock().unwrap().unavailable.insert(org);
    let entitlements = Entitlements::new(client, config.upgrade_url());
    let hits = || {
        platform
            .lock()
            .unwrap()
            .usage_status_hits
            .get(&org)
            .copied()
            .unwrap_or(0)
    };

    for _ in 0..2 {
        assert_eq!(
            entitlements.check(OrgId::from(org), AUTO_ROLLBACK).await,
            Entitlement::Unknown
        );
    }
    assert_eq!(
        hits(),
        1,
        "the second check within 10 s asked the platform again"
    );
}

/// The platform's rejection reason is its text: stored capped, and a NUL in it
/// cannot fail the batch and leave the shipper retrying it forever (#51).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_rejected_usage_event_keeps_a_capped_reason(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    // 600 characters, multi-byte, with a NUL.
    platform.lock().unwrap().reject_with = Some(format!("\0{}", "é".repeat(599)));

    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 1);
    let reasons: Vec<String> =
        sqlx::query_scalar("SELECT reason FROM usage_outbox_rejected WHERE event_id = $1")
            .bind(event_id)
            .fetch_all(s.db.pool())
            .await
            .unwrap();
    assert_eq!(reasons, vec![format!("\u{FFFD}{}", "é".repeat(499))]);
    assert_eq!(reasons[0].chars().count(), 500);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(s.db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A metered outbox row the way the hour close writes it.
async fn insert_metered(pool: &PgPool, org: Uuid, quantity: i64) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable, meter, quantity) \
         VALUES ($1, NULL, 'sdk_evaluations', true, 'otto-flags/evaluations', $2) \
         RETURNING event_id",
    )
    .bind(org)
    .bind(quantity)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn ship_once_sends_the_meter_and_quantity_of_a_metered_row(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = insert_metered(s.db.pool(), Uuid::new_v4(), 1234).await;

    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 1);
    let p = platform.lock().unwrap();
    let e = p.events.iter().find(|e| e.event_id == event_id).unwrap();
    assert_eq!(e.meter.as_deref(), Some("otto-flags/evaluations"));
    assert_eq!(e.quantity, Some(1234));
    assert_eq!(e.service, None);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_rejected_metered_row_keeps_its_meter_and_quantity(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = insert_metered(s.db.pool(), Uuid::new_v4(), 55).await;
    platform.lock().unwrap().reject_with = Some("unknown meter".into());

    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 1);
    let (meter, quantity): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT meter, quantity FROM usage_outbox_rejected WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(s.db.pool())
            .await
            .unwrap();
    assert_eq!(meter.as_deref(), Some("otto-flags/evaluations"));
    assert_eq!(quantity, Some(55));
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
}

fn quick_cfg() -> flags_core::usage::ShipperConfig {
    flags_core::usage::ShipperConfig {
        base_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(40),
        ..Default::default()
    }
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_unknown_meter_rejection_redeclares_and_retries_instead_of_dead_lettering(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = insert_metered(s.db.pool(), Uuid::new_v4(), 77).await;
    // The platform has forgotten the meter (or never saw it in this process).
    platform.lock().unwrap().enforce_meters = true;

    let n = flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    assert_eq!(n, 1);
    // Re-declared, and the row kept for a retry rather than dead-lettered.
    assert_eq!(
        platform.lock().unwrap().declared,
        vec!["otto-flags/evaluations".to_string()]
    );
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);

    tokio::time::sleep(Duration::from_millis(100)).await;
    flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);
    let p = platform.lock().unwrap();
    assert_eq!(p.shipped, 1);
    let sent = p.events.iter().filter(|e| e.event_id == event_id).count();
    assert_eq!(sent, 2, "sent once refused, once accepted");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_failed_redeclaration_still_keeps_the_rows(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    insert_metered(s.db.pool(), Uuid::new_v4(), 5).await;
    {
        let mut p = platform.lock().unwrap();
        p.enforce_meters = true;
        p.meter_modes = [MeterMode::ServiceNotSet].into();
    }

    flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);

    // The next unknown_meter answer re-declares again and succeeds.
    tokio::time::sleep(Duration::from_millis(100)).await;
    flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
    assert_eq!(platform.lock().unwrap().shipped, 1);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn other_rejections_in_a_mixed_batch_still_dead_letter(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let metered = insert_metered(s.db.pool(), Uuid::new_v4(), 3).await;
    let unmetered: Uuid = sqlx::query_scalar(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', false) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 0,
        duplicates: 0,
        rejected: vec![
            RejectedEvent {
                event_id: metered,
                reason: "unknown_meter: otto-flags/evaluations".into(),
            },
            RejectedEvent {
                event_id: unmetered,
                reason: "invalid_quantity: nope".into(),
            },
        ],
    });

    flags_core::usage::ship_once(&s.db, &s.platform, &quick_cfg())
        .await
        .unwrap();
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, vec![unmetered]);
    let kept: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(kept, vec![metered]);
}

async fn get_json(app: &Router, path: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_stalled_meter_declaration_shows_on_statusz_but_not_readyz(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    insert_metered(s.db.pool(), Uuid::new_v4(), 10).await;
    let health = flags_core::usage::DeclarationHealth::default();
    let mut config = Config::for_test();
    config.platform_url = url.clone();
    let app = flags_server::router_with_meter_health(
        s.db.clone(),
        s.platform.clone(),
        Listener::detached(),
        Hints::detached(),
        s.evaluations.clone(),
        &config,
        health.clone(),
    );

    // Before the shipper has tried: declaring, nothing failing.
    let (status, body) = get_json(&app, "/statusz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["metering"]["declaration"], "declaring", "{body}");

    platform.lock().unwrap().meter_modes =
        std::iter::repeat_n(MeterMode::ServiceNotSet, 3).collect();
    let cfg = flags_core::usage::ShipperConfig {
        poll: Duration::from_millis(20),
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(50),
        ..Default::default()
    };
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(flags_core::usage::run_reporting(
        s.db.clone(),
        s.platform.clone(),
        cfg,
        shutdown,
        health,
    ));

    // While it keeps failing: stalled, and the service is still ready.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, body) = get_json(&app, "/statusz").await;
            if body["metering"]["declaration"] == "stalled" {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map(|body| {
        // Only the coarse state: no queue depth, duration, reason or status.
        assert_eq!(body, json!({"metering": {"declaration": "stalled"}}));
    })
    .expect("statusz reports the stall");
    let (status, body) = get_json(&app, "/readyz").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"status": "ready"}));

    // Once a declaration succeeds the signal clears.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, body) = get_json(&app, "/statusz").await;
            if body["metering"]["declaration"] == "declared" {
                assert_eq!(body, json!({"metering": {"declaration": "declared"}}));
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("statusz reports the declaration");
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Runs `usage::run` against the mock until `done` holds, then stops it.
async fn run_shipper_until(
    s: &Server,
    platform: &Shared,
    done: impl Fn(&Platform) -> bool,
) -> Vec<&'static str> {
    let cfg = flags_core::usage::ShipperConfig {
        poll: Duration::from_millis(20),
        base_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(40),
        ..Default::default()
    };
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(flags_core::usage::run(
        s.db.clone(),
        s.platform.clone(),
        cfg,
        shutdown,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done(&platform.lock().unwrap()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the shipper reaches the expected state");
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the shipper stops on shutdown")
        .unwrap();
    let calls = platform.lock().unwrap().calls.clone();
    calls
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_shipper_declares_the_evaluations_meter_before_its_first_shipment(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    insert_metered(s.db.pool(), Uuid::new_v4(), 10).await;

    let calls = run_shipper_until(&s, &platform, |p| p.shipped >= 1).await;
    assert_eq!(calls.first(), Some(&"meters"), "{calls:?}");
    assert!(calls.contains(&"usage"), "{calls:?}");
    assert_eq!(
        platform.lock().unwrap().declared,
        vec!["otto-flags/evaluations".to_string()]
    );
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_failed_declaration_holds_shipping_until_it_succeeds(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    insert_metered(s.db.pool(), Uuid::new_v4(), 10).await;
    platform.lock().unwrap().meter_modes = [MeterMode::ServiceNotSet, MeterMode::Rejected].into();

    let calls = run_shipper_until(&s, &platform, |p| p.shipped >= 1).await;
    // Two refused declarations and a third that succeeded, all before the
    // first shipment; the row stayed queued meanwhile.
    let first_usage = calls.iter().position(|c| *c == "usage").unwrap();
    assert_eq!(
        &calls[..first_usage],
        ["meters", "meters", "meters"],
        "{calls:?}"
    );
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn shutdown_while_declaring_ships_nothing(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    insert_metered(s.db.pool(), Uuid::new_v4(), 10).await;
    platform.lock().unwrap().meter_modes =
        std::iter::repeat_n(MeterMode::ServiceNotSet, 1000).collect();

    let calls = run_shipper_until(&s, &platform, |p| p.calls.len() >= 2).await;
    assert!(!calls.contains(&"usage"), "{calls:?}");
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 1);
}

/// The shipper on the default test pool, whose superuser role RLS does not
/// apply to. Its maintenance transactions run there as `otto_app` (#73), so
/// this guards that the outbox's marker policies and grants still let it
/// claim, delete, and record rejections for every org. It would pass before
/// #73 too: it guards, it does not reproduce.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_shipper_reaches_every_org_on_the_superuser_pool(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let insert = |org: Uuid| {
        let pool = s.db.pool().clone();
        async move {
            sqlx::query(
                "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
                 VALUES ($1, NULL, 'create_flag', true)",
            )
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let count = |table: &'static str| {
        let pool = s.db.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(DISTINCT org_id) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

    insert(a).await;
    insert(b).await;
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(platform.lock().unwrap().shipped, 2);
    assert_eq!(count("usage_outbox").await, 0);

    platform.lock().unwrap().reject_with = Some("unknown org".into());
    insert(a).await;
    insert(b).await;
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(count("usage_outbox").await, 0);
    assert_eq!(count("usage_outbox_rejected").await, 2);
}

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn make_outbox_due(pool: &PgPool) {
    sqlx::query("UPDATE usage_outbox SET next_attempt_at = now(), last_error = NULL")
        .execute(pool)
        .await
        .unwrap();
}

/// The platform's receipt can only move rows of the batch it answers, each
/// once: one naming an event outside the batch, or repeating one, moves and
/// deletes nothing (#63).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_receipt_naming_events_outside_the_batch_moves_nothing(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let a = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    let b = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable, next_attempt_at) \
         VALUES ($1, NULL, 'create_flag', true, now() + interval '1 hour') RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    let rejected = |id: Uuid| RejectedEvent {
        event_id: id,
        reason: "x".into(),
    };

    // Outside the batch: B was not shipped.
    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 0,
        duplicates: 0,
        rejected: vec![rejected(b)],
    });
    let err = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap_err();
    assert_eq!(err.failure_reason().reason, "decode");
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 2);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);
    let (a_err,): (Option<String>,) =
        sqlx::query_as("SELECT last_error FROM usage_outbox WHERE event_id = $1")
            .bind(a)
            .fetch_one(s.db.pool())
            .await
            .unwrap();
    assert_eq!(a_err.as_deref(), Some("decode"));
    let (b_err, b_attempts): (Option<String>, i32) =
        sqlx::query_as("SELECT last_error, attempts FROM usage_outbox WHERE event_id = $1")
            .bind(b)
            .fetch_one(s.db.pool())
            .await
            .unwrap();
    assert_eq!(b_err, None);
    assert_eq!(b_attempts, 0);

    // Repeated: both rows are due, so the batch is A and B and the count (2)
    // still matches; only the id check can refuse it.
    make_outbox_due(s.db.pool()).await;
    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 0,
        duplicates: 0,
        rejected: vec![rejected(a), rejected(a)],
    });
    let err = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap_err();
    assert_eq!(err.failure_reason().reason, "decode");
    let (a_err,): (Option<String>,) =
        sqlx::query_as("SELECT last_error FROM usage_outbox WHERE event_id = $1")
            .bind(a)
            .fetch_one(s.db.pool())
            .await
            .unwrap();
    assert_eq!(a_err.as_deref(), Some("decode"));
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 2);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);

    // Control: a well-formed receipt still moves exactly the rejected row.
    make_outbox_due(s.db.pool()).await;
    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 1,
        duplicates: 0,
        rejected: vec![rejected(a)],
    });
    let n = flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT event_id, reason FROM usage_outbox_rejected")
            .fetch_all(s.db.pool())
            .await
            .unwrap();
    assert_eq!(rows, vec![(a, "x".to_string())]);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
}

/// The release for the next shipper to reach the gated mock platform. Bounded,
/// so a shipper that never gets there fails the test instead of hanging it.
async fn reached(
    held: &mut tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
) -> tokio::sync::oneshot::Sender<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), held.recv())
        .await
        .expect("shipper reached the platform")
        .expect("gate open")
}

/// A shipper whose claim lapsed while it waited on the platform cannot act on
/// the row once another shipper has claimed it: the newer claim records it,
/// once (#67).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lapsed_usage_claim_leaves_the_row_to_the_newer_one(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    let (gate, mut held) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut p = platform.lock().unwrap();
        p.reject_with = Some("unknown org".into());
        p.usage_gate = Some(gate);
    }
    // A lease of 0: a claimed row is due again at once, as if the claim lapsed.
    let cfg = flags_core::usage::ShipperConfig {
        claim_lease: std::time::Duration::ZERO,
        ..Default::default()
    };
    let shipper = |cfg: flags_core::usage::ShipperConfig| {
        let (db, platform) = (s.db.clone(), s.platform.clone());
        tokio::spawn(async move { flags_core::usage::ship_once(&db, &platform, &cfg).await })
    };

    let first = shipper(cfg.clone());
    let release_first = reached(&mut held).await;
    let second = shipper(cfg.clone());
    let release_second = reached(&mut held).await;

    // The first shipper's claim has lapsed: its rejection is not recorded and
    // the row it no longer holds stays for the second.
    release_first.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);

    release_second.send(()).unwrap();
    assert_eq!(second.await.unwrap().unwrap(), 1);
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, vec![event_id]);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
}

/// The shipper reads and writes the outbox in maintenance transactions, so it
/// still works when its pool is subject to row-level security (#68). The test
/// pool is a superuser, which RLS ignores, so this one runs as otto_app.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_usage_shipper_sets_the_maintenance_marker(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let as_app = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE otto_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let insert = || {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
             VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
        )
        .bind(Uuid::new_v4())
        .fetch_one(&pool)
    };
    let _accepted = insert().await.unwrap();
    let refused = insert().await.unwrap();

    // The role is subject to RLS: unpinned and unmarked, it sees nothing.
    let unmarked: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(&as_app)
        .await
        .unwrap();
    assert_eq!(unmarked, 0);

    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 1,
        duplicates: 0,
        rejected: vec![RejectedEvent {
            event_id: refused,
            reason: "x".into(),
        }],
    });
    let db = savvagent_tenant::Db::from_pool(as_app);
    let n = flags_core::usage::ship_once(&db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(row_count(&pool, "usage_outbox").await, 0);
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rejected, vec![refused]);
    db.pool().close().await;
}

/// A shipper whose claim lapsed cannot reschedule the row once another shipper
/// has claimed it: its failure leaves the newer claim's schedule and error as
/// they are (#67).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lapsed_claim_does_not_reschedule_the_newer_one(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    let (gate, mut held) = tokio::sync::mpsc::unbounded_channel();
    platform.lock().unwrap().usage_gate = Some(gate);
    // A lease of 0: a claimed row is due again at once, as if the claim lapsed.
    let cfg = flags_core::usage::ShipperConfig {
        claim_lease: std::time::Duration::ZERO,
        ..Default::default()
    };
    let shipper = |cfg: flags_core::usage::ShipperConfig| {
        let (db, platform) = (s.db.clone(), s.platform.clone());
        tokio::spawn(async move { flags_core::usage::ship_once(&db, &platform, &cfg).await })
    };
    let schedule = || {
        sqlx::query_as::<_, (i32, Option<String>, chrono::DateTime<chrono::Utc>)>(
            "SELECT attempts, last_error, next_attempt_at FROM usage_outbox WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_one(s.db.pool())
    };

    let first = shipper(cfg.clone());
    let release_first = reached(&mut held).await;
    let second = shipper(cfg.clone());
    let release_second = reached(&mut held).await;
    let (attempts, _, claimed_until) = schedule().await.unwrap();
    assert_eq!(attempts, 2);

    // The first shipper's call fails after its claim lapsed: it must not
    // reschedule the row the second now holds.
    platform.lock().unwrap().fail_usage = true;
    release_first.send(()).unwrap();
    let err = first.await.unwrap().unwrap_err();
    assert_eq!(err.failure_reason().reason, "http_status");
    let (attempts, last_error, next_attempt_at) = schedule().await.unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(last_error, None);
    assert_eq!(next_attempt_at, claimed_until);

    platform.lock().unwrap().fail_usage = false;
    release_second.send(()).unwrap();
    assert_eq!(second.await.unwrap().unwrap(), 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);
}

/// The fence pairs each claimed row with the attempts value the claim set for
/// that row: a lapsed claim still finishes the rows it holds and leaves the one
/// a newer claim took (#67).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lapsed_claim_is_fenced_row_by_row(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let insert = |attempts: i32| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO usage_outbox (org_id, user_id, tool, billable, attempts) \
             VALUES ($1, NULL, 'create_flag', true, $2) RETURNING event_id",
        )
        .bind(Uuid::new_v4())
        .bind(attempts)
        .fetch_one(s.db.pool())
    };
    // The claim sets 6 on the first row and 1 on the second: a fence that
    // paired ids and attempts other than row by row would miss both.
    let r1 = insert(5).await.unwrap();
    let r2 = insert(0).await.unwrap();
    let (gate, mut held) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut p = platform.lock().unwrap();
        p.reject_with = Some("unknown org".into());
        p.usage_gate = Some(gate);
    }

    let first = {
        let (db, platform) = (s.db.clone(), s.platform.clone());
        tokio::spawn(async move {
            flags_core::usage::ship_once(&db, &platform, &Default::default()).await
        })
    };
    let release_first = reached(&mut held).await;
    // A newer claim of the second row only.
    sqlx::query("UPDATE usage_outbox SET attempts = attempts + 1 WHERE event_id = $1")
        .bind(r2)
        .execute(s.db.pool())
        .await
        .unwrap();

    release_first.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), 2);
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![r2]);
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, vec![r1]);
}

// ------------------------------------------------- metering production evaluations

/// Move every counted hour into the past, past the close grace, so the next
/// `close_due` closes it. The only way a test can age an hour.
async fn age_hours(pool: &PgPool) {
    sqlx::query("UPDATE usage_meter_hourly SET hour = hour - interval '3 hours'")
        .execute(pool)
        .await
        .unwrap();
}

/// Flush the counter, age the hours, close them against the mock platform
/// through the production lookup, and ship.
async fn flush_close_ship(s: &Server) {
    metering::flush(&s.db, &s.evaluations).await.unwrap();
    age_hours(s.db.pool()).await;
    let meter = flags_core::usage::Meter::new(s.platform.clone());
    metering::close_due(&s.db, |org| {
        let meter = meter.clone();
        async move { meter.billable_blocked(org).await }
    })
    .await
    .unwrap();
    flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();
}

async fn evaluate_n(s: &Server, key: &str, flag: &str, n: usize) {
    for _ in 0..n {
        let (status, r) = sdk(
            &s.app,
            "POST",
            &format!("/api/evaluate/{flag}"),
            key,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{r}");
    }
}

async fn report_n(s: &Server, key: &str, flag: &str, n: usize) -> Value {
    let now = chrono::Utc::now().timestamp();
    let evals: Vec<Value> = (0..n)
        .map(|i| {
            json!({"flag_key": flag, "result": true, "user_id": format!("u{i}"),
                        "environment": "production", "timestamp": now})
        })
        .collect();
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/evaluations",
        key,
        Some(json!({"evaluations": evals})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    r
}

fn metered(p: &Shared, org: Uuid) -> Vec<UsageEvent> {
    p.lock()
        .unwrap()
        .events
        .iter()
        .filter(|e| e.org_id == org && e.tool == metering::EVALUATIONS_TOOL)
        .cloned()
        .collect()
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_larger_of_served_and_reported_is_metered(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let (org_a, org_b) = (Uuid::new_v4(), Uuid::new_v4());
    let (_, _, a_key, _) = org_with_flag(&s, &platform, org_a, "checkout").await;
    let (_, _, b_key, _) = org_with_flag(&s, &platform, org_b, "checkout").await;

    // 3 served, 5 reported: 5. 4 served, nothing reported: 4.
    evaluate_n(&s, &a_key, "checkout", 3).await;
    let receipt = report_n(&s, &a_key, "checkout", 5).await;
    assert_eq!(receipt["accepted"], 5);
    evaluate_n(&s, &b_key, "checkout", 4).await;

    flush_close_ship(&s).await;

    let a = metered(&platform, org_a);
    assert_eq!(a.len(), 1, "one metered event per app");
    assert_eq!(a[0].meter.as_deref(), Some(metering::METER_EVALUATIONS));
    assert_eq!(a[0].quantity, Some(5));
    let b = metered(&platform, org_b);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].quantity, Some(4));
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn client_key_telemetry_is_never_billed(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (_, _, _, client_key) = org_with_flag(&s, &platform, org, "checkout").await;

    // The client key is public: its reports are recorded for health but
    // never raise the bill. Its 2 served evaluations are billed; its 50
    // reported ones are not.
    evaluate_n(&s, &client_key, "checkout", 2).await;
    let receipt = report_n(&s, &client_key, "checkout", 50).await;
    assert_eq!(receipt["accepted"], 50);
    flush_close_ship(&s).await;

    let events = metered(&platform, org);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].quantity, Some(2));
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn only_the_apps_own_environments_are_counted(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (_, _, key, client_key) = org_with_flag(&s, &platform, org, "checkout").await;

    // The caller names the environment, and a public client key can name any.
    for k in [&key, &client_key] {
        let (status, r) = sdk(
            &s.app,
            "POST",
            "/api/evaluate/checkout",
            k,
            Some(json!({"context": {"environment": "made-up"}})),
        )
        .await;
        // Answered as ever, just not counted.
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["key"], "checkout");
    }
    assert!(s.evaluations.take().is_empty());

    evaluate_n(&s, &client_key, "checkout", 2).await;
    evaluate_n(&s, &key, "checkout", 1).await;
    let counts = s.evaluations.take();
    let c = counts.values().next().expect("production is counted");
    assert_eq!((c.served, c.served_client), (3, 2));
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn client_answers_above_the_ceiling_are_served_not_billed_and_usage_shows_it(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (t, _, key, client_key) = org_with_flag(&s, &platform, org, "checkout").await;

    evaluate_n(&s, &client_key, "checkout", 5).await;
    evaluate_n(&s, &key, "checkout", 2).await;
    metering::flush(&s.db, &s.evaluations).await.unwrap();
    age_hours(s.db.pool()).await;
    let meter = flags_core::usage::Meter::new(s.platform.clone());
    metering::close_due_at(
        &s.db,
        |org| {
            let meter = meter.clone();
            async move { meter.billable_blocked(org).await }
        },
        3,
    )
    .await
    .unwrap();
    flags_core::usage::ship_once(&s.db, &s.platform, &Default::default())
        .await
        .unwrap();

    // 2 server answers and 3 of the 5 client answers.
    let events = metered(&platform, org);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].quantity, Some(5));

    let usage = tool(&s.app, &t, "usage", json!({})).await.unwrap();
    let e = &usage["evaluations"];
    assert_eq!(e["clientServed"], 5, "{usage}");
    assert_eq!(e["serverServed"], 2, "{usage}");
    assert_eq!(e["clipped"], 2, "{usage}");
    assert_eq!(e["apps"][0]["clientServed"], 5, "{usage}");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn unknown_flag_telemetry_and_missing_flags_count_nothing(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (_, _, key, _) = org_with_flag(&s, &platform, org, "checkout").await;

    let receipt = report_n(&s, &key, "no-such-flag", 3).await;
    assert_eq!(receipt["accepted"], 0);
    let (status, _) = sdk(
        &s.app,
        "POST",
        "/api/evaluate/no-such-flag",
        &key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert!(s.evaluations.take().is_empty());
    flush_close_ship(&s).await;
    assert!(metered(&platform, org).is_empty());
    assert_eq!(row_count(s.db.pool(), "usage_meter_hourly").await, 0);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_blocked_org_ships_its_evaluations_unmetered(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (_, _, key, _) = org_with_flag(&s, &platform, org, "checkout").await;
    platform
        .lock()
        .unwrap()
        .blocked_reason
        .insert(org, "budget_exceeded".into());

    evaluate_n(&s, &key, "checkout", 2).await;
    flush_close_ship(&s).await;

    let events: Vec<UsageEvent> = platform
        .lock()
        .unwrap()
        .events
        .iter()
        .filter(|e| e.org_id == org && e.tool == metering::EVALUATIONS_TOOL)
        .cloned()
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].meter, None);
    assert_eq!(events[0].quantity, None);
    assert!(!events[0].billable);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn evaluating_never_asks_the_platform_about_a_blocked_org(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (_, _, key, _) = org_with_flag(&s, &platform, org, "checkout").await;
    {
        let mut p = platform.lock().unwrap();
        p.blocked_reason.insert(org, "budget_exceeded".into());
        p.budget.insert(
            org,
            json!({"monthly_budget_cents": 1, "spend_cents": 1, "percent_used": 100,
                   "thresholds_reached": [100], "hard_stop": true, "blocked": true,
                   "reason": "budget_exceeded"}),
        );
    }
    let hits = |p: &Shared| {
        p.lock()
            .unwrap()
            .usage_status_hits
            .get(&org)
            .copied()
            .unwrap_or(0)
    };
    // The setup's MCP calls may have asked; only the request itself is measured.
    let before = hits(&platform);
    // Nothing has asked yet, so no cached answer could hide a request-path
    // lookup from the hit count below.
    assert_eq!(before, 0, "setup must not warm the platform's usage cache");
    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/evaluate/checkout",
        &key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["key"], "checkout");
    assert_eq!(r["enabled"], false);
    assert_eq!(hits(&platform), before);
    // Counted in memory only.
    assert_eq!(s.evaluations.take().len(), 1);
}

/// [`sdk`] with an `Origin` header (or none), as a browser sends one.
async fn sdk_from(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    origin: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "flags.example.com")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(origin) = origin {
        req = req.header("origin", origin);
    }
    let body = if method == "POST" {
        Body::from("{}")
    } else {
        Body::empty()
    };
    send(app, req.body(body).unwrap()).await
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_client_key_is_held_to_the_apps_allowed_origins(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (t, _, key, client_key) = org_with_flag(&s, &platform, org, "checkout").await;
    let evaluate = "/api/evaluate/checkout";

    // No list: unrestricted, as every app is today.
    let (status, _) = sdk_from(
        &s.app,
        "POST",
        evaluate,
        &client_key,
        Some("https://any.example"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let app = tool(
        &s.app,
        &t,
        "update_app",
        json!({"app": "web", "allowedOrigins": ["https://App.Example.com", "https://*.example.org"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        app["app"]["allowedOrigins"],
        json!(["https://app.example.com", "https://*.example.org"]),
        "{app}"
    );
    let listed = tool(&s.app, &t, "list_apps", json!({})).await.unwrap();
    assert_eq!(
        listed["apps"][0]["allowedOrigins"],
        app["app"]["allowedOrigins"]
    );

    let _ = s.evaluations.take();
    for ok in ["https://app.example.com", "https://a.example.org"] {
        let (status, r) = sdk_from(&s.app, "POST", evaluate, &client_key, Some(ok)).await;
        assert_eq!(status, StatusCode::OK, "{ok}: {r}");
    }
    assert_eq!(
        s.evaluations.take().values().next().unwrap().served_client,
        2
    );

    // Another site: refused, not counted, and the body does not echo the list.
    for bad in ["https://evil.example", "https://example.org", "null"] {
        let (status, r) = sdk_from(&s.app, "POST", evaluate, &client_key, Some(bad)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{bad}: {r}");
        assert_eq!(r["error"], "origin_not_allowed");
        assert!(!r.to_string().contains("app.example.com"), "{r}");
    }
    let (status, _) = sdk_from(
        &s.app,
        "GET",
        "/api/sdk/flags",
        &client_key,
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = sdk_from(
        &s.app,
        "POST",
        "/api/flags/checkout/evaluate",
        &client_key,
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(s.evaluations.take().is_empty());

    // No Origin header (a native app, curl) and the server key are not checked.
    let (status, _) = sdk_from(&s.app, "POST", evaluate, &client_key, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = sdk_from(&s.app, "POST", evaluate, &key, Some("https://evil.example")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = sdk_from(
        &s.app,
        "GET",
        "/api/sdk/flags",
        &key,
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = sdk_from(
        &s.app,
        "GET",
        "/api/sdk/flags",
        &client_key,
        Some("https://app.example.com"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Clearing the list lifts the restriction; doing it again changes nothing.
    for _ in 0..2 {
        let cleared = tool(
            &s.app,
            &t,
            "update_app",
            json!({"app": "web", "allowedOrigins": []}),
        )
        .await
        .unwrap();
        assert_eq!(cleared["app"]["allowedOrigins"], json!([]));
    }
    let (status, _) = sdk_from(
        &s.app,
        "POST",
        evaluate,
        &client_key,
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn only_an_admin_sets_origins_and_the_input_is_checked(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let org = Uuid::new_v4();
    let (t, _, _, _) = org_with_flag(&s, &platform, org, "checkout").await;
    let member = token(
        &platform,
        org,
        Uuid::new_v4(),
        Role::Member,
        &["flags:read", "flags:write", "apps:admin"],
    );

    let err = tool(
        &s.app,
        &member,
        "update_app",
        json!({"app": "web", "allowedOrigins": ["https://a.example"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err["data"]["code"], "forbidden", "{err}");

    for bad in [
        json!(["https://a.example/path"]),
        json!(["a.example"]),
        json!(["*"]),
    ] {
        let err = tool(
            &s.app,
            &t,
            "update_app",
            json!({"app": "web", "allowedOrigins": bad}),
        )
        .await
        .unwrap_err();
        assert_eq!(err["data"]["code"], "invalid_argument", "{err}");
    }
    let err = tool(&s.app, &t, "update_app", json!({"app": "web"}))
        .await
        .unwrap_err();
    assert_eq!(err["data"]["code"], "invalid_argument", "{err}");

    // Environments alone still work, and keep the origins.
    tool(
        &s.app,
        &t,
        "update_app",
        json!({"app": "web", "allowedOrigins": ["https://a.example"]}),
    )
    .await
    .unwrap();
    let r = tool(
        &s.app,
        &t,
        "update_app",
        json!({"app": "web", "environments": ["production"]}),
    )
    .await
    .unwrap();
    assert_eq!(r["app"]["environments"], json!(["production"]));
    assert_eq!(r["app"]["allowedOrigins"], json!(["https://a.example"]));
}

/// #123: the SDK flag list is bounded by the app's flag budget. The budget is
/// measured on the stored `jsonb` text plus `FLAG_OVERHEAD_BYTES` per flag, and
/// the body the server sends is never longer: here with the forms most likely
/// to print longer than they are stored (non-ASCII and escaped text, a
/// fractional percentage, numbers with many digits, empty variations, keys of
/// 128 characters), seeded directly up to the budget.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_sdk_flag_list_is_bounded_by_the_app_budget(pool: PgPool) {
    use flags_core::flags::{FLAG_OVERHEAD_BYTES, MAX_APP_FLAG_BYTES};
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let org = Uuid::new_v4();
    let (_, _, server_key, _) = org_with_flag(&s, &platform, org, "first").await;
    let measure = "octet_length(environments::text) + octet_length(variations::text) \
                   + coalesce(octet_length(configuration::text), 0)";
    let total = || async {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT coalesce(sum({measure} + $2), 0)::bigint FROM feature_flags \
             WHERE org_id = $1 AND status = 'active'"
        ))
        .bind(org)
        .bind(FLAG_OVERHEAD_BYTES)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let seed = |from: i64, to: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO feature_flags \
                   (org_id, app_id, key, name, environments, variations, configuration) \
                 SELECT a.org_id, a.id, lpad(i::text, 128, 'k'), 'n', \
                        '{\"production\": {\"enabled\": true, \"rollout_percentage\": 12.5, \
                          \"rules\": [{\"attribute\": \"país\", \"operator\": \"in\", \
                                       \"values\": [\"ünï ✓\", \"a\\u0001b\", \"q\\\"t\"]}]}, \
                          \"staging\": {\"enabled\": false}}'::jsonb, \
                        '{}'::jsonb, \
                        '{\"big\": 123456789012345678901234567890, \"tiny\": 0.0000001, \
                          \"text\": \"日本語\"}'::jsonb \
                   FROM flag_apps a, generate_series($2::bigint, $3::bigint) AS i \
                  WHERE a.org_id = $1",
            )
            .bind(org)
            .bind(from)
            .bind(to)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    seed(1, 1).await;
    let one = total().await;
    let first_share = {
        // The flag org_with_flag made, measured alone.
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT ({measure} + $2)::bigint FROM feature_flags WHERE org_id = $1 AND key = 'first'"
        ))
        .bind(org)
        .bind(FLAG_OVERHEAD_BYTES)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let per_flag = one - first_share;
    let more = (MAX_APP_FLAG_BYTES - one) / per_flag;
    seed(2, 1 + more).await;
    let stored = total().await;
    assert!(stored <= MAX_APP_FLAG_BYTES, "{stored}");
    assert!(stored > MAX_APP_FLAG_BYTES - per_flag, "{stored}");

    let res = s
        .app
        .clone()
        .oneshot(
            Request::get("/api/sdk/flags?environment=production")
                .header("host", "flags.example.com")
                .header("authorization", format!("Bearer {server_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1 << 22)
        .await
        .unwrap();
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list["count"], 2 + more);
    assert!(
        list.to_string().contains("ünï ✓"),
        "rules are listed for a server key"
    );
    // The envelope (count, organization and application ids) is about 150 bytes.
    let sent = body.len() as i64;
    assert!(
        sent <= stored + 256,
        "sent {sent} bytes for {stored} stored"
    );
}

/// #123: a write past the per-flag budget comes back through MCP as an
/// invalid argument naming the limit, built in code (no database text), and
/// changes nothing.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_oversized_flag_write_is_refused_over_mcp(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let (t, _, _, _) = org_with_flag(&s, &platform, Uuid::new_v4(), "f").await;
    let big = "x".repeat(32_000);
    let variations: serde_json::Map<String, Value> = ["a", "b", "c", "d"]
        .iter()
        .map(|v| (v.to_string(), json!({"configuration": big})))
        .collect();
    let err = tool(
        &s.app,
        &t,
        "update_flag",
        json!({"app": "web", "key": "f", "variations": variations, "configuration": big}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("131072"), "{err}");
    assert!(err.contains("would be"), "{err}");
    for leak in ["ERROR", "SQLSTATE", "sqlx", "feature_flags"] {
        assert!(!err.contains(leak), "database text in {err}");
    }
    let f = flag(&s, &t, "f").await;
    assert_eq!(f["version"], 1, "{f}");
}
