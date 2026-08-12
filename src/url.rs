//! Pointing a connection string at one schema.
//!
//! Through libpq's `options`, rather than through anything a particular
//! driver understands, so that what a lease hands back is a url every Rust
//! Postgres client already knows how to open: sqlx, SeaORM, Diesel,
//! tokio-postgres. Nothing here has to know which one is being used.

/// The same url, with `search_path` set to one schema and nothing after it.
///
/// Nothing after it on purpose: a search path with `public` still on the end
/// is what lets a test read a table it never made and pass for the wrong
/// reason.
pub fn with_search_path(url: &str, schema: &str) -> String {
    let value = format!("-csearch_path%3D{}", encode(schema));
    match url.split_once('?') {
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| !pair.starts_with("options="))
                .filter(|pair| !pair.is_empty())
                .collect();
            if kept.is_empty() {
                format!("{base}?options={value}")
            } else {
                format!("{base}?{}&options={value}", kept.join("&"))
            }
        }
        None => format!("{url}?options={value}"),
    }
}

/// Percent-encoding for the few characters a schema name may carry that would
/// otherwise end the option or the query.
fn encode(schema: &str) -> String {
    schema
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '.' | '~' => c.to_string(),
            other => other
                .to_string()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::with_search_path;

    #[test]
    fn a_plain_url_gains_the_option() {
        assert_eq!(
            with_search_path("postgres://u@h/db", "test_1"),
            "postgres://u@h/db?options=-csearch_path%3Dtest_1"
        );
    }

    #[test]
    fn an_existing_query_is_kept() {
        assert_eq!(
            with_search_path("postgres://u@h/db?sslmode=disable", "s"),
            "postgres://u@h/db?sslmode=disable&options=-csearch_path%3Ds"
        );
    }

    /// Two `options` would leave libpq reading whichever it saw last, and
    /// which that is is not something to leave to a driver.
    #[test]
    fn an_options_already_there_is_replaced_rather_than_joined() {
        let url = with_search_path("postgres://u@h/db?options=-csearch_path%3Dold", "new");
        assert_eq!(url.matches("options=").count(), 1);
        assert!(url.ends_with("options=-csearch_path%3Dnew"));
    }

    #[test]
    fn a_name_that_would_end_the_query_is_encoded() {
        let url = with_search_path("postgres://u@h/db", "od&d ?");
        assert_eq!(
            url,
            "postgres://u@h/db?options=-csearch_path%3Dod%26d%20%3F"
        );
    }
}
