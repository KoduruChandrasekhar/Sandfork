//! Difficulty-band task scheduling.
//!
//! A GRPO group whose rewards are all equal has zero advantage everywhere and
//! teaches nothing. With a binary reward and pass probability p, the chance a
//! group of G is wasted is `p^G + (1 - p)^G`. A task far from 50% burns most
//! of its sandbox time producing nothing, so the scheduler:
//!
//! 1. keeps a Beta posterior per task, adding passes to alpha and failures to
//!    beta;
//! 2. forgets slowly (alpha and beta are multiplied by a decay each epoch),
//!    because the policy improves and p drifts upward;
//! 3. samples tasks Thompson-style: draw p from each posterior, score the
//!    task by `1 - p^G - (1 - p)^G`, and pick from the top.
//!
//! This is curriculum selection, so it changes what the model trains on. The
//! orchestrator keeps a uniformly sampled evaluation set to detect overfitting
//! to the band.

use sandfork_core::rng::Rng;
use std::collections::BTreeMap;

/// Probability that a group of `g` rollouts at pass rate `p` has identical
/// rewards, and therefore zero advantage.
pub fn wasted(p: f64, g: u32) -> f64 {
    p.powi(g as i32) + (1.0 - p).powi(g as i32)
}

#[derive(Debug, Clone)]
pub struct TaskStat {
    pub dir: String,
    pub family: String,
    pub alpha: f64,
    pub beta: f64,
    pub groups: u64,
}

impl TaskStat {
    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }
}

#[derive(Debug, Clone)]
pub struct Pick {
    pub task_id: String,
    pub dir: String,
    pub pass_estimate: f64,
    pub usefulness: f64,
}

#[derive(Debug, Default)]
pub struct Scheduler {
    pub tasks: BTreeMap<String, TaskStat>,
}

/// Multiplied into every alpha and beta at the start of an epoch.
pub const DECAY: f64 = 0.95;

impl Scheduler {
    pub fn register(&mut self, task_id: &str, dir: &str, family: &str) {
        // Beta(1, 1): nothing known, so the task still gets explored.
        self.tasks
            .entry(task_id.to_string())
            .or_insert_with(|| TaskStat {
                dir: dir.to_string(),
                family: family.to_string(),
                alpha: 1.0,
                beta: 1.0,
                groups: 0,
            });
    }

