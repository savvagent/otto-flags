//! otto-flags server binary: every HTTP surface on one port, plus the
//! background tasks (usage shipper, flag-change listener, retention sweeps).

use std::time::Duration;

use anyhow::{Context, Result};
use flags_core::notify::Listener;
use flags_core::usage::ShipperConfig;
use flags_server::{platform_client, router, Config, LogFormat};
use otto_tenant::Db;
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Never overrides a variable already set, so a deployment's environment
    // always wins over a stray .env.
    let dotenv = dotenvy::dotenv();
    let config = Config::from_env().context(
        "configuration is incomplete. Copy .env.example to .env for local runs, or set the \
         variables named above in the deployment",
    )?;
    init_tracing(config.log_format);
    if let Ok(path) = dotenv {
        tracing::debug!(path = %path.display(), "loaded .env");
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
        .context("could not connect to the database")?;
    let db = Db::from_pool(pool);

    if config.run_migrations {
        tracing::info!("applying migrations");
        flags_core::migrate(&db)
            .await
            .context("migrations failed")?;
    } else {
        tracing::warn!("FLAGS_RUN_MIGRATIONS is off; assuming the schema is already current");
    }

    // Prove tenant isolation before binding a port. The same migrations
    // isolate perfectly under one database role and not at all under another,
    // and only the running database can say which this is.
    let isolation = db
        .verify_tenant_isolation()
        .await
        .context("refusing to serve: tenant isolation is not enforced by this database")?;
    tracing::info!("{}", isolation.summary());

    let platform = platform_client(&config)?;
    check_platform(&platform, &config).await;

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let listener = Listener::spawn(db.pool().clone(), shutdown.clone());
    let shipper = tokio::spawn(flags_core::usage::run(
        db.clone(),
        platform.clone(),
        ShipperConfig::default(),
        shutdown.clone(),
    ));
    let sweeper = tokio::spawn(sweep_loop(db.clone(), shutdown.clone()));

    let app = router(db, platform, listener, &config);
    let tcp = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("could not bind {}", config.bind))?;
    tracing::info!(
        bind = %config.bind,
        public_url = %config.public_url,
        resource_uri = %config.resource_uri,
        platform_url = %config.platform_url,
        enforce_quotas = config.enforce_quotas,
        "flags-server listening"
    );

    axum::serve(tcp, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("flushing usage");
    stop.send(true).ok();
    if let Err(e) = shipper.await {
        tracing::warn!(error = %e, "the usage shipper did not stop cleanly");
    }
    sweeper.abort();
    tracing::info!("stopped");
    Ok(())
}

const PLATFORM_EVENT_RETENTION_DAYS: i32 = 30;

/// Hourly retention: platform-event markers, removed-member tombstones, error
/// reports, and old evaluation buckets.
async fn sweep_loop(db: Db, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        match flags_core::platform_events::sweep(&db, PLATFORM_EVENT_RETENTION_DAYS).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept old platform-event records"),
            Err(e) => tracing::warn!(error = %e, "platform-event sweep failed"),
        }
        match flags_core::telemetry::sweep(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept expired telemetry"),
            Err(e) => tracing::warn!(error = %e, "telemetry sweep failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// Learn at startup whether the platform accepts our credential. Non-fatal:
/// a platform outage must not stop a restart.
async fn check_platform(platform: &otto_resource::PlatformClient, config: &Config) {
    match platform.usage_status(uuid::Uuid::nil()).await {
        Ok(_) | Err(otto_resource::Error::NotFound) => {
            tracing::info!(platform = %config.platform_url, "the otto platform accepted this service's credential")
        }
        Err(otto_resource::Error::Unauthorized) => tracing::error!(
            platform = %config.platform_url,
            resource_uri = %config.resource_uri,
            "the otto platform REJECTED this service's credential: no token can be validated and \
             no usage can be shipped. Check FLAGS_INTROSPECTION_SECRET and that the resource URI \
             is the one this service was registered with"
        ),
        Err(e) => tracing::warn!(
            platform = %config.platform_url,
            error = %e,
            "could not reach the otto platform at startup; continuing"
        ),
    }
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json())
            .init(),
        LogFormat::Text => registry.with(tracing_subscriber::fmt::layer()).init(),
    }
}

/// SIGTERM is what a container runtime sends before SIGKILL; handling only
/// SIGINT would hard-kill every deploy.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("SIGINT handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT; shutting down"),
        _ = terminate => tracing::info!("SIGTERM; shutting down"),
    }
}
