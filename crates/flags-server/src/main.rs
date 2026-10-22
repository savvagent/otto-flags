//! otto-flags server binary: every HTTP surface on one port, plus the
//! background tasks (usage shipper, flag-change listener, rollback checker,
//! retention sweeps).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use flags_core::metering::EvalCounter;
use flags_core::notify::Listener;
use flags_core::rollback::CheckerConfig;
use flags_core::usage::{DeclarationHealth, Meter, ShipperConfig};
use flags_server::{
    platform_client, router_with_meter_health, spawn_checker, watch_checker, Config, LogFormat,
};
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
    config.warn_retired();
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

    // The org.deleted purge removes a deleted org's audit trail through a
    // function whose owner must be exempt from row-level security (#40).
    // Like isolation, only the running database can say whether it is.
    let purge = flags_core::audit_purge::check(&db)
        .await
        .context("refusing to serve: could not read the audit purge function from the catalog")?;
    let problems = purge.problems();
    if !problems.is_empty() {
        anyhow::bail!(
            "refusing to serve: an org.deleted could not purge the org's audit trail: {}",
            problems.join(" | ")
        );
    }
    tracing::info!("{}", purge.summary());

    let platform = platform_client(&config)?;
    check_platform(&platform, &config).await;

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    // Usage is flushed on its own, earlier signal (see the graceful shutdown
    // below). Everything else, the flag-change listener above all, keeps
    // running until the HTTP drain ends, so a stream still open during the
    // drain keeps receiving changes (a kill switch included).
    let (stop_usage, usage_shutdown) = tokio::sync::watch::channel(false);
    let listener = Listener::spawn(db.pool().clone(), shutdown.clone());
    let meter_health = DeclarationHealth::default();
    let shipper = tokio::spawn(flags_core::usage::run_reporting(
        db.clone(),
        platform.clone(),
        ShipperConfig::default(),
        usage_shutdown.clone(),
        meter_health.clone(),
    ));
    let evaluations = Arc::new(EvalCounter::default());
    let metering = tokio::spawn(flags_core::metering::run(
        db.clone(),
        Meter::new(platform.clone()),
        evaluations.clone(),
        usage_shutdown,
    ));
    let sweeper = tokio::spawn(sweep_loop(db.clone(), shutdown.clone()));

    let (hints, checker) = spawn_checker(
        db.clone(),
        platform.clone(),
        &config,
        CheckerConfig::default(),
        shutdown.clone(),
    );
    // The watcher logs at error if the checker ends while still serving; on
    // shutdown it is what is awaited, and the abort handle stops the checker
    // itself if it does not stop in time.
    let checker_abort = checker.abort_handle();
    let mut checker = tokio::spawn(watch_checker(checker, shutdown.clone()));

    let app = router_with_meter_health(
        db.clone(),
        platform,
        listener,
        hints,
        evaluations.clone(),
        &config,
        meter_health,
    );
    let tcp = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("could not bind {}", config.bind))?;
    tracing::info!(
        bind = %config.bind,
        public_url = %config.public_url,
        resource_uri = %config.resource_uri,
        platform_url = %config.platform_url,
        "flags-server listening"
    );

    // Stop the usage tasks as soon as the signal arrives, not after the HTTP
    // drain: an SDK flag stream stays open until the client goes, so the drain
    // can outlast the platform's kill timeout, and the shipper's and the
    // meter's final passes must not wait behind it.
    axum::serve(tcp, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("flushing usage");
            stop_usage.send(true).ok();
        })
        .await
        .context("server error")?;

    stop.send(true).ok();
    if let Err(e) = shipper.await {
        let reason = join_failure_reason(&e);
        tracing::warn!(reason, "the usage shipper did not stop cleanly");
        tracing::debug!(reason, error = %e, "the usage shipper did not stop cleanly");
    }
    if let Err(e) = metering.await {
        let reason = join_failure_reason(&e);
        tracing::warn!(reason, "the evaluation metering task did not stop cleanly");
        tracing::debug!(reason, error = %e, "the evaluation metering task did not stop cleanly");
    }
    // Evaluations answered during the drain, after the metering task's own
    // final flush. Bounded server-side: `flush` is not cancel-safe.
    if let Err(e) = flags_core::metering::flush_with(
        &db,
        &evaluations,
        flags_core::metering::FINAL_FLUSH_TIMEOUT,
        Duration::ZERO,
    )
    .await
    {
        let f = e.failure_reason();
        tracing::warn!(
            reason = f.reason,
            sqlstate = f.sqlstate(),
            pending = evaluations.pending(),
            "could not flush the evaluations answered while draining; they are lost"
        );
        tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), error = %e,
            "could not flush the evaluations answered while draining; they are lost");
    }
    match tokio::time::timeout(Duration::from_secs(5), &mut checker).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let reason = join_failure_reason(&e);
            tracing::warn!(reason, "the rollback checker did not stop cleanly");
            tracing::debug!(reason, error = %e, "the rollback checker did not stop cleanly");
        }
        Err(_) => {
            tracing::warn!("the rollback checker did not stop within 5 s; aborting it");
            checker_abort.abort();
            checker.abort();
        }
    }
    sweeper.abort();
    tracing::info!("stopped");
    Ok(())
}

const PLATFORM_EVENT_RETENTION_DAYS: i32 = 30;

/// Whether a background task panicked or was cancelled; the panic payload
/// stays at debug with the rest of the error text.
fn join_failure_reason(e: &tokio::task::JoinError) -> &'static str {
    if e.is_panic() {
        "panicked"
    } else {
        "cancelled"
    }
}

/// Hourly retention: platform-event markers, removed-member tombstones, error
/// reports, old evaluation buckets, old flag versions, and rejected usage
/// events.
async fn sweep_loop(db: Db, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        match flags_core::platform_events::sweep(&db, PLATFORM_EVENT_RETENTION_DAYS).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept old platform-event records"),
            Err(e) => {
                let f = e.failure_reason();
                tracing::warn!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "platform-event sweep failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "platform-event sweep failed");
            }
        }
        match flags_core::telemetry::sweep(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept expired telemetry"),
            Err(e) => {
                let f = e.failure_reason();
                tracing::warn!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "telemetry sweep failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "telemetry sweep failed");
            }
        }
        match flags_core::flags::sweep_versions(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept expired flag versions"),
            Err(e) => {
                let f = e.failure_reason();
                tracing::warn!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "flag-version sweep failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "flag-version sweep failed");
            }
        }
        match flags_core::usage::sweep_rejected(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept expired rejected usage events"),
            Err(e) => {
                let f = e.failure_reason();
                tracing::warn!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "rejected-usage sweep failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "rejected-usage sweep failed");
            }
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
        Err(e) => {
            let f = flags_core::error::platform_failure_reason(&e);
            tracing::warn!(
                platform = %config.platform_url,
                reason = f.reason,
                status = f.status,
                transport = f.transport,
                "could not reach the otto platform at startup; continuing"
            );
            tracing::debug!(
                platform = %config.platform_url,
                reason = f.reason,
                status = f.status,
                transport = f.transport,
                error = %e,
                "could not reach the otto platform at startup; continuing"
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn join_failures_name_panic_and_cancel_without_the_payload() {
        let panicked = tokio::spawn(async { panic!("secret") })
            .await
            .expect_err("the task panicked");
        assert_eq!(join_failure_reason(&panicked), "panicked");

        let pending = tokio::spawn(std::future::pending::<()>());
        pending.abort();
        let cancelled = pending.await.expect_err("the task was aborted");
        assert_eq!(join_failure_reason(&cancelled), "cancelled");
    }
}
