//! Admission control, built around the GRPO group.
//!
//! * **Gang admission.** `ForkGroup(G)` starts only when all G sandboxes fit.
//!   Otherwise it queues with a deadline. Without this, two half-started
//!   groups can each hold the resources the other needs, and neither
//!   finishes; and a partial group is useless to GRPO anyway.
//! * **Memory-aware.** A group is admitted only if its estimated memory keeps
//!   the total under the budget set aside for Sandfork.
//! * **Priorities.** Verification forks first, because they complete work
//!   already in flight. Restores of in-flight rollouts second. New groups
//!   last. Within a priority the queue is first in, first out, and only the
//!   head may be admitted, so a large group is never starved by small ones.
//! * **Headroom for work in flight.** Priority orders the queue but cannot
//!   preempt, so new groups may use only part of the pool: the rest is a
//!   reserve that only verification forks and restores may use. Without it,
//!   rollouts that fill the pool wait forever for the verification forks that
//!   would let them finish.

use sandfork_core::{Result, SfError};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    New = 0,
    Restore = 1,
    Verify = 2,
}

#[derive(Debug, Clone)]
struct Waiter {
    seq: u64,
    prio: Priority,
    n: u32,
    mb_each: u64,
}

/// The pure admission arithmetic, separate from waiting so it can be tested
/// exhaustively.
#[derive(Debug)]
pub struct AdmState {
    pub max_slots: u32,
    /// 0 means no memory limit.
    pub budget_mb: u64,
    /// Held back from new groups for verification and restores.
    pub reserve_slots: u32,
    pub reserve_mb: u64,
    pub used_slots: u32,
    pub used_mb: u64,
    queue: Vec<Waiter>,
    seq: u64,
}

impl AdmState {
    pub fn new(max_slots: u32, budget_mb: u64) -> Self {
        Self::with_reserve(max_slots, budget_mb, 0, 0)
    }

    pub fn with_reserve(
        max_slots: u32,
        budget_mb: u64,
        reserve_slots: u32,
        reserve_mb: u64,
    ) -> Self {
        AdmState {
            max_slots,
            budget_mb,
            reserve_slots: reserve_slots.min(max_slots.saturating_sub(1)),
            reserve_mb: reserve_mb.min(budget_mb),
            used_slots: 0,
            used_mb: 0,
            queue: Vec::new(),
            seq: 0,
        }
    }

    /// (slots, MB) a group of this priority may bring the pool up to.
    fn limits(&self, prio: Priority) -> (u32, u64) {
        if prio == Priority::New {
            (
                self.max_slots - self.reserve_slots,
                self.budget_mb - self.reserve_mb,
            )
        } else {
            (self.max_slots, self.budget_mb)
        }
    }

    fn fits(&self, prio: Priority, n: u32, mb_each: u64) -> bool {
        let (slots, mb) = self.limits(prio);
        self.used_slots + n <= slots
            && (self.budget_mb == 0 || self.used_mb + n as u64 * mb_each <= mb)
    }

    /// Join the queue. A group that could not fit even in an empty pool is
    /// refused at once instead of waiting for its deadline.
    pub fn enqueue(&mut self, prio: Priority, n: u32, mb_each: u64) -> Result<u64> {
        if n == 0 {
            return Err(SfError::client(
                "bad_request",
                "a group needs at least one sandbox",
            ));
        }
        let (slots, mb) = self.limits(prio);
        if n > slots || (self.budget_mb != 0 && n as u64 * mb_each > mb) {
            return Err(SfError::client(
                "group_too_large",
                format!("a group of {n} sandboxes at {mb_each} MB each can never be admitted (pool for this priority: {slots} sandboxes, {mb} MB)"),
            ));
        }
        self.seq += 1;
        self.queue.push(Waiter {
            seq: self.seq,
            prio,
            n,
            mb_each,
        });
        Ok(self.seq)
    }

    fn head(&self) -> Option<&Waiter> {
        // Highest priority first; within a priority, earliest first.
        self.queue
            .iter()
            .max_by(|a, b| a.prio.cmp(&b.prio).then(b.seq.cmp(&a.seq)))
    }

    /// Admit `seq` if it is at the head of the queue and the whole group fits.
    pub fn try_admit(&mut self, seq: u64) -> bool {
        let Some(h) = self.head().cloned() else {
            return false;
        };
        if h.seq != seq || !self.fits(h.prio, h.n, h.mb_each) {
            return false;
        }
        self.queue.retain(|w| w.seq != seq);
        self.used_slots += h.n;
        self.used_mb += h.n as u64 * h.mb_each;
        true
    }

