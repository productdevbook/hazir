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
    // Storing a snapshot no longer makes it the one `hazir::lease` gets, and a
    // database this suite has not run against before has no other.
    pool.set_current(&fingerprint)
        .await
        .expect("the snapshot to be the one a lease gets");
    pool.warm(&fingerprint, want).await.expect("a warm pool");

    Some((pool, fingerprint))
}

/// A snapshot nothing else in this file will lease from.
async fn own_snapshot(pool: &Pool, what: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/migrations");
    let recipe = Recipe::Sql(hazir::sql_files(&dir).expect("the fixture migrations"));
    let fingerprint = format!("{:0<64}", format!("own{what}"));
    hazir::capture(pool, &fingerprint, &recipe)
        .await
        .expect("a snapshot of its own");
    fingerprint
}

/// The url a lease would have handed back, for a schema claimed directly.
fn hazir_url_for(pool: &Pool, schema: &str) -> String {
    let (base, _) = pool.url().split_once('?').unwrap_or((pool.url(), ""));
    format!("{base}?options=-csearch_path%3D{schema}")
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
///
/// On a snapshot of its own, because what it is counting is the size of a
/// pool, and every other test in this file is leasing out of the shared one.
/// Told that twenty leases grew the pool it cannot tell "nothing was
/// recycled" from "a sibling was holding them all".
#[tokio::test]
async fn a_run_of_leases_does_not_leave_schemas_behind() {
    let Some((pool, _)) = ready(2).await else {
        return;
    };
    let mine = own_snapshot(&pool, "recycling").await;
    pool.warm(&mine, 2).await.expect("a pool of its own");

    let before = held(&pool, &mine).await;
    for _ in 0..20 {
        let schema = pool.claim_from(&mine, false).await.expect("a schema");
        let client = open(&hazir_url_for(&pool, &schema)).await;
        client
            .execute("INSERT INTO site (name) VALUES ($1)", &[&"passing through"])
            .await
            .expect("a row");
        drop(client);
        pool.give_back(&schema, false)
            .await
            .expect("giving it back");
    }
    let after = held(&pool, &mine).await;

    assert_eq!(
        after, before,
        "twenty leases grew the pool from {before} to {after}; they should have been reused"
    );
}

async fn held(pool: &Pool, fingerprint: &str) -> i64 {
    pool.client()
        .query_one(
            "SELECT count(*) FROM hazir.pool WHERE fingerprint = $1",
            &[&fingerprint],
        )
        .await
        .expect("a count")
        .get(0)
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

/// A suite with two shapes of database asks for one by name, and the agent
/// has to carry that through rather than handing out whatever is current.
#[tokio::test]
async fn a_lease_can_name_the_snapshot_it_wants() {
    let Some((pool, shared)) = ready(2).await else {
        return;
    };
    let mine = own_snapshot(&pool, "named").await;
    pool.warm(&mine, 2).await.expect("a pool of its own");
    assert_ne!(mine, shared);

    let lease = hazir::lease_from(&mine).await.expect("a schema");

    let from: String = pool
        .client()
        .query_one(
            "SELECT fingerprint FROM hazir.pool WHERE schema_name = $1",
            &[&lease.schema()],
        )
        .await
        .expect("the row")
        .get(0);
    assert_eq!(from, mine, "the lease came out of the wrong pool");
}

/// A pg_dump carries `set_config('search_path', '')` of its own, and it is
/// replayed on the connection that keeps the pool's books. That connection
/// then could not see an unqualified table again, which surfaced a long way
/// away as somebody else's table having gone missing.
#[tokio::test]
async fn replaying_a_snapshot_leaves_the_search_path_alone() {
    let Some((pool, _)) = ready(1).await else {
        return;
    };

    pool.client()
        .batch_execute("CREATE TABLE IF NOT EXISTS hazir_canary (id int)")
        .await
        .expect("a table in the default schema");

    let dir = std::env::temp_dir().join(format!("hazir-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("somewhere to put it");
    std::fs::write(
        dir.join("001.sql"),
        "CREATE TABLE thing (id bigserial PRIMARY KEY);\n         SELECT pg_catalog.set_config('search_path', '', false);\n",
    )
    .expect("a migration that does what a dump does");

    let recipe = Recipe::Sql(hazir::sql_files(&dir).expect("the file"));
    let fingerprint = format!("{:0<64}", "searchpath");
    hazir::capture(&pool, &fingerprint, &recipe)
        .await
        .expect("a snapshot");
    let schema = pool
        .build_one(&fingerprint, false, false)
        .await
        .expect("a schema from it");

    // Unqualified on purpose: this is the thing that broke.
    let still_there = pool
        .client()
        .query_one("SELECT count(*) FROM hazir_canary", &[])
        .await;

    pool.give_back(&schema, true).await.ok();
    let _ = std::fs::remove_dir_all(&dir);
    still_there.expect("the pool's connection lost its search path");
}

/// The same, for a snapshot that came out of pg_dump rather than out of a
/// file. This is the path a project whose migrations are code takes, and the
/// one where the `set_config` is not the test's own doing but pg_dump's.
#[tokio::test]
async fn replaying_a_dumped_snapshot_leaves_the_search_path_alone() {
    let Some((pool, _)) = ready(1).await else {
        return;
    };

    pool.client()
        .batch_execute("CREATE TABLE IF NOT EXISTS hazir_canary (id int)")
        .await
        .expect("a table in the default schema");

    // What pg_dump writes: every object qualified, and the search path
    // emptied so that nothing can be resolved by accident.
    let fingerprint = format!("{:0<64}", "dumped");
    let ddl = format!(
        "SELECT pg_catalog.set_config('search_path', '', false);\n\
         CREATE TABLE {token}.\"thing\" (\"id\" bigint NOT NULL);\n",
        token = hazir::PLACEHOLDER
    );
    pool.client()
        .execute(
            "INSERT INTO hazir.snapshot (fingerprint, ddl, shape, apply)
             VALUES ($1, $2, '', 'placeholder')
             ON CONFLICT (fingerprint) DO UPDATE SET ddl = EXCLUDED.ddl",
            &[&fingerprint, &ddl],
        )
        .await
        .expect("a snapshot standing in for a dump");

    let schema = pool
        .build_one(&fingerprint, false, false)
        .await
        .expect("a schema built from it");

    let still_there = pool
        .client()
        .query_one("SELECT count(*) FROM hazir_canary", &[])
        .await;

    pool.give_back(&schema, true).await.ok();
    still_there.expect("the pool's connection lost its search path");
}