    /// Record one verified rollout.
    pub fn record(&mut self, task_id: &str, passed: bool) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            if passed {
                t.alpha += 1.0;
            } else {
                t.beta += 1.0;
            }
        }
    }

    /// Forget slowly. The floor keeps both parameters positive.
    pub fn decay(&mut self) {
        for t in self.tasks.values_mut() {
            t.alpha = (t.alpha * DECAY).max(0.5);
            t.beta = (t.beta * DECAY).max(0.5);
        }
    }

    /// Pick `n` distinct tasks. `band` is the Thompson-sampled difficulty
    /// band; `uniform` samples uniformly.
    pub fn pick(
        &mut self,
        n: usize,
        group_size: u32,
        strategy: &str,
        family: &str,
        rng: &mut Rng,
    ) -> Vec<Pick> {
        let mut cands: Vec<(&String, &TaskStat)> = self
            .tasks
            .iter()
            .filter(|(_, t)| family.is_empty() || t.family == family)
            .collect();
        let g = group_size.max(2);
        let picks: Vec<Pick> = if strategy == "uniform" {
            rng.shuffle(&mut cands);
            cands
                .iter()
                .take(n)
                .map(|(id, t)| Pick {
                    task_id: (*id).clone(),
                    dir: t.dir.clone(),
                    pass_estimate: t.mean(),
                    usefulness: 1.0 - wasted(t.mean(), g),
                })
                .collect()
        } else {
            let mut scored: Vec<Pick> = cands
                .iter()
                .map(|(id, t)| {
                    let p = rng.beta(t.alpha, t.beta);
                    Pick {
                        task_id: (*id).clone(),
                        dir: t.dir.clone(),
                        pass_estimate: t.mean(),
                        usefulness: 1.0 - wasted(p, g),
                    }
                })
                .collect();
            scored.sort_by(|a, b| {
                b.usefulness
                    .partial_cmp(&a.usefulness)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(n);
            scored
        };
        for p in &picks {
            if let Some(t) = self.tasks.get_mut(&p.task_id) {
                t.groups += 1;
            }
        }
        picks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasted_group_table() {
        // The table in docs/pool.md, at G = 8.
        for (p, want) in [
            (0.05, 0.66),
            (0.10, 0.43),
            (0.20, 0.17),
            (0.50, 0.008),
            (0.80, 0.17),
            (0.95, 0.66),
        ] {
            assert!(
                (wasted(p, 8) - want).abs() < 0.01,
                "p={p}: {}",
                wasted(p, 8)
            );
        }
    }

    #[test]
    fn posterior_updates_and_decays() {
        let mut s = Scheduler::default();
        s.register("t", "/t", "f");
        for _ in 0..9 {
            s.record("t", true);
        }
        s.record("t", false);
        let t = &s.tasks["t"];
        assert_eq!((t.alpha, t.beta), (10.0, 2.0));
        assert!((t.mean() - 10.0 / 12.0).abs() < 1e-9);
        s.decay();
        let t = &s.tasks["t"];
        assert!((t.alpha - 9.5).abs() < 1e-9 && (t.beta - 1.9).abs() < 1e-9);
        // Decay keeps the mean and widens the posterior.
        assert!((t.mean() - 10.0 / 12.0).abs() < 1e-9);
        for _ in 0..200 {
            s.decay();
        }
        assert!(s.tasks["t"].alpha >= 0.5 && s.tasks["t"].beta >= 0.5);
    }

    /// Simulate training groups against known pass rates and count the
    /// fraction of groups with reward variance.
    fn useful_fraction(strategy: &str, seed: u64) -> f64 {
        let mut s = Scheduler::default();
        // Twenty tasks with true pass rates from 0.02 to 0.98.
        let truth: Vec<(String, f64)> = (0..20)
            .map(|i| (format!("t{i:02}"), 0.02 + 0.96 * i as f64 / 19.0))
            .collect();
        for (id, _) in &truth {
            s.register(id, "/d", "f");
        }
        let mut rng = Rng::new(seed);
        let (mut useful, mut total) = (0u32, 0u32);
        for _ in 0..150 {
            s.decay();
            for pick in s.pick(4, 8, strategy, "", &mut rng) {
                let p = truth
                    .iter()
                    .find(|(id, _)| *id == pick.task_id)
                    .map(|t| t.1)
                    .unwrap_or(0.5);
                let passes = (0..8).filter(|_| rng.f64() < p).count();
                for i in 0..8 {
                    s.record(&pick.task_id, i < passes);
                }
                total += 1;
                if passes != 0 && passes != 8 {
                    useful += 1;
                }
            }
        }
        useful as f64 / total as f64
    }

    #[test]
    fn the_band_wastes_fewer_groups_than_uniform_sampling() {
        let band: f64 = (1..=3).map(|s| useful_fraction("band", s)).sum::<f64>() / 3.0;
        let uniform: f64 = (1..=3).map(|s| useful_fraction("uniform", s)).sum::<f64>() / 3.0;
        assert!(
            band > uniform + 0.1,
            "band {band:.2} should beat uniform {uniform:.2} clearly"
        );
        assert!(band > 0.85, "band useful-group fraction {band:.2}");
    }

    #[test]
    fn picks_are_distinct_and_filtered_by_family() {
        let mut s = Scheduler::default();
        for i in 0..5 {
            s.register(&format!("a{i}"), "/a", "fam-a");
            s.register(&format!("b{i}"), "/b", "fam-b");
        }
        let mut rng = Rng::new(9);
        for strategy in ["band", "uniform"] {
            let picks = s.pick(4, 8, strategy, "fam-a", &mut rng);
            assert_eq!(picks.len(), 4);
            let mut ids: Vec<_> = picks.iter().map(|p| p.task_id.clone()).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 4, "{strategy}: picks must be distinct");
            assert!(ids.iter().all(|i| i.starts_with('a')));
        }
        assert_eq!(s.pick(100, 8, "band", "fam-b", &mut rng).len(), 5);
    }
}