    pub fn cancel(&mut self, seq: u64) {
        self.queue.retain(|w| w.seq != seq);
    }

    pub fn release(&mut self, n: u32, mb_each: u64) {
        self.used_slots = self.used_slots.saturating_sub(n);
        self.used_mb = self.used_mb.saturating_sub(n as u64 * mb_each);
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

pub struct Admission {
    state: Mutex<AdmState>,
    notify: Notify,
}

/// Removes a waiter from the queue if its future is dropped before admission,
/// so an abandoned request can never block the head of the line.
struct WaitGuard<'a> {
    adm: &'a Admission,
    seq: u64,
    admitted: bool,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if !self.admitted {
            self.adm.lock().cancel(self.seq);
            self.adm.notify.notify_waiters();
        }
    }
}

impl Admission {
    pub fn new(max_slots: u32, budget_mb: u64) -> Self {
        Self::with_reserve(max_slots, budget_mb, 0, 0)
    }

    pub fn with_reserve(
        max_slots: u32,
        budget_mb: u64,
        reserve_slots: u32,
        reserve_mb: u64,
    ) -> Self {
        Admission {
            state: Mutex::new(AdmState::with_reserve(
                max_slots,
                budget_mb,
                reserve_slots,
                reserve_mb,
            )),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AdmState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait until the whole group is admitted. Returns the time spent queued.
    pub async fn acquire(
        &self,
        prio: Priority,
        n: u32,
        mb_each: u64,
        deadline: Duration,
    ) -> Result<Duration> {
        let start = Instant::now();
        let seq = self.lock().enqueue(prio, n, mb_each)?;
        let mut guard = WaitGuard {
            adm: self,
            seq,
            admitted: false,
        };
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().try_admit(seq) {
                guard.admitted = true;
                // Whoever is behind this group may now be at the head.
                self.notify.notify_waiters();
                return Ok(start.elapsed());
            }
            let left = deadline.saturating_sub(start.elapsed());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Err(SfError::infra(
                    "admission_deadline",
                    format!(
                        "group of {n} was not admitted within {} ms",
                        deadline.as_millis()
                    ),
                ));
            }
        }
    }

    pub fn release(&self, n: u32, mb_each: u64) {
        self.lock().release(n, mb_each);
        self.notify.notify_waiters();
    }

