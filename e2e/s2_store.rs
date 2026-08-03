use std::env;
use std::error::Error;
use std::io;

use sqlx::postgres::PgPoolOptions;
use steward_store::PgStore;
use steward_types::SpendSummary;

#[tokio::test]
async fn s2_spend_observations_are_append_only() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S2 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    sqlx::query(
        "INSERT INTO spend_observations \
         (runtime_uid, observed_amount, currency, exhausted) \
         VALUES ('runtime-a', '1.00', 'USD', true)",
    )
    .execute(store.pool())
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "a spend projection from the inference plane must be recordable: {error}"
        ))
    })?;

    let mutation = sqlx::query(
        "UPDATE spend_observations \
         SET observed_amount = '0.00' \
         WHERE runtime_uid = 'runtime-a'",
    )
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "observed spend history must reject mutation after it is recorded"
    );
    let deletion = sqlx::query("DELETE FROM spend_observations WHERE runtime_uid = 'runtime-a'")
        .execute(store.pool())
        .await;
    assert!(
        deletion.is_err(),
        "observed spend history must reject deletion after it is recorded"
    );
    Ok(())
}

#[tokio::test]
async fn s2_exhaustion_authority_survives_spec_generation_changes() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S2 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let spend = SpendSummary {
        observed_amount: "1.00".to_owned(),
        currency: "USD".to_owned(),
    };
    store
        .record_spend_observation("runtime-b", 3, "spec-digest-a", &spend, true)
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "budget exhaustion must be durably recordable before credential revocation: {error}"
            ))
        })?;

    assert_eq!(
        store.inference_exhaustion("runtime-b").await?,
        Some(spend),
        "durable exhaustion must remain visible after unrelated spec generation changes"
    );
    Ok(())
}
