//! What a lease costs, on whatever `HAZIR_URL` points at.
//!
//! Ignored by default: it is a measurement rather than a claim, and what it
//! measures is the machine as much as the code. `cargo test --test cost --
//! --ignored --nocapture` prints it.

use std::path::PathBuf;
use std::time::Instant;

use hazir::{Pool, Recipe};

#[tokio::test]
#[ignore = "a measurement, not an assertion"]
async fn what_a_lease_costs() {
    let Ok(url) = std::env::var("HAZIR_URL") else {
        println!("HAZIR_URL says where; nothing to measure without it");
        return;
    };

    let pool = Pool::connect(&url).await.expect("a Postgres that answers");
    pool.bootstrap().await.expect("the pool's own tables");

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/migrations");
    let recipe = Recipe::Sql(hazir::sql_files(&dir).expect("the fixture migrations"));
    let fingerprint = hazir::fingerprint(&[dir], &recipe).expect("a fingerprint");
    hazir::capture(&pool, &fingerprint, &recipe)
        .await
        .expect("a snapshot");
    pool.warm(&fingerprint, 140).await.expect("a warm pool");

    let rounds = 100;

    let started = Instant::now();
    for _ in 0..rounds {
        drop(Pool::connect(&url).await.expect("a connection"));
    }
    println!("opening a connection:  {:?}", started.elapsed() / rounds);

    // A pool bigger than this loop, so that what is measured is claiming a
    // ready schema rather than quietly building new ones once it runs dry.
    let mut names = Vec::new();
    let started = Instant::now();
    for _ in 0..rounds {
        names.push(pool.claim(false).await.expect("a schema"));
    }
    println!("claiming a schema:     {:?}", started.elapsed() / rounds);

    let started = Instant::now();
    for name in &names {
        pool.give_back(name, false).await.expect("giving it back");
    }
    println!("giving one back:       {:?}", started.elapsed() / rounds);

    // End to end, the way a test sees it: through the process's one agent,
    // including handing it back on drop.
    let started = Instant::now();
    for _ in 0..rounds {
        let _lease = hazir::lease().await.expect("a schema");
    }
    println!("lease and give back:   {:?}", started.elapsed() / rounds);

    let started = Instant::now();
    let built = pool
        .build_one(&fingerprint, false, false)
        .await
        .expect("a schema built from the snapshot");
    println!("building one instead:  {:?}", started.elapsed());
    pool.give_back(&built, true).await.expect("tidying up");
}
