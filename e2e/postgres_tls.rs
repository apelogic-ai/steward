use std::env;
use std::error::Error;
use std::io;

use sqlx::postgres::PgPoolOptions;
use steward_store::PgStore;

#[tokio::test]
async fn tls_required_postgres_accepts_store_migrations() -> Result<(), Box<dyn Error>> {
    let plaintext_url = env::var("STEWARD_TEST_PLAINTEXT_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_PLAINTEXT_DATABASE_URL is required for the TLS test")
    })?;
    let plaintext = PgPoolOptions::new()
        .max_connections(1)
        .connect(&plaintext_url)
        .await;
    assert!(
        plaintext.is_err(),
        "the PostgreSQL fixture must reject plaintext connections"
    );

    let tls_url = env::var("STEWARD_TEST_TLS_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_TLS_DATABASE_URL is required for the TLS test")
    })?;
    let store = PgStore::connect(&tls_url).await.map_err(|error| {
        io::Error::other(format!(
            "Steward must connect when PostgreSQL requires sslmode=require: {error}"
        ))
    })?;
    store.migrate().await.map_err(|error| {
        io::Error::other(format!(
            "Steward migrations must complete over the required TLS session: {error}"
        ))
    })?;

    let tls_active =
        sqlx::query_scalar::<_, bool>("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(store.pool())
            .await?;
    assert!(
        tls_active,
        "the successful Steward database session must be encrypted"
    );

    let applied_migrations =
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM _sqlx_migrations")
            .fetch_one(store.pool())
            .await?;
    assert!(
        applied_migrations > 0,
        "the TLS database must contain applied Steward migrations"
    );

    Ok(())
}
