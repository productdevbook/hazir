# hazır

A test asks for a Postgres database and is given one that already exists.

The usual arrangement builds one per test: create a schema, run every
migration into it, drop it afterwards. That is a build step, and putting a
build step inside a test means running it once per test — forty migrations,
four hundred times.

It also gets slower as it goes. Creating and dropping tables writes to
Postgres's own catalogue harder than the tests write to their own tables, and
autovacuum spends the run chasing it. A suite that starts brisk is crawling by
the end, and nothing in the code explains why.

`hazır` does neither.

- **The migrations run once.** The result is kept as a snapshot addressed by
  the hash of the files that produced it. Change a migration and it is rebuilt;
  change anything else and it is not.
- **A test leases a schema rather than building one.** One statement,
  `FOR UPDATE SKIP LOCKED`, about a millisecond.
- **A schema comes back emptied, not dropped.** `TRUNCATE` every table in one
  statement. The catalogue never grows, so the last test in a run costs what
  the first one did.
- **Nothing depends on a test cleaning up.** A lease records the machine, the
  boot and the process holding it; `hazir reclaim` takes back what is held by
  processes that have finished. The tests that break are exactly the ones that
  would forget to tidy up, so they are not asked to.

The pool lives in the database rather than in a process. That is what makes it
work under `cargo nextest`, which gives every test its own process and so
leaves nothing in memory for the next one to find.

## Using it

```toml
[dev-dependencies]
hazir = "0.1"
```

```rust
#[tokio::test]
async fn a_site_can_be_written_to() {
    let db = hazir::lease().await.unwrap();
    let pool = sqlx::PgPool::connect(db.url()).await.unwrap();
    // ... the test ...
}
```

`db.url()` is an ordinary connection string with `search_path` set to the
leased schema and nothing after it — so sqlx, SeaORM, Diesel and
tokio-postgres all take it as they are. Nothing here needs to know which one
you use.

Nothing after it is deliberate: a search path with `public` still on the end
is what lets a test read a table it never made and pass for the wrong reason.

### Stocking the pool

Once, before the tests:

```sh
cargo install hazir --features cli

# migrations that are .sql files
hazir warm --sql migrations/ --pool 16

# migrations that are code
hazir warm --migrate "cargo run -p migration" --source migration/src --pool 16
```

`--sql` needs nothing installed: hand-written sql names no schema, so it can
be pointed at one and applied exactly as written.

`--migrate` runs your migrator once against a template and keeps `pg_dump`'s
output, so it wants the Postgres client tools on the machine that warms —
once per change to the migrations, and never during a test run.

Where the pool lives comes from `HAZIR_URL`, `TEST_DATABASE_URL` or
`DATABASE_URL`, in that order. There is no default: a url guessed here would
either fail to connect, which reads as this crate being broken rather than the
machine being unconfigured, or connect to something somebody was using.

### With nextest

```toml
# .config/nextest.toml

# Setup scripts are still experimental in nextest, and it refuses the file
# without this line.
experimental = ["setup-scripts"]

[scripts.setup.hazir]
command = 'hazir warm --sql migrations/ --pool 16'

[[profile.default.scripts]]
filter = 'all()'
setup = 'hazir'
```

The setup script runs once for the whole run and hands every test the
snapshot it warmed, through `NEXTEST_ENV`. This repository's own
`.config/nextest.toml` does exactly that, against its own fixtures.

This is the arrangement `hazır` exists for. nextest runs every test in a
process of its own, which is what makes a leaked test cheap to contain — and
also what makes an in-process pool worthless, because each process starts
with an empty one. Putting the pool in the database is what survives it.

### Tests that change the schema

A test that alters what it was given — the tests of a migration, above all —
should ask for a schema of its own:

```rust
let db = hazir::lease_fresh().await.unwrap();
```

That one is dropped afterwards rather than passed on. And if an ordinary lease
comes back altered anyway, it is noticed: what a schema looks like is recorded
when the snapshot is taken, and compared on the way back. A schema that no
longer matches is thrown away rather than handed to the next test looking
clean.

### Taking back what was abandoned

```sh
hazir reclaim --every 30 &
```

Optional. Leases are returned when they are dropped; this is for the ones that
never were — a test that panicked, a run that was killed, a machine that went
away. It is also run at the start of every `hazir warm`, so a suite that warms
before each run needs nothing else.

## What it costs

Measured against a Postgres 18 in a container on one developer machine, with
a fixture of two tables — `cargo test --test cost -- --ignored --nocapture`
prints these for yours:

| | |
|---|---|
| claiming a ready schema | 0.7 ms |
| giving one back | 6.8 ms |
| both, as a test sees it | 7.6 ms |
| building one from the snapshot instead | 14 ms |
| opening a connection | 85 ms |

Two things worth reading out of that.

The first is that even against two tables, leasing beats building. The
schemas this was written for have sixty, and are reached through forty-odd
migrations; there the comparison is milliseconds against seconds.

The second is the last row. Opening a connection costs more than everything
else together — it is almost all SCRAM's key derivation, which is slow on
purpose. So this process opens exactly one, on a thread of its own, and every
lease goes through it. A version that opened one per lease measured 96 ms and
was slower than having no pool at all.

## What it does not do

- **One Postgres, many schemas.** If your application cannot live inside one
  schema — it installs extensions per database, or names `public` explicitly —
  this is not for you.
- **`--migrate` wants `pg_dump`.** Only when warming, never when testing.
- **Sequences that no table owns are not reset** by the wipe. Tables and their
  own sequences are.
- **`hazir` writes to a schema called `hazir`** in the database you point it
  at. Point it at a test database.

## Why not the ones that exist

`sqlx::test` does keep a pool of databases from a template, and it is good.
It is also sqlx's, it works a database at a time rather than a schema at a
time, and its pool lives in the test process — which a runner like nextest
gives away for free at the end of every test. `testcontainers` starts a
Postgres per run. `pgtemp` starts one per test.

None of them caches the migrations by content, none of them recycles instead
of dropping, and none of them survives a runner that will not let two tests
share a process.

## Licence

MIT.
