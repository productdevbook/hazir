use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;
use std::time::Duration;

use crate::pool::Pool;

/// A schema a test has been given, and the url to reach it on.
///
/// Handed back when this is dropped. A test that panics never drops anything
/// tidily, and that is allowed for: `hazir reclaim` takes back what the
/// processes holding it are no longer alive to use. Nothing here depends on a
/// test remembering to clean up, because the tests that break are exactly the
/// ones that would forget.
pub struct Lease {
    admin_url: String,
    url: String,
    schema: String,
    burn: bool,
}

impl Lease {
    pub(crate) fn new(admin_url: String, schema: String, burn: bool) -> Lease {
        Lease {
            url: crate::url::with_search_path(&admin_url, &schema),
            admin_url,
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
        let Some(releaser) = releaser(&self.admin_url) else {
            return;
        };
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        let job = Job {
            schema: std::mem::take(&mut self.schema),
            burn: self.burn,
            done,
        };
        if releaser.send(job).is_err() {
            return;
        }
        // Bounded, because a database that has stopped answering must not be
        // able to hang a test run on the way out. What is not handed back
        // here is taken back by `hazir reclaim`.
        let _ = wait.recv_timeout(Duration::from_secs(5));
    }
}

struct Job {
    schema: String,
    burn: bool,
    done: SyncSender<()>,
}

/// One thread, one runtime, one connection, for every lease this process
/// returns.
///
/// A `Drop` cannot await, and the alternatives are worse than a thread: the
/// current runtime may be a `#[tokio::test]`'s, which stops the moment the
/// test returns and would never poll a task spawned on the way out.
fn releaser(url: &str) -> Option<&'static tokio::sync::mpsc::UnboundedSender<Job>> {
    static RELEASER: OnceLock<Option<tokio::sync::mpsc::UnboundedSender<Job>>> = OnceLock::new();

    RELEASER
        .get_or_init(|| {
            let (send, mut recv) = tokio::sync::mpsc::unbounded_channel::<Job>();
            let url = url.to_owned();

            std::thread::Builder::new()
                .name("hazir-release".to_owned())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(_) => return,
                    };
                    runtime.block_on(async move {
                        let Ok(pool) = Pool::connect(&url).await else {
                            return;
                        };
                        while let Some(job) = recv.recv().await {
                            let _ = pool.give_back(&job.schema, job.burn).await;
                            let _ = job.done.send(());
                        }
                    });
                })
                .ok()?;

            Some(send)
        })
        .as_ref()
}
