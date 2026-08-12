use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use hazir::Pool;

#[derive(Parser)]
#[command(
    name = "hazir",
    about = "Postgres schemas for tests, leased from a warm pool",
    version
)]
struct Cli {
    /// Where the pool lives. Defaults to HAZIR_URL, TEST_DATABASE_URL or
    /// DATABASE_URL, in that order.
    #[arg(long, global = true)]
    url: Option<String>,

    #[command(subcommand)]
    what: What,
}

#[derive(Subcommand)]
enum What {
    /// Build the schema once and stock the pool from it. Run before the tests.
    Warm {
        /// A directory of .sql migrations, applied in name order. Wants
        /// nothing installed and rewrites nothing.
        #[arg(long, conflicts_with = "migrate", required_unless_present = "migrate")]
        sql: Option<PathBuf>,

        /// The command that builds the schema, for migrations that are code
        /// rather than sql. It is given the url to build into as
        /// DATABASE_URL, TEST_DATABASE_URL and HAZIR_URL. Wants pg_dump.
        #[arg(long, conflicts_with = "sql")]
        migrate: Option<String>,

        /// Files or directories whose contents decide whether the snapshot
        /// still stands. The migrations, usually. Defaults to what --sql was
        /// given; required with --migrate.
        #[arg(long = "source")]
        sources: Vec<PathBuf>,

        /// How many schemas to keep ready. Two per test thread is a
        /// comfortable start.
        #[arg(long, default_value_t = 16)]
        pool: usize,

        /// Take the snapshot again even if one for these files is already
        /// there.
        #[arg(long)]
        force: bool,
    },

    /// Take back the schemas whose processes have gone. Run it once, or leave
    /// it running alongside the tests.
    Reclaim {
        /// How long a lease may be held by something this cannot ask about —
        /// another machine, or this one before a restart.
        #[arg(long, default_value_t = 900)]
        stale: u64,

        /// Keep going, this many seconds apart.
        #[arg(long)]
        every: Option<u64>,
    },

    /// What is in the pool.
    Status,

    /// Drop every schema this made, and the snapshots with them.
    Clean,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("hazir: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let pool = match &cli.url {
        Some(url) => Pool::connect(url).await?,
        None => Pool::from_env().await?,
    };
    pool.bootstrap().await?;

    match cli.what {
        What::Warm {
            sql,
            migrate,
            sources,
            pool: want,
            force,
        } => {
            let (recipe, sources) = match (&sql, &migrate) {
                (Some(dir), _) => {
                    let files = hazir::sql_files(dir)?;
                    if files.is_empty() {
                        return Err(format!("no .sql files under {}", dir.display()).into());
                    }
                    let sources = if sources.is_empty() {
                        vec![dir.clone()]
                    } else {
                        sources
                    };
                    (hazir::Recipe::Sql(files), sources)
                }
                (None, Some(command)) => {
                    if sources.is_empty() {
                        return Err("--migrate wants at least one --source: the files \
                                    that decide whether the snapshot still stands"
                            .into());
                    }
                    if !hazir::have_pg_dump().await {
                        return Err("--migrate needs pg_dump on PATH to take the snapshot. \
                                    Install the Postgres client tools, or point --sql at \
                                    a directory of .sql migrations instead"
                            .into());
                    }
                    (hazir::Recipe::Command(command.clone()), sources)
                }
                (None, None) => unreachable!("clap requires one of them"),
            };
            warm(&pool, &recipe, &sources, want, force).await?
        }

        What::Reclaim { stale, every } => {
            let stale = Duration::from_secs(stale);
            loop {
                let taken = pool.reclaim(stale).await?;
                if taken > 0 {
                    println!("took back {taken}");
                }
                match every {
                    Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
                    None => break,
                }
            }
        }

        What::Status => status(&pool).await?,
        What::Clean => clean(&pool).await?,
    }

    Ok(())
}

async fn warm(
    pool: &Pool,
    recipe: &hazir::Recipe,
    sources: &[PathBuf],
    want: usize,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fingerprint = hazir::fingerprint(sources, recipe)?;

    let known: i64 = pool
        .client()
        .query_one(
            "SELECT count(*) FROM hazir.snapshot WHERE fingerprint = $1",
            &[&fingerprint],
        )
        .await?
        .get(0);

    if known == 0 || force {
        println!("building the schema once ({})", &fingerprint[..12]);
        hazir::capture(pool, &fingerprint, recipe).await?;
    } else {
        println!("the schema is unchanged ({})", &fingerprint[..12]);
        pool.client()
            .execute(
                "INSERT INTO hazir.current (sole, fingerprint) VALUES (true, $1)
                 ON CONFLICT (sole) DO UPDATE SET fingerprint = EXCLUDED.fingerprint",
                &[&fingerprint],
            )
            .await?;
    }

    // Before stocking, not after: an earlier run's leftovers are schemas that
    // are already built, and taking them back is free where making their
    // replacements is not.
    let back = pool.reclaim(Duration::from_secs(900)).await?;
    let made = pool.warm(&fingerprint, want).await?;
    println!("pool of {want}: {back} taken back, {made} made");

    // nextest reads this file after a setup script and puts what is in it into
    // the environment of every test it then runs.
    if let Ok(path) = std::env::var("NEXTEST_ENV") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        writeln!(file, "HAZIR_FINGERPRINT={fingerprint}")?;
    }

    Ok(())
}

async fn status(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
    let rows = pool
        .client()
        .query(
            "SELECT fingerprint, state, count(*) FROM hazir.pool
              GROUP BY fingerprint, state ORDER BY fingerprint, state",
            &[],
        )
        .await?;

    if rows.is_empty() {
        println!("nothing warmed here");
        return Ok(());
    }
    for row in rows {
        let fingerprint: String = row.get(0);
        let state: String = row.get(1);
        let count: i64 = row.get(2);
        println!("{:12}  {:6}  {count}", &fingerprint[..12], state);
    }
    Ok(())
}

async fn clean(pool: &Pool) -> Result<(), Box<dyn std::error::Error>> {
    let schemas = pool
        .client()
        .query("SELECT schema_name FROM hazir.pool", &[])
        .await?;

    // A few at a time. Each schema is every table in it, every table is a
    // lock held to the end of the statement, and all of them at once has run
    // a server out of shared memory.
    let names: Vec<String> = schemas.iter().map(|row| row.get(0)).collect();
    for chunk in names.chunks(10) {
        let list: Vec<String> = chunk.iter().map(|name| format!("\"{name}\"")).collect();
        pool.client()
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                list.join(", ")
            ))
            .await?;
    }

    pool.client()
        .batch_execute("DROP SCHEMA hazir CASCADE")
        .await?;
    println!("dropped {} schemas", names.len());
    Ok(())
}
