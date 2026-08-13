//! A test asks for a database and is given one that already exists.
//!
//! The usual arrangement builds one per test: create a schema, run every
//! migration into it, drop it afterwards. That is a build step, and putting a
//! build step inside a test means running it once per test — forty
//! migrations, four hundred times. It also gets slower as it goes, because
//! creating and dropping tables writes to Postgres's own catalogue harder
//! than the tests write to their tables, and autovacuum spends the run
//! chasing it.
//!
//! So this does neither. The migrations run once, into a snapshot addressed
//! by the hash of the files that produced it. A warm pool of schemas is built
//! from that snapshot ahead of the run. A test leases one — a single
//! statement, `FOR UPDATE SKIP LOCKED` — and gives it back emptied rather
//! than dropped, so the catalogue never grows.
//!
//! The pool lives in the database rather than in a process, which is what
//! makes it work under `cargo nextest`: a runner that gives every test its
//! own process leaves nothing in memory for the next one to find.
//!
//! ```text
//! let db = hazir::lease().await?;
//! let pool = sqlx::PgPool::connect(db.url()).await?;   // or SeaORM, or Diesel
//! // ... the test ...
//! ```
//!
//! The url is a plain connection string with `search_path` set on it, so
//! nothing here has to know which client is being used.
//!
//! Before the run, once:
//!
//! ```text
//! hazir warm --pool 16 --migrate "cargo run --quiet -p migration"
//! ```

mod agent;
mod error;
mod holder;
mod lease;
mod pool;
mod snapshot;
mod url;

pub use error::Error;
pub use holder::Holder;
pub use lease::Lease;
pub use pool::Pool;
pub use snapshot::{capture, fingerprint, have_pg_dump, sql_files, Apply, Recipe, Snapshot};

pub type Result<T> = std::result::Result<T, Error>;

/// A schema with the tables already in it, emptied and passed on afterwards.
pub async fn lease() -> Result<Lease> {
    let taken = agent::ask(None, false).await?;
    Ok(Lease::new(taken.url, taken.schema, false))
}

/// The same, from a snapshot named rather than looked up.
///
/// For a suite that keeps more than one shape of database. Only one of them
/// can be what `lease` gives, and a test handed the wrong one fails a long
/// way from the reason.
pub async fn lease_from(fingerprint: &str) -> Result<Lease> {
    let taken = agent::ask(Some(fingerprint.to_owned()), false).await?;
    Ok(Lease::new(taken.url, taken.schema, false))
}

/// A schema of this test's own, out of a named snapshot, thrown away after.
pub async fn lease_fresh_from(fingerprint: &str) -> Result<Lease> {
    let taken = agent::ask(Some(fingerprint.to_owned()), true).await?;
    Ok(Lease::new(taken.url, taken.schema, true))
}

/// A schema of this test's own, thrown away afterwards.
///
/// For a test that changes the shape of what it is given rather than only its
/// rows — the tests of a migration, above all.
pub async fn lease_fresh() -> Result<Lease> {
    let taken = agent::ask(None, true).await?;
    Ok(Lease::new(taken.url, taken.schema, true))
}

/// Where the pool is.
///
/// `TEST_DATABASE_URL` as well as this crate's own name, because that is what
/// most suites already set and asking them to set a second one that means the
/// same thing is how the two drift apart.
pub(crate) fn env_url() -> Result<String> {
    for name in ["HAZIR_URL", "TEST_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(url) = std::env::var(name) {
            if !url.trim().is_empty() {
                return Ok(url);
            }
        }
    }
    Err(Error::NoUrl)
}
