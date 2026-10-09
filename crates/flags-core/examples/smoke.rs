//! Manual end-to-end check against a real Postgres: create an app and a
//! flag, exercise the duplicate-key and patch/archive paths, record
//! evaluations, and confirm cross-tenant RLS isolation. Run migrations first
//! (`cargo run -p flags-server`, or `sqlx migrate run --source
//! crates/flags-core/migrations`), then:
//!
//! ```sh
//! FLAGS_DATABASE_URL=postgres://postgres:postgres@localhost:5432/otto_flags \
//!   cargo run -p flags-core --example smoke
//! ```

use flags_core::apps::AppsExt;
use flags_core::evaluations::EvaluationsExt;
use flags_core::flags::FlagsExt;
use otto_tenant::ids::OrgId;
use otto_tenant::Db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let database_url = std::env::var("FLAGS_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/otto_flags".to_string());
    let db = Db::connect(&database_url).await?;
    let org = OrgId::new();
    let other_org = OrgId::new();

    let (app, flag) = {
        let mut tx = db.begin(org).await?;
        let app = tx
            .create_app(
                "acme-web",
                &["production".to_string(), "staging".to_string()],
            )
            .await?;
        println!(
            "created app: {} keys=({}, {})",
            app.id, app.client_key, app.server_key
        );

        let flag = tx
            .create_flag(app.id, "new-checkout", "New checkout flow")
            .await?;
        println!(
            "created flag: {} key={} status={:?} version={}",
            flag.id, flag.key, flag.status, flag.version
        );
        tx.commit().await?;
        (app, flag)
    };

    // Duplicate key: its own transaction, since a failed statement poisons the
    // rest of a Postgres transaction until rollback — the shape a real caller
    // (one transaction per MCP tool call) already has for free.
    {
        let mut tx = db.begin(org).await?;
        let dup = tx.create_flag(app.id, "new-checkout", "dup").await;
        assert!(matches!(
            dup,
            Err(flags_core::Error::DuplicateFlagKey { .. })
        ));
        println!("duplicate key correctly rejected: {:?}", dup.unwrap_err());
        tx.rollback().await?;
    }

    {
        let mut tx = db.begin(org).await?;
        let updated = tx
            .update_flag(
                flag.id,
                flags_core::flags::FlagPatch {
                    environments: Some(
                        serde_json::json!({"production": {"enabled": true, "rolloutPercent": 10}}),
                    ),
                    ..Default::default()
                },
            )
            .await?;
        println!(
            "updated flag version={} environments={}",
            updated.version, updated.environments
        );

        tx.record_evaluation(
            app.id,
            flag.id,
            "production",
            "treatment",
            serde_json::json!({"userId": "u1"}),
        )
        .await?;
        tx.record_evaluation(
            app.id,
            flag.id,
            "production",
            "control",
            serde_json::json!({"userId": "u2"}),
        )
        .await?;

        let recent = tx.recent_evaluations(flag.id, 10).await?;
        println!("recorded {} evaluations", recent.len());
        assert_eq!(recent.len(), 2);

        let archived = tx.archive_flag(flag.id).await?;
        println!(
            "archived flag status={:?} archived_at={:?}",
            archived.status, archived.archived_at
        );
        tx.commit().await?;
    }

    // Cross-tenant isolation: a different org must see none of this.
    {
        let mut tx = db.begin(other_org).await?;
        let apps = tx.list_apps().await?;
        assert!(
            apps.is_empty(),
            "RLS leak: other org saw {} apps",
            apps.len()
        );
        println!("cross-tenant isolation confirmed: other org sees 0 apps");
        tx.commit().await?;
    }

    println!("SMOKE TEST PASSED");
    Ok(())
}
