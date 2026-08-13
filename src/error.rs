use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Nowhere to connect. `HAZIR_URL` or `TEST_DATABASE_URL` says where.
    NoUrl,
    /// Nothing has been warmed into this database yet.
    NoSnapshot,
    /// A snapshot was named that this database has never been given.
    UnknownSnapshot(String),
    /// The pool was empty and a schema could not be built to replace it.
    Exhausted,
    /// The database this process leases from did not answer at all.
    Unreachable(String),
    Postgres(tokio_postgres::Error),
    Io(std::io::Error),
    /// The command that builds the schema said no.
    Migrate(String),
    /// `pg_dump` is not on PATH, or it refused.
    Dump(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoUrl => write!(
                f,
                "no database to lease from. HAZIR_URL or TEST_DATABASE_URL says where"
            ),
            Error::NoSnapshot => write!(
                f,
                "nothing warmed here yet — run `hazir warm` before the tests"
            ),
            Error::UnknownSnapshot(fp) => write!(f, "no snapshot called {fp} in this database"),
            Error::Exhausted => write!(f, "the pool is empty and a schema could not be built"),
            Error::Unreachable(why) => write!(f, "the pool's database did not answer: {why}"),
            // tokio-postgres prints "db error" and puts everything the
            // server actually said behind `as_db_error`. Passing that through
            // is the difference between a message and a shrug.
            Error::Postgres(err) => match err.as_db_error() {
                Some(said) => {
                    write!(f, "postgres: {}", said.message())?;
                    if let Some(detail) = said.detail() {
                        write!(f, " ({detail})")?;
                    }
                    if let Some(hint) = said.hint() {
                        write!(f, " — {hint}")?;
                    }
                    Ok(())
                }
                None => write!(f, "postgres: {err}"),
            },
            Error::Io(err) => write!(f, "{err}"),
            Error::Migrate(what) => write!(f, "the schema could not be built: {what}"),
            Error::Dump(what) => write!(f, "pg_dump: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Postgres(err) => Some(err),
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(err: tokio_postgres::Error) -> Self {
        Error::Postgres(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}