    /// (used slots, used MB, queue depth).
    pub fn usage(&self) -> (u32, u64, usize) {
        let s = self.lock();
        (s.used_slots, s.used_mb, s.queued())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandfork_core::rng::Rng;

    #[test]
    fn a_group_is_admitted_whole_or_not_at_all() {
        let mut s = AdmState::new(8, 0);
        let a = s.enqueue(Priority::New, 6, 100).unwrap();
        let b = s.enqueue(Priority::New, 6, 100).unwrap();
        assert!(s.try_admit(a));
        assert!(
            !s.try_admit(b),
            "6 + 6 > 8: the second group must wait, not start partially"
        );
        assert_eq!(s.used_slots, 6);
        s.release(6, 100);
        assert!(s.try_admit(b));
    }

    #[test]
    fn fifo_within_a_priority_and_no_overtaking() {
        let mut s = AdmState::new(8, 0);
        let first = s.enqueue(Priority::New, 8, 1).unwrap();
        let big = s.enqueue(Priority::New, 8, 1).unwrap();
        let small = s.enqueue(Priority::New, 1, 1).unwrap();
        assert!(s.try_admit(first));
        s.release(4, 1);
        // Four slots are free. The small group would fit, but the big group
        // is ahead of it; letting small groups overtake would starve it.
        assert!(!s.try_admit(small));
        assert!(!s.try_admit(big));
        s.release(4, 1);
        assert!(s.try_admit(big));
    }

    #[test]
    fn verification_jumps_the_queue() {
        let mut s = AdmState::new(4, 0);
        let running = s.enqueue(Priority::New, 4, 1).unwrap();
        assert!(s.try_admit(running));
        let new = s.enqueue(Priority::New, 2, 1).unwrap();
        let restore = s.enqueue(Priority::Restore, 1, 1).unwrap();
        let verify = s.enqueue(Priority::Verify, 1, 1).unwrap();
        s.release(1, 1);
        assert!(!s.try_admit(new) && !s.try_admit(restore));
        assert!(
            s.try_admit(verify),
            "verification completes work already in flight"
        );
        s.release(1, 1);
        assert!(!s.try_admit(new));
        assert!(s.try_admit(restore));
    }

    #[test]
    fn memory_budget_is_respected() {
        let mut s = AdmState::new(100, 1000);
        let a = s.enqueue(Priority::New, 4, 200).unwrap();
        let b = s.enqueue(Priority::New, 2, 200).unwrap();
        assert!(s.try_admit(a));
        assert!(!s.try_admit(b), "800 + 400 > 1000");
        assert_eq!(
            s.enqueue(Priority::New, 8, 200).unwrap_err().code,
            "group_too_large"
        );
        assert_eq!(
            s.enqueue(Priority::New, 0, 1).unwrap_err().code,
            "bad_request"
        );
    }

    /// Random arrivals, admissions and releases under heavy oversubscription:
    /// the pool is never overcommitted, and every admitted group got all of
    /// its sandboxes.
    #[test]
    fn invariants_hold_under_random_load() {
        let mut rng = Rng::new(42);
        for round in 0..200 {
            let (slots, budget) = (1 + rng.below(16) as u32, rng.below(3) * 500);
            let mut s = AdmState::new(slots, budget);
            let mut waiting: Vec<(u64, u32, u64)> = Vec::new();
            let mut running: Vec<(u32, u64)> = Vec::new();
            for _ in 0..300 {
                match rng.below(3) {
                    0 => {
                        let (n, mb) = (1 + rng.below(slots as u64 * 2) as u32, 50 + rng.below(200));
                        let prio = [Priority::New, Priority::Restore, Priority::Verify]
                            [rng.below(3) as usize];
                        if let Ok(seq) = s.enqueue(prio, n, mb) {
                            waiting.push((seq, n, mb));
                        }
                    }
                    1 => {
                        if !running.is_empty() {
                            let (n, mb) =
                                running.swap_remove(rng.below(running.len() as u64) as usize);
                            s.release(n, mb);
                        }
                    }
                    _ => {}
                }
                let before = s.used_slots;
                if let Some(i) = waiting.iter().position(|(seq, _, _)| s.try_admit(*seq)) {
                    let (_, n, mb) = waiting.swap_remove(i);
                    assert_eq!(
                        s.used_slots,
                        before + n,
                        "round {round}: a group was admitted partially"
                    );
                    running.push((n, mb));
                }
                assert!(s.used_slots <= slots, "round {round}: slots overcommitted");
                assert!(
                    budget == 0 || s.used_mb <= budget,
                    "round {round}: memory overcommitted"
                );
                assert_eq!(s.used_slots, running.iter().map(|r| r.0).sum::<u32>());
            }
        }
    }

    #[test]
    fn verification_can_always_start_when_new_groups_fill_their_share() {
        // The deadlock this guards against: rollouts fill the pool, then each
        // waits for a verification fork that can never be admitted.
        let mut s = AdmState::with_reserve(8, 1000, 1, 150);
        let g = s.enqueue(Priority::New, 4, 200).unwrap();
        assert!(s.try_admit(g));
        let more = s.enqueue(Priority::New, 1, 200).unwrap();
        assert!(!s.try_admit(more), "new groups may use only 850 MB");
        s.cancel(more);
        let v = s.enqueue(Priority::Verify, 1, 150).unwrap();
        assert!(s.try_admit(v), "the reserve is there for verification");
        assert_eq!(
            s.enqueue(Priority::New, 5, 200).unwrap_err().code,
            "group_too_large"
        );
        assert!(s.enqueue(Priority::Restore, 5, 200).is_ok());
    }

    #[tokio::test]
    async fn queued_group_times_out_and_leaves_the_queue() {
        let adm = Admission::new(2, 0);
        adm.acquire(Priority::New, 2, 1, Duration::from_secs(1))
            .await
            .unwrap();
        let err = adm
            .acquire(Priority::New, 1, 1, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(err.code, "admission_deadline");
        assert_eq!(
            adm.usage(),
            (2, 2, 0),
            "a timed-out waiter must not stay queued"
        );
        adm.release(2, 1);
        adm.acquire(Priority::New, 2, 1, Duration::from_millis(30))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_waiter_is_woken_by_a_release() {
        let adm = std::sync::Arc::new(Admission::new(1, 0));
        adm.acquire(Priority::New, 1, 1, Duration::from_secs(1))
            .await
            .unwrap();
        let a2 = adm.clone();
        let waiter = tokio::spawn(async move {
            a2.acquire(Priority::New, 1, 1, Duration::from_secs(5))
                .await
        });
        tokio::task::yield_now().await;
        adm.release(1, 1);
        let waited = waiter.await.unwrap().unwrap();
        assert!(waited < Duration::from_secs(4));
    }
}
