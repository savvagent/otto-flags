//! The whole server, end to end: an agent manages a flag over MCP (bearer
//! token introspected by a mock platform), an app evaluates it over the SDK
//! API, telemetry flows back into `flag_health`, and the platform's webhooks
//! clean up. Needs `DATABASE_URL` (see `compose.yaml`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use flags_core::notify::Listener;
use flags_server::{platform_client, router, Config};
use http::{Request, StatusCode};
use otto_resource::{
    IntrospectionResponse, MemberInfo, OrgInfo, Role, TokenKind, UsageBatch, UsageReceipt,
    UsageStatus, UserInfo,
};
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

async fn usage_status(Path(org): Path<Uuid>) -> Json<UsageStatus> {
    Json(UsageStatus {
        org_id: org,
        plan: "team".into(),
        period_start: chrono::Utc::now().date_naive(),
        billable_count: 0,
        total_count: 0,
        included_ops: 1000,
        hard_stop: false,
    })
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
        app: router(db.clone(), platform.clone(), Listener::detached(), &config),
        db,
        platform,
    }
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
