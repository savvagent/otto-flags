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
use flags_core::notify::{FlagChange, Listener};
use flags_core::platform_events::begin_live;
use flags_core::rollback::{CheckerConfig, Hints, Mode, RollbackExt, Thresholds};
use flags_server::{platform_client, router, spawn_checker, Config};
use http::{Request, StatusCode};
use otto_resource::{
    IntrospectionResponse, MemberInfo, OrgInfo, Role, TokenKind, UsageBatch, UsageReceipt, UserInfo,
};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Db;
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
    /// Usage-status requests received, per org.
    usage_status_hits: HashMap<Uuid, usize>,
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
            aud: Some(RESOURCE.into()),
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
    if !p.legacy.contains(&org) {
        status["features"] = if p.entitled.contains(&org) {
            json!({"auto_rollback": true})
        } else {
            json!({})
        };
    }
    Ok(Json(status))
}

async fn ship(State(p): State<Shared>, Json(b): Json<UsageBatch>) -> Json<UsageReceipt> {
    p.lock().unwrap().shipped += b.events.len();
    Json(UsageReceipt {
        accepted: b.events.len() as u32,
        duplicates: 0,
        rejected: vec![],
    })
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
        .route("/internal/orgs/{org}/members/{user}", get(member))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

fn token(p: &Shared, org: Uuid, user: Uuid, role: Role, scopes: &[&str]) -> String {
    let t = format!(
        "otto_at_{}",
        "a".repeat(11) + &Uuid::new_v4().simple().to_string()
    );
    assert!(otto_resource::looks_like_token(&t), "{t}");
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
    platform: Arc<otto_resource::PlatformClient>,
}

async fn server(pool: PgPool, platform_url: &str) -> Server {
    let mut config = Config::for_test();
    config.platform_url = platform_url.into();
    let db = Db::from_pool(pool);
    let platform = platform_client(&config).unwrap();
    Server {
        app: router(
            db.clone(),
            platform.clone(),
            Listener::detached(),
            Hints::detached(),
            &config,
        ),
        db,
        platform,
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
    let server = Server {
        app: router(
            db.clone(),
            platform.clone(),
            listener.clone(),
            hints,
            &config,
        ),
        db,
        platform,
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
    let s = server(pool, &url).await;
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

    let body = otto_resource::webhook::body(
        Uuid::new_v4(),
        "org.deleted",
        chrono::Utc::now(),
        &json!({"org_id": org}),
    );
    let deliver = |sig: String| {
        Request::post("/platform/webhooks")
            .header("host", "flags.example.com")
            .header(otto_resource::webhook::SIGNATURE_HEADER, sig)
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let forged =
        otto_resource::webhook::sign("otto_whsec_wrong", chrono::Utc::now().timestamp(), &body);
    let (status, _) = send(&s.app, deliver(forged)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let good =
        otto_resource::webhook::sign("otto_whsec_test", chrono::Utc::now().timestamp(), &body);
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
    let (_, _, server_key, _) = org_with_flag(&s, &platform, Uuid::new_v4(), "checkout").await;
    let now = chrono::Utc::now().timestamp();

    let (status, r) = sdk(
        &s.app,
        "POST",
        "/api/telemetry/errors",
        &server_key,
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
        &server_key,
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

/// Only setting a policy is billed; the rest record a free row, except the
/// dry run, which records none. A refused set is never billed.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn only_setting_a_policy_is_billed(pool: PgPool) {
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
            ("set_rollback_policy".to_string(), true, 1),
        ]
    );
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
