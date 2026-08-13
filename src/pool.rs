use std::time::Duration;

use tokio_postgres::{Client, NoTls};

use crate::holder::Holder;
use crate::snapshot::Apply;
use crate::{Error, Lease, Result};

/// The token a stored snapshot carries where the schema name goes.
///
/// Public because it is part of what is written into `hazir.snapshot`, and
/// anything that reads or writes one of those rows has to name it.
pub const PLACEHOLDER: &str = "@hazir_schema@";

const BOOTSTRAP: &str = include_str!("sql/bootstrap.sql");

/// How many schemas one `DROP` takes down. Each is every table in it, and
/// every table is a lock held to the end of the statement; all of them at once
/// has run a server out of shared memory.
const SWEEP_AT_A_TIME: usize = 10;

/// An arbitrary number, and only ever compared with itself.
const BOOTSTRAP_LOCK: i64 = 6_174_310_982_745_113;

/// A connection to the database the pool lives in.
///
/// Cheap and short-lived on purpose. Caching one in a static would tie it to
/// whichever runtime happened to build it first, and under `#[tokio::test]`
/// that runtime is gone by the time the second test asks — the client would
/// still look alive and every query on it would hang.
pub struct Pool {
    url: String,
    client: Client,
    /// Asked once. It cannot change while a run is going on, and asking per
    /// lease is a round trip spent learning what is already known.
    fingerprint: tokio::sync::OnceCell<String>,
}

