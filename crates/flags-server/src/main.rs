//! `flags-server` — boots otto-flags' own domain database (physically
//! separate from otto-platform's), runs its migrations, and proves tenant
//! isolation is actually in force before calling itself ready. Mirrors
//! `otto-platform-server`'s boot sequence exactly; see that binary's module
//! docs in the otto-platform repo for the full rationale.
//!
//! Not a full MCP server yet — see `docs/specs/2026-09-15-otto-flags-design.md`
//! §6 and `VISION.md`. What this binary proves today is narrower: that
//! `flags-core`'s schema applies cleanly to a fresh database and that
//! row-level security is genuinely enforced against whatever role it
//! connects as, using `otto_tenant::Db` unmodified from otto-platform.

use anyhow::{Context, Result};
use otto_tenant::Db;

#[tokio::main]
async fn main() -> Result<()> {
    let dotenv = dotenvy::dotenv();

    init_tracing()?;
    match dotenv {
        Ok(path) => tracing::debug!(path = %path.display(), "loaded .env"),
        Err(_) => tracing::debug!("no .env file; using the process environment"),
    }

    // Named distinctly from otto-platform-server's DATABASE_URL/
    // OTTO_RUN_MIGRATIONS on purpose: the two servers point at two physically
    // separate databases (design doc §3), and sharing an env var name would
    // make it easy to run both processes in the same shell against the
    // wrong one.
    let database_url = std::env::var("FLAGS_DATABASE_URL")
        .context("FLAGS_DATABASE_URL must be set. Copy .env.example to .env for local runs.")?;

    let run_migrations = std::env::var("FLAGS_RUN_MIGRATIONS")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);

    let db = Db::connect(&database_url)
        .await
        .context("could not connect to FLAGS_DATABASE_URL")?;

    if run_migrations {
        tracing::info!("applying migrations");
        // otto_tenant::Db::migrate() is fixed to otto-tenant's own migrations
        // directory, so it cannot run flags-core's schema — see
        // flags-core::lib's doc comment. sqlx::migrate! is invoked here
        // instead, pointed at flags-core's migrations relative to this
        // crate's manifest dir.
        sqlx::migrate!("../flags-core/migrations")
            .run(db.pool())
            .await
            .context("migrations failed")?;
    } else {
        tracing::warn!("FLAGS_RUN_MIGRATIONS is off; assuming the schema is already current");
    }

    let isolation = db
        .verify_tenant_isolation()
        .await
        .context("refusing to serve: tenant isolation is not enforced by this database")?;
    tracing::info!("{}", isolation.summary());

    tracing::info!("flags-server ready: schema migrated, tenant isolation verified");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;
    tracing::info!("shutting down");
    Ok(())
}

fn init_tracing() -> Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .context("could not initialize tracing")?;
    Ok(())
}
