use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::pool::{quote, scratch_name, Pool, PLACEHOLDER};
use crate::{Error, Result};

/// How the tables get made.
#[derive(Debug, Clone)]
pub enum Recipe {
    /// Plain `.sql` files, applied in name order.
    ///
    /// The quick path, and the one with nothing outside this process in it:
    /// sql written by hand names no schema, so it can be pointed at one with
    /// `search_path` and applied exactly as written. Nothing is dumped and
    /// nothing is rewritten, so there is nothing to get subtly wrong.
    Sql(Vec<PathBuf>),

    /// Any migrator at all, run once against a template and dumped.
    ///
    /// For the projects whose migrations are code — SeaORM, Diesel, refinery.
    /// It wants `pg_dump`; without one the command is run again for every
    /// schema in the pool instead, which is slower but asks nothing of the
    /// machine.
    Command(String),
}

/// A schema, built once, kept as the text that rebuilds it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub fingerprint: String,
    pub ddl: String,
    pub shape: String,
    pub apply: Apply,
}

/// What has to happen to a snapshot's text before it will build a schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// Point the session at the schema and run it as written.
    SearchPath,
    /// Swap the token for the schema's name.
    Placeholder,
}

impl Apply {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Apply::SearchPath => "search_path",
            Apply::Placeholder => "placeholder",
        }
    }

    pub(crate) fn from_name(name: &str) -> Apply {
        match name {
            "placeholder" => Apply::Placeholder,
            _ => Apply::SearchPath,
        }
    }

    /// The text that builds one schema.
    ///
    /// The search path is put back afterwards because the connection running
    /// this is the one that goes on to keep the pool's own books, and leaving
    /// it pointed at somebody's test schema is how those end up written
    /// somewhere nobody looks.
    pub(crate) fn statements(self, ddl: &str, schema: &str) -> String {
        match self {
            Apply::SearchPath => {
                format!(
                    "SET search_path TO {};\n{ddl}\n;RESET search_path;",
                    quote(schema)
                )
            }
            Apply::Placeholder => ddl.replace(PLACEHOLDER, &quote(schema)),
        }
    }
}

/// What the schema is built from, hashed.
///
/// The migration sources and the recipe that runs them, because a snapshot is
/// only reusable while both are what made it. Anything else changing — this
/// project's own code, its tests — leaves the schema alone and must not throw
/// the pool away.
pub fn fingerprint(sources: &[PathBuf], recipe: &Recipe) -> Result<String> {
    let mut files = Vec::new();
    for source in sources {
        collect(source, &mut files)?;
    }
    files.sort();

    let mut hash = Sha256::new();
    match recipe {
        Recipe::Sql(_) => hash.update(b"sql"),
        Recipe::Command(command) => {
            hash.update(b"command");
            hash.update(command.as_bytes());
        }
    }
    hash.update([0]);
    for file in &files {
        hash.update(file.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update(std::fs::read(file)?);
        hash.update([0]);
    }
    Ok(hex(&hash.finalize()))
}

/// Every `.sql` file under a path, in the order they should be applied.
pub fn sql_files(at: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect(at, &mut files)?;
    files.retain(|file| file.extension().is_some_and(|ext| ext == "sql"));
    files.sort();
    Ok(files)
}

fn collect(at: &Path, into: &mut Vec<PathBuf>) -> Result<()> {
    if at.is_file() {
        into.push(at.to_path_buf());
        return Ok(());
    }
    if !at.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(at)? {
        let path = entry?.path();
        // Nothing a build wrote: it is derived from what is already hashed,
        // and it changes on every build.
        if path.file_name().is_some_and(|name| name == "target") {
            continue;
        }
        collect(&path, into)?;
    }
    Ok(())
}

/// Builds the schema once and keeps the result.
///
/// Storing a snapshot does not make it the one a test gets. Deciding that is
/// `set_current`, and it is separate because it is the only part of this that
/// every other run of the suite can see.
pub async fn capture(pool: &Pool, fingerprint: &str, recipe: &Recipe) -> Result<Snapshot> {
    let (ddl, apply) = match recipe {
        Recipe::Sql(files) => (read_all(files)?, Apply::SearchPath),
        Recipe::Command(command) => (
            build_and_dump(pool, fingerprint, command).await?,
            Apply::Placeholder,
        ),
    };

    // Built once more, into a schema of its own, purely to record what the
    // result looks like. That record is what tells a returning schema apart
    // from one a test has altered, and it has to come from the snapshot
    // rather than from the migrator — otherwise a dump that loses something
    // would go unnoticed, because the shape it is compared against would have
    // lost it too.
    let template = scratch_name("shape", fingerprint);
    pool.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote(&template)
        ))
        .await?;
    pool.client()
        .batch_execute(&format!("CREATE SCHEMA {}", quote(&template)))
        .await?;
    pool.client()
        .batch_execute(&apply.statements(&ddl, &template))
        .await?;
    let shape: String = pool
        .client()
        .query_one("SELECT hazir.shape($1)", &[&template])
        .await?
        .get(0);
    pool.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote(&template)
        ))
        .await?;

    pool.client()
        .execute(
            "INSERT INTO hazir.snapshot (fingerprint, ddl, shape, apply)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (fingerprint) DO UPDATE
                SET ddl = EXCLUDED.ddl, shape = EXCLUDED.shape, apply = EXCLUDED.apply",
            &[&fingerprint, &ddl, &shape, &apply.name()],
        )
        .await?;
    Ok(Snapshot {
        fingerprint: fingerprint.to_owned(),
        ddl,
        shape,
        apply,
    })
}

