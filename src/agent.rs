//! One thread, one runtime, one connection, for everything this process does
//! to the pool.
//!
//! Opening a connection is the expensive part of leasing a schema — eighty
//! milliseconds of it on the machine this was written on, nearly all of it
//! SCRAM's key derivation, which is slow on purpose and cannot be argued
//! with. Doing that once per lease costs more than building the schema from
//! scratch would have.
//!
//! It cannot simply be cached in a `static`, either: a connection is tied to
//! the runtime whose task is driving it, and under `#[tokio::test]` that
//! runtime is gone by the time the second test asks. The client would still
//! look alive and every query on it would hang.
//!
//! So the connection lives on a thread of its own, with a runtime of its own,
//! which outlives every test's. It is also what returns a lease from `Drop`,
//! where there is no runtime to await on at all.

use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::pool::Pool;
use crate::{Error, Result};

pub(crate) enum Request {
    Lease {
        burn: bool,
        reply: oneshot::Sender<Result<Taken>>,
    },
    GiveBack {
        schema: String,
        burn: bool,
        done: SyncSender<()>,
    },
}

pub(crate) struct Taken {
    pub schema: String,
    pub url: String,
}

pub(crate) fn agent() -> Result<&'static UnboundedSender<Request>> {
    static AGENT: OnceLock<Option<UnboundedSender<Request>>> = OnceLock::new();

    AGENT
        .get_or_init(|| {
            let url = crate::env_url().ok()?;
            let (send, mut recv) = tokio::sync::mpsc::unbounded_channel::<Request>();

            std::thread::Builder::new()
                .name("hazir".to_owned())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    runtime.block_on(async move {
                        let pool = match Pool::connect(&url).await {
                            Ok(pool) => pool,
                            Err(err) => {
                                // Nothing to serve requests with. Answer them
                                // rather than leaving them waiting: a test
                                // wants to fail saying the database refused,
                                // not time out saying nothing.
                                while let Some(request) = recv.recv().await {
                                    refuse(request, &err);
                                }
                                return;
                            }
                        };
                        while let Some(request) = recv.recv().await {
                            serve(&pool, request).await;
                        }
                    });
                })
                .ok()?;

            Some(send)
        })
        .as_ref()
        .ok_or(Error::NoUrl)
}

async fn serve(pool: &Pool, request: Request) {
    match request {
        Request::Lease { burn, reply } => {
            let taken = pool.claim(burn).await.map(|schema| Taken {
                schema,
                url: pool.url().to_owned(),
            });
            let _ = reply.send(taken);
        }
        Request::GiveBack { schema, burn, done } => {
            let _ = pool.give_back(&schema, burn).await;
            let _ = done.send(());
        }
    }
}

fn refuse(request: Request, why: &Error) {
    match request {
        Request::Lease { reply, .. } => {
            let _ = reply.send(Err(Error::Unreachable(why.to_string())));
        }
        Request::GiveBack { done, .. } => {
            let _ = done.send(());
        }
    }
}

/// Asks for a schema and waits for the answer.
pub(crate) async fn ask(burn: bool) -> Result<Taken> {
    let (reply, answer) = oneshot::channel();
    agent()?
        .send(Request::Lease { burn, reply })
        .map_err(|_| Error::Exhausted)?;
    answer.await.map_err(|_| Error::Exhausted)?
}

/// Hands one back, from a `Drop` that cannot await.
///
/// Bounded, because a database that has stopped answering must not be able to
/// hang a test run on the way out. What is not handed back here is taken back
/// by `hazir reclaim`.
pub(crate) fn hand_back(schema: String, burn: bool) {
    let Ok(agent) = agent() else {
        return;
    };
    let (done, wait) = std::sync::mpsc::sync_channel(1);
    if agent
        .send(Request::GiveBack { schema, burn, done })
        .is_err()
    {
        return;
    }
    let _ = wait.recv_timeout(std::time::Duration::from_secs(5));
}
