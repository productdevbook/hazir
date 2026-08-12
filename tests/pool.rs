//! What the pool claims to do, asked of a real Postgres.
//!
//! `HAZIR_URL` says where. Without it these do not run: a url guessed here
//! would either fail to connect, which reads as the crate being broken rather
//! than the machine being unconfigured, or connect to something somebody was
//! using.

use std::path::PathBuf;
use std::time::Duration;

use hazir::{Pool, Recipe};
use tokio_postgres::NoTls;

fn url() -> Option<String> {
    std::env::var("HAZIR_URL")
        .ok()
        .filter(|url| !url.is_empty())
}

/// A pool with the fixture migrations in it, warmed once for the whole run.
async fn ready(want: usize) -> Option<(Pool, String)> {
    let url = url()?;
    let pool = Pool::connect(&url).await.expect("a Postgres that answers");
    pool.bootstrap().await.expect("the pool's own tables");

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/migrations");
    let recipe = Recipe::Sql(hazir::sql_files(&dir).expect("the fixture migrations"));
    let fingerprint = hazir::fingerprint(&[dir], &recipe).expect("a fingerprint");

    hazir::capture(&pool, &fingerprint, &recipe)
        .await
        .expect("the schema to be built once");
    pool.warm(&fingerprint, want).await.expect("a warm pool");

    Some((pool, fingerprint))
}

async fn open(url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .expect("a connection");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn count(client: &tokio_postgres::Client, table: &str) -> i64 {
    client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("a count")
        .get(0)
}

#[tokio::test]
async fn a_lease_arrives_with_the_tables_already_in_it() {
    let Some((_pool, _)) = ready(4).await else {
        return;
    };
    let lease = hazir::lease().await.expect("a schema");
    let client = open(lease.url()).await;

    client
        .execute("INSERT INTO site (name) VALUES ($1)", &[&"a made-up site"])
        .await
        .expect("the tables to be there");
    assert_eq!(count(&client, "site").await, 1);
}

/// The whole point of recycling: what comes back is empty, and what comes back
/// is the same schema rather than a new one.
#[tokio::test]
async fn a_schema_that_comes_back_comes_back_empty() {
    let Some((pool, _)) = ready(1).await else {
        return;
    };

    let first = hazir::lease().await.expect("a schema");
    let name = first.schema().to_owned();
    {
        let client = open(first.url()).await;
        client
            .execute("INSERT INTO site (name) VALUES ($1)", &[&"left behind"])
            .await
            .expect("a row");
        assert_eq!(count(&client, "site").await, 1);
    }
    drop(first);

    // Still the same schema, rather than one dropped and built again. That is
    // the claim being made, and it holds however many other schemas the pool
    // happens to have.
    let kept: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM hazir.pool WHERE schema_name = $1",
            &[&name],
        )
        .await
        .expect("a count")
        .get(0);
    assert_eq!(kept, 1, "the schema was thrown away rather than recycled");

    let again = hazir::lease().await.expect("a schema");
    let client = open(again.url()).await;
    assert_eq!(
        count(&client, "site").await,
        0,
        "a recycled schema still had the last test's rows in it"
    );
}

/// The reason recycling exists. Dropping and recreating writes to pg_class and
/// pg_attribute once per table per test, and a long run spends itself in
/// autovacuum rather than in the tests.
#[tokio::test]
async fn a_run_of_leases_does_not_leave_schemas_behind() {
    let Some((pool, fingerprint)) = ready(2).await else {
        return;
    };

    let before: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM hazir.pool WHERE fingerprint = $1",
            &[&fingerprint],
        )
        .await
        .expect("a count")
        .get(0);

    for _ in 0..20 {
        let lease = hazir::lease().await.expect("a schema");
        let client = open(lease.url()).await;
        client
            .execute("INSERT INTO site (name) VALUES ($1)", &[&"passing through"])
            .await
            .expect("a row");
    }

    let after: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM hazir.pool WHERE fingerprint = $1",
            &[&fingerprint],
        )
        .await
        .expect("a count")
        .get(0);

    assert!(
        after <= before + 2,
        "twenty leases grew the pool from {before} to {after}; they should have been reused"
    );
}

/// A test that alters what it was given must not hand it on looking clean.
#[tokio::test]
async fn a_schema_whose_shape_moved_is_not_passed_on() {
    let Some((pool, fingerprint)) = ready(2).await else {
        return;
    };

    let lease = hazir::lease().await.expect("a schema");
    let name = lease.schema().to_owned();
    {
        let client = open(lease.url()).await;
        client
            .execute("ALTER TABLE site ADD COLUMN motto text", &[])
            .await
            .expect("the alteration");
    }
    drop(lease);

    let still_there: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM hazir.pool WHERE schema_name = $1",
            &[&name],
        )
        .await
        .expect("a count")
        .get(0);
    assert_eq!(still_there, 0, "an altered schema went back into the pool");

    let next = hazir::lease().await.expect("a schema");
    let client = open(next.url()).await;
    let columns: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = $1 AND table_name = 'site' AND column_name = 'motto'",
            &[&next.schema()],
        )
        .await
        .expect("a count")
        .get(0);
    assert_eq!(
        columns, 0,
        "the alteration followed the schema to the next test"
    );
    let _ = fingerprint;
}

/// A test that panics never tidies up. Nothing here depends on it doing so.
#[tokio::test]
async fn what_a_finished_process_was_holding_comes_back() {
    let Some((pool, fingerprint)) = ready(2).await else {
        return;
    };

    let lease = hazir::lease().await.expect("a schema");
    let name = lease.schema().to_owned();
    std::mem::forget(lease);

    // A pid that cannot be running: nothing is ever numbered zero.
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    let dead = format!("{}/{}/0", host.trim(), boot.trim().replace('-', ""));
    pool.client()
        .execute(
            "UPDATE hazir.pool SET holder = $2 WHERE schema_name = $1",
            &[&name, &dead],
        )
        .await
        .expect("the claim to be rewritten");

    let taken = pool
        .reclaim(Duration::from_secs(3600))
        .await
        .expect("a reclaim");
    assert!(taken >= 1);

    let state: String = pool
        .client()
        .query_one(
            "SELECT state FROM hazir.pool WHERE schema_name = $1",
            &[&name],
        )
        .await
        .expect("the row")
        .get(0);
    assert_eq!(state, "ready");
    let _ = fingerprint;
}

/// Two processes reaching for the last ready schema is the ordinary case.
#[tokio::test]
async fn no_two_leases_are_the_same_schema() {
    let Some((_pool, _)) = ready(8).await else {
        return;
    };

    let mut held = Vec::new();
    for _ in 0..6 {
        held.push(hazir::lease().await.expect("a schema"));
    }

    let names: std::collections::HashSet<&str> = held.iter().map(|lease| lease.schema()).collect();
    assert_eq!(
        names.len(),
        held.len(),
        "one schema was leased twice at once"
    );
}

/// A schema for a test that is going to break it, dropped rather than reused.
#[tokio::test]
async fn a_fresh_lease_is_thrown_away_afterwards() {
    let Some((pool, _)) = ready(2).await else {
        return;
    };

    let lease = hazir::lease_fresh().await.expect("a schema of its own");
    let name = lease.schema().to_owned();
    drop(lease);

    let left: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM information_schema.schemata WHERE schema_name = $1",
            &[&name],
        )
        .await
        .expect("a count")
        .get(0);
    assert_eq!(left, 0, "a burn-after-use schema was kept");
}