impl Pool {
    pub async fn connect(url: &str) -> Result<Pool> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Pool {
            url: url.to_owned(),
            client,
            fingerprint: tokio::sync::OnceCell::new(),
        })
    }

    pub async fn from_env() -> Result<Pool> {
        Pool::connect(&crate::env_url()?).await
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Makes the tables this crate keeps, if they are not there.
    ///
    /// Behind a lock, and in one transaction, because `IF NOT EXISTS` is not
    /// the same as "safe to run twice at once": two processes reaching this
    /// together get `tuple concurrently updated` out of the catalogue. Two
    /// processes reaching it together is the ordinary case — every test
    /// binary calls it.
    pub async fn bootstrap(&self) -> Result<()> {
        self.client
            .batch_execute(&format!(
                "BEGIN;\nSELECT pg_advisory_xact_lock({BOOTSTRAP_LOCK});\n{BOOTSTRAP}\nCOMMIT;"
            ))
            .await?;
        Ok(())
    }

    /// Which snapshot a lease that names none is given.
    pub async fn current(&self) -> Result<String> {
        self.fingerprint
            .get_or_try_init(|| async {
                if let Ok(named) = std::env::var("HAZIR_FINGERPRINT") {
                    if !named.is_empty() {
                        return Ok(named);
                    }
                }
                let row = self
                    .client
                    .query_opt("SELECT fingerprint FROM hazir.current", &[])
                    .await?;
                row.map(|row| row.get(0)).ok_or(Error::NoSnapshot)
            })
            .await
            .cloned()
    }

    /// Says which snapshot a lease that names none should be given.
    ///
    /// Apart from taking one, because taking a snapshot is a local matter and
    /// this is the only part of it every other run of the suite can see.
    pub async fn set_current(&self, fingerprint: &str) -> Result<()> {
        self.client
            .execute(
                "INSERT INTO hazir.current (sole, fingerprint) VALUES (true, $1)
                 ON CONFLICT (sole) DO UPDATE SET fingerprint = EXCLUDED.fingerprint",
                &[&fingerprint],
            )
            .await?;
        Ok(())
    }

    /// The name of a schema with the tables already in it.
    ///
    /// One round trip in the ordinary case. `SKIP LOCKED` because two
    /// processes reaching for the last ready row is the normal case, not the
    /// rare one, and without it the second waits for the first's transaction
    /// rather than taking the next row along.
    ///
    /// `burn` asks for one of this test's own, dropped rather than recycled
    /// when it comes back. For a test that changes the shape of what it is
    /// given — the tests of a migration, above all. Recycling one of those
    /// would hand the next test a schema that is no longer what the snapshot
    /// says it is.
    pub async fn claim(&self, burn: bool) -> Result<String> {
        let fingerprint = self.current().await?;
        self.claim_from(&fingerprint, burn).await
    }

    /// The same, from a snapshot named rather than looked up.
    ///
    /// A suite with more than one shape of database keeps more than one
    /// snapshot, and each has a pool of its own.
    pub async fn claim_from(&self, fingerprint: &str, burn: bool) -> Result<String> {
        if burn {
            return self.build_one(fingerprint, true, true).await;
        }

        let holder = Holder::me().to_string();
        let claimed = self
            .client
            .query_opt(
                "UPDATE hazir.pool AS p
                    SET state = 'leased', holder = $2, held_since = now()
                  WHERE p.id = (SELECT id FROM hazir.pool
                                 WHERE state = 'ready' AND fingerprint = $1
                                 ORDER BY id
                                   FOR UPDATE SKIP LOCKED
                                 LIMIT 1)
              RETURNING p.schema_name",
                &[&fingerprint, &holder],
            )
            .await?;

        match claimed {
            Some(row) => Ok(row.get(0)),
            None => self.build_one(fingerprint, true, false).await,
        }
    }

    pub async fn lease(&self) -> Result<Lease> {
        Ok(Lease::new(
            self.url.clone(),
            self.claim(false).await?,
            false,
        ))
    }

    pub async fn lease_fresh(&self) -> Result<Lease> {
        Ok(Lease::new(self.url.clone(), self.claim(true).await?, true))
    }

    /// Brings the number of schemas for a snapshot up to `want`.
    ///
    /// Returns how many it had to make.
    pub async fn warm(&self, fingerprint: &str, want: usize) -> Result<usize> {
        let have: i64 = self
            .client
            .query_one(
                "SELECT count(*) FROM hazir.pool WHERE fingerprint = $1 AND NOT burn",
                &[&fingerprint],
            )
            .await?
            .get(0);

        let short = want.saturating_sub(have.max(0) as usize);
        for _ in 0..short {
            self.build_one(fingerprint, false, false).await?;
        }
        Ok(short)
    }

    /// Throws away every snapshot but the ones named, and the schemas built
    /// from them.
    ///
    /// A suite whose fingerprint moves with its own source — which is the
    /// safe way to fingerprint, because a list of the files that matter is a
    /// list that goes silently out of date — leaves a snapshot behind on
    /// every change. A fortnight of those is a fortnight of schemas, which is
    /// the mess this crate exists to prevent arriving by another door.
    ///
    /// Returns how many schemas were dropped.
    pub async fn forget(&self, keep: &[String]) -> Result<usize> {
        let mut going: Vec<String> = self
            .client
            .query(
                "SELECT schema_name FROM hazir.pool WHERE fingerprint <> ALL($1)",
                &[&keep],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let dropped = going.len();

        going.extend(
            self.client
                .query(
                    "SELECT seed FROM hazir.snapshot
                      WHERE fingerprint <> ALL($1) AND seed IS NOT NULL",
                    &[&keep],
                )
                .await?
                .iter()
                .map(|row| row.get::<_, String>(0)),
        );

        // A few at a time: each schema is every table in it, and every table
        // is a lock held to the end of the statement.
        for some in going.chunks(SWEEP_AT_A_TIME) {
            let names: Vec<String> = some.iter().map(|name| quote(name)).collect();
            self.client
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    names.join(", ")
                ))
                .await?;
        }

        self.client
            .execute(
                "DELETE FROM hazir.pool WHERE fingerprint <> ALL($1)",
                &[&keep],
            )
            .await?;
        self.client
            .execute(
                "DELETE FROM hazir.snapshot WHERE fingerprint <> ALL($1)",
                &[&keep],
            )
            .await?;

        Ok(dropped)
    }

    /// Takes back what the processes that had them are no longer alive to use.
    ///
    /// Returns how many came back.
    pub async fn reclaim(&self, stale_after: Duration) -> Result<usize> {
        let held = self
            .client
            .query(
                "SELECT schema_name, holder, burn,
                        held_since < now() - make_interval(secs => $1) AS overdue
                   FROM hazir.pool
                  WHERE state = 'leased'",
                &[&stale_after.as_secs_f64().max(1.0)],
            )
            .await?;

        let mut taken = 0;
        for row in held {
            let schema: String = row.get(0);
            let holder: Option<String> = row.get(1);
            let burn: bool = row.get(2);
            let overdue: Option<bool> = row.get(3);

            let finished = match holder.as_deref().and_then(Holder::parse) {
                Some(who) => who.gone().unwrap_or_else(|| overdue.unwrap_or(false)),
                // A claim this cannot read is not one it wrote. Only its age
                // can speak for it.
                None => overdue.unwrap_or(false),
            };

            if finished {
                self.give_back(&schema, burn).await?;
                taken += 1;
            }
        }
        Ok(taken)
    }

    /// A schema returning to the pool.
    ///
    /// Emptied rather than dropped — see `hazir.wipe`. Unless its shape has
    /// moved away from the snapshot's, in which case it is not the thing the
    /// next test asked for and there is nothing to do but throw it away.
    pub async fn give_back(&self, schema: &str, burn: bool) -> Result<()> {
        if burn || !self.still_the_right_shape(schema).await? {
            self.client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote(schema)))
                .await?;
            self.client
                .execute("DELETE FROM hazir.pool WHERE schema_name = $1", &[&schema])
                .await?;
            return Ok(());
        }

        self.client
            .execute("SELECT hazir.wipe($1)", &[&schema])
            .await?;
        // Emptying took out what the migrations wrote as well as what the
        // test did. Only the second of those was the test's to lose.
        self.client
            .execute(
                "SELECT hazir.reseed($1, (SELECT s.seed FROM hazir.pool p
                                            JOIN hazir.snapshot s
                                              ON s.fingerprint = p.fingerprint
                                           WHERE p.schema_name = $1))",
                &[&schema],
            )
            .await?;
        self.client
            .execute(
                "UPDATE hazir.pool
                    SET state = 'ready', holder = NULL, held_since = NULL
                  WHERE schema_name = $1",
                &[&schema],
            )
            .await?;
        Ok(())
    }

    async fn still_the_right_shape(&self, schema: &str) -> Result<bool> {
        let row = self
            .client
            .query_opt(
                "SELECT s.shape = hazir.shape(p.schema_name)
                   FROM hazir.pool p
                   JOIN hazir.snapshot s ON s.fingerprint = p.fingerprint
                  WHERE p.schema_name = $1",
                &[&schema],
            )
            .await?;
        Ok(row
            .and_then(|row| row.get::<_, Option<bool>>(0))
            .unwrap_or(false))
    }

    /// Builds one schema from a stored snapshot and records it.
    pub async fn build_one(&self, fingerprint: &str, leased: bool, burn: bool) -> Result<String> {
        let row = self
            .client
            .query_opt(
                "SELECT ddl, apply FROM hazir.snapshot WHERE fingerprint = $1",
                &[&fingerprint],
            )
            .await?
            .ok_or_else(|| Error::UnknownSnapshot(fingerprint.to_owned()))?;
        let ddl: String = row.get(0);
        let apply = Apply::from_name(row.get(1));

        // Nothing is reseeded here: the ddl carries the rows the migrations
        // wrote, whether it is sql somebody wrote or a dump taken with
        // --inserts. Putting them in twice is a duplicate key. Restoring is
        // for the schema that comes back emptied — see `give_back`.
        let schema = name_for(fingerprint);
        self.client
            .batch_execute(&format!("CREATE SCHEMA {}", quote(&schema)))
            .await?;
        if let Err(err) = self
            .client
            .batch_execute(&apply.statements(&ddl, &schema))
            .await
        {
            // The search path is this connection's, not this statement's, and
            // a snapshot that failed half way through has left it somewhere.
            let _ = self.client.batch_execute("RESET search_path").await;
            let _ = self
                .client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote(&schema)))
                .await;
            return Err(err.into());
        }

        let holder = leased.then(|| Holder::me().to_string());
        self.client
            .execute(
                "INSERT INTO hazir.pool (schema_name, fingerprint, state, burn, holder, held_since)
                 VALUES ($1, $2, $3, $4, $5::text,
                         CASE WHEN $5::text IS NULL THEN NULL ELSE now() END)",
                &[
                    &schema,
                    &fingerprint,
                    &if leased { "leased" } else { "ready" },
                    &burn,
                    &holder,
                ],
            )
            .await?;

        Ok(schema)
    }
}

