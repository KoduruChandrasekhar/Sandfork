//! Idempotency for mutating calls.
//!
//! Every mutating call carries a client `request_id`. The daemon keeps the
//! outcome of a completed request in a bounded cache and returns it on a
//! retry. A retry of a call that is still running attaches to it rather than
//! running it again.
//!
//! The operation runs in its own task, detached from the connection that
//! asked for it. A client that times out and reconnects therefore finds
//! either the finished result or the still-running operation, never a second
//! execution: an `Exec` retried after a network timeout must never run
//! `pip install` or `git commit` twice.
//!
//! Infrastructure errors are not cached: a retry after one runs again, which
//! is what makes an idempotent retry useful after a transient failure.

use sandfork_core::{ErrorClass, Result, SfError};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

type Outcome = Option<std::result::Result<Vec<u8>, SfError>>;

pub struct IdemCache {
    map: Arc<Mutex<HashMap<String, watch::Receiver<Outcome>>>>,
    order: Mutex<VecDeque<String>>,
    cap: usize,
}

impl IdemCache {
    pub fn new(cap: usize) -> Self {
        IdemCache {
            map: Arc::new(Mutex::new(HashMap::new())),
            order: Mutex::new(VecDeque::new()),
            cap,
        }
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Run `f` at most once per `(scope, request_id)`. An empty request id
    /// opts out of caching but still detaches the operation from the caller.
    pub async fn run<T, F>(&self, scope: &str, request_id: &str, f: F) -> Result<T>
    where
        T: prost::Message + Default + Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        if request_id.is_empty() {
            return tokio::spawn(f)
                .await
                .map_err(|e| SfError::infra("task_aborted", e.to_string()))?;
        }
        let key = format!("{scope}:{request_id}");
        let mut rx = {
            let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
            match map.get(&key) {
                Some(rx) => rx.clone(),
                None => {
                    let (tx, rx) = watch::channel::<Outcome>(None);
                    map.insert(key.clone(), rx.clone());
                    let mut order = self.order.lock().unwrap_or_else(|p| p.into_inner());
                    order.push_back(key.clone());
                    while order.len() > self.cap {
                        if let Some(old) = order.pop_front() {
                            map.remove(&old);
                        }
                    }
                    let (shared, k) = (self.map.clone(), key.clone());
                    tokio::spawn(async move {
                        let r = f.await.map(|m| m.encode_to_vec());
                        if matches!(&r, Err(e) if e.class == ErrorClass::Infra) {
                            shared.lock().unwrap_or_else(|p| p.into_inner()).remove(&k);
                        }
                        let _ = tx.send(Some(r));
                    });
                    rx
                }
            }
        };
        let outcome = match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v.clone(),
            Err(_) => {
                // The operation's task died without reporting. Forget it so a
                // retry runs again.
                self.map
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
                return Err(SfError::infra(
                    "task_aborted",
                    "the operation ended without a result",
                ));
            }
        };
        match outcome {
            Some(Ok(bytes)) => T::decode(bytes.as_slice())
                .map_err(|e| SfError::infra("idem_decode", e.to_string())),
            Some(Err(e)) => Err(e),
            None => Err(SfError::infra("task_aborted", "no outcome")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandfork_proto::v1::WriteFileResult;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn a_retry_returns_the_stored_result_without_running_again() {
        let cache = IdemCache::new(16);
        let runs = Arc::new(AtomicU32::new(0));
        for _ in 0..3 {
            let r = runs.clone();
            let out: WriteFileResult = cache
                .run("sbx-1", "req-1", async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Ok(WriteFileResult { bytes_written: 7 })
                })
                .await
                .unwrap();
            assert_eq!(out.bytes_written, 7);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_retry_of_a_running_call_attaches_to_it() {
        let cache = Arc::new(IdemCache::new(16));
        let runs = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let (c, r) = (cache.clone(), runs.clone());
            handles.push(tokio::spawn(async move {
                c.run::<WriteFileResult, _>("s", "slow", async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(WriteFileResult { bytes_written: 1 })
                })
                .await
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap().bytes_written, 1);
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "five concurrent calls with one request id ran the operation once"
        );
    }

    #[tokio::test]
    async fn the_operation_survives_its_caller_being_dropped() {
        let cache = Arc::new(IdemCache::new(16));
        let runs = Arc::new(AtomicU32::new(0));
        let (c, r) = (cache.clone(), runs.clone());
        let first = tokio::spawn(async move {
            c.run::<WriteFileResult, _>("s", "dropped", async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                r.fetch_add(1, Ordering::SeqCst);
                Ok(WriteFileResult { bytes_written: 9 })
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        first.abort(); // the client's connection drops mid-call
        let r2 = runs.clone();
        let out: WriteFileResult = cache
            .run("s", "dropped", async move {
                r2.fetch_add(100, Ordering::SeqCst);
                Ok(WriteFileResult { bytes_written: 0 })
            })
            .await
            .unwrap();
        assert_eq!(
            out.bytes_written, 9,
            "the retry must see the original operation's result"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn task_errors_are_cached_but_infra_errors_are_retried() {
        let cache = IdemCache::new(16);
        let runs = Arc::new(AtomicU32::new(0));
        for _ in 0..2 {
            let r = runs.clone();
            let e = cache
                .run::<WriteFileResult, _>("s", "task", async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Err(SfError::task("file", "no such file"))
                })
                .await
                .unwrap_err();
            assert_eq!(e.class, ErrorClass::Task);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        for want in [1u64, 2] {
            let r = runs.clone();
            let out = cache
                .run::<WriteFileResult, _>("s", "infra", async move {
                    if r.fetch_add(1, Ordering::SeqCst) == 1 {
                        Err(SfError::infra("spawn", "transient"))
                    } else {
                        Ok(WriteFileResult { bytes_written: 2 })
                    }
                })
                .await;
            if want == 1 {
                assert!(out.is_err());
            } else {
                assert_eq!(
                    out.unwrap().bytes_written,
                    2,
                    "a retry after an infrastructure error runs again"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let cache = IdemCache::new(4);
        for i in 0..20 {
            let _: WriteFileResult = cache
                .run("s", &format!("r{i}"), async {
                    Ok(WriteFileResult::default())
                })
                .await
                .unwrap();
        }
        assert!(cache.len() <= 4);
    }
}
