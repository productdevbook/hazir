/// A schema a test has been given, and the url to reach it on.
///
/// Handed back when this is dropped. A test that panics never drops anything
/// tidily, and that is allowed for: `hazir reclaim` takes back what the
/// processes holding it are no longer alive to use. Nothing here depends on a
/// test remembering to clean up, because the tests that break are exactly the
/// ones that would forget.
pub struct Lease {
    url: String,
    schema: String,
    burn: bool,
}

impl Lease {
    pub(crate) fn new(admin_url: String, schema: String, burn: bool) -> Lease {
        Lease {
            url: crate::url::with_search_path(&admin_url, &schema),
            schema,
            burn,
        }
    }

    /// What to hand to sqlx, SeaORM, Diesel or tokio-postgres. Opening it
    /// puts the connection in this schema and nowhere else.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        crate::agent::hand_back(std::mem::take(&mut self.schema), self.burn);
    }
}