/// A name no two of these ever share.
///
/// The tail of the clock rather than the head: the leading digits of the
/// millisecond are the same for every schema a warm-up makes, and it makes
/// them faster than the clock moves.
pub(crate) fn name_for(fingerprint: &str) -> String {
    scratch_name("pool", fingerprint)
}

/// The same, for the schemas built and thrown away while taking a snapshot.
///
/// Unique per process rather than derived from the fingerprint alone: two
/// runs warming the same database at once would otherwise pick the same name
/// and the second would fail on a schema the first was still using.
pub(crate) fn scratch_name(kind: &str, fingerprint: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);

    format!(
        "hazir_{kind}_{}_{}_{:x}{:x}",
        &fingerprint[..8.min(fingerprint.len())],
        std::process::id(),
        nanos,
        n
    )
}

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{name_for, quote};

    #[test]
    fn two_names_asked_for_together_differ() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(name_for("abcdef0123456789")));
        }
    }

    #[test]
    fn a_name_is_a_legal_identifier_to_start_with() {
        let name = name_for("abcdef0123456789");
        assert!(name.starts_with("hazir_"));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(name.len() < 64);
    }

    #[test]
    fn a_quote_inside_a_name_cannot_end_the_name() {
        assert_eq!(quote(r#"od"d"#), r#""od""d""#);
    }
}