fn read_all(files: &[PathBuf]) -> Result<String> {
    let mut text = String::new();
    for file in files {
        text.push_str(&std::fs::read_to_string(file)?);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(";\n");
    }
    Ok(text)
}

async fn build_and_dump(pool: &Pool, fingerprint: &str, command: &str) -> Result<String> {
    let template = scratch_name("template", fingerprint);

    pool.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote(&template)
        ))
        .await?;
    pool.client()
        .batch_execute(&format!("CREATE SCHEMA {}", quote(&template)))
        .await?;

    run(
        command,
        &crate::url::with_search_path(pool.url(), &template),
        &template,
    )
    .await?;
    let dumped = dump(pool.url(), &template).await?;

    pool.client()
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote(&template)
        ))
        .await?;

    Ok(dumped)
}

/// Whatever builds the schema, pointed at one schema and nothing else.
///
/// The url is handed over under three names because a migrator reads whichever
/// one its project happens to use, and being told to set a fourth is how a
/// migration ends up running against the wrong database.
pub(crate) async fn run(command: &str, scoped_url: &str, schema: &str) -> Result<()> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("DATABASE_URL", scoped_url)
        .env("TEST_DATABASE_URL", scoped_url)
        .env("HAZIR_URL", scoped_url)
        .env("HAZIR_SCHEMA", schema)
        .output()
        .await?;

    if !output.status.success() {
        return Err(Error::Migrate(format!(
            "`{command}` exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Which `pg_dump` to run.
///
/// `HAZIR_PG_DUMP` because the one on PATH is often the wrong one and often
/// not replaceable: pg_dump refuses a server newer than itself, and a distro
/// that packages 16 against a server running 18 leaves nothing to do but say
/// where the right one is.
fn pg_dump() -> String {
    std::env::var("HAZIR_PG_DUMP")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| "pg_dump".to_owned())
}

/// Whether the machine has the one external tool any of this wants.
pub async fn have_pg_dump() -> bool {
    tokio::process::Command::new(pg_dump())
        .arg("--version")
        .output()
        .await
        .is_ok_and(|out| out.status.success())
}

/// The schema as text, with its own name taken out of it.
///
/// The name is replaced by a token rather than by anything clever, and the
/// token is what every new schema substitutes itself into. It is a long,
/// generated identifier, so a literal replacement of it cannot hit anything
/// else in the dump.
async fn dump(url: &str, template: &str) -> Result<String> {
    let output = tokio::process::Command::new(pg_dump())
        .arg("--dbname")
        .arg(url)
        .arg("--schema-only")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--no-comments")
        .arg("--quote-all-identifiers")
        .arg("--schema")
        .arg(template)
        .output()
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Error::Dump(
                "not on PATH. It comes with the Postgres client tools; without it \
                 `--migrate` is run once per schema instead of once in total"
                    .to_owned(),
            ),
            _ => Error::Io(err),
        })?;

    if !output.status.success() {
        return Err(Error::Dump(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }

    Ok(tidy(&String::from_utf8_lossy(&output.stdout), template))
}

/// What comes back from pg_dump, made into something a driver can replay.
///
/// A pure function because the two things it has to get right are both easy
/// to get wrong and impossible to notice: it is only ever run against a real
/// pg_dump, and a dump that fails to replay says "syntax error" pointing at a
/// character rather than at a reason.
fn tidy(dumped: &str, template: &str) -> String {
    let quoted = quote(template);

    dumped
        .lines()
        // The dump makes the schema; whoever replays this has already made it.
        //
        // At the start of the line rather than anywhere in it, and the same
        // for the backslashes below: pg_dump writes its own statements at
        // column zero, and a dollar-quoted function body is indented. Being
        // lenient here would quietly delete a line out of somebody's trigger.
        .filter(|line| !line.starts_with("CREATE SCHEMA"))
        // `\restrict` and `\unrestrict`, which Postgres 18's pg_dump wraps
        // every dump in. They are psql's own, not the server's, and a driver
        // handed one reports a syntax error at a backslash and nothing else.
        .filter(|line| !line.starts_with('\\'))
        .map(|line| {
            line.replace(&quoted, PLACEHOLDER)
                .replace(template, PLACEHOLDER)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, hex, Apply, Recipe};
    use std::path::PathBuf;

    #[test]
    fn the_recipe_is_part_of_what_is_hashed() {
        let none: Vec<PathBuf> = vec![];
        assert_ne!(
            fingerprint(&none, &Recipe::Command("one".into())).unwrap(),
            fingerprint(&none, &Recipe::Command("another".into())).unwrap()
        );
        assert_ne!(
            fingerprint(&none, &Recipe::Sql(vec![])).unwrap(),
            fingerprint(&none, &Recipe::Command("sql".into())).unwrap()
        );
    }

    #[test]
    fn the_same_input_hashes_the_same_way_twice() {
        let sources = vec![PathBuf::from("src")];
        let recipe = Recipe::Command("x".into());
        assert_eq!(
            fingerprint(&sources, &recipe).unwrap(),
            fingerprint(&sources, &recipe).unwrap()
        );
    }

    /// What a build wrote is derived from what is already hashed, and hashing
    /// it too would throw the pool away on every build.
    #[test]
    fn a_target_directory_is_not_looked_at() {
        let dir = std::env::temp_dir().join(format!("hazir-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.sql"), "create table a ();").unwrap();
        let recipe = Recipe::Command("x".into());

        let before = fingerprint(std::slice::from_ref(&dir), &recipe).unwrap();
        std::fs::write(dir.join("target/rubbish"), "anything").unwrap();
        let after = fingerprint(std::slice::from_ref(&dir), &recipe).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(before, after);
    }

    #[test]
    fn sql_is_pointed_at_a_schema_rather_than_rewritten() {
        let built = Apply::SearchPath.statements("CREATE TABLE a ();", "s");
        assert!(built.starts_with("SET search_path TO \"s\";"));
        assert!(built.contains("CREATE TABLE a ();"));
        assert!(built.trim_end().ends_with("RESET search_path;"));
    }

    #[test]
    fn a_dump_has_its_token_swapped() {
        let built = Apply::Placeholder.statements("CREATE TABLE @hazir_schema@.\"a\" ();", "s");
        assert_eq!(built, "CREATE TABLE \"s\".\"a\" ();");
    }

    #[test]
    fn how_to_apply_survives_being_written_down() {
        for one in [Apply::SearchPath, Apply::Placeholder] {
            assert_eq!(Apply::from_name(one.name()), one);
        }
    }

    /// The dump a Postgres 18 actually produces, in miniature.
    #[test]
    fn what_pg_dump_wraps_a_dump_in_is_taken_back_off() {
        let dumped = concat!(
            "--\n",
            "-- PostgreSQL database dump\n",
            "--\n",
            "\n",
            "\\restrict RZzJ6DyJpEzvTvMo3Hn18oVUtwXfFskKfFDdZOokpFS\n",
            "\n",
            "SET statement_timeout = 0;\n",
            "CREATE SCHEMA \"hazir_template_abc\";\n",
            "CREATE TABLE \"hazir_template_abc\".\"site\" (\n",
            "    \"id\" bigint NOT NULL\n",
            ");\n",
            "\n",
            "\\unrestrict RZzJ6DyJpEzvTvMo3Hn18oVUtwXfFskKfFDdZOokpFS\n",
        );

        let tidied = super::tidy(dumped, "hazir_template_abc");

        assert!(!tidied.contains("restrict"), "{tidied}");
        assert!(!tidied.contains("CREATE SCHEMA"), "{tidied}");
        assert!(!tidied.contains("hazir_template_abc"), "{tidied}");
        assert!(
            tidied.contains("CREATE TABLE @hazir_schema@.\"site\""),
            "{tidied}"
        );
        assert!(tidied.contains("SET statement_timeout = 0;"), "{tidied}");
    }

    /// An indented backslash is somebody's function body, not psql's.
    #[test]
    fn a_backslash_inside_a_statement_is_left_alone() {
        let dumped = concat!(
            "CREATE FUNCTION \"hazir_template_abc\".\"f\"() RETURNS text AS $$\n",
            "    \\ this line belongs to the body\n",
            "$$ LANGUAGE sql;\n",
        );
        let tidied = super::tidy(dumped, "hazir_template_abc");
        assert!(tidied.contains("belongs to the body"), "{tidied}");
        assert!(tidied.contains("$$ LANGUAGE sql;"), "{tidied}");
    }

    #[test]
    fn hex_is_two_characters_a_byte() {
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    }
}
