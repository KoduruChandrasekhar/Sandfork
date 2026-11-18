//! A small deterministic PRNG (xoshiro256**) with the two distributions the
//! scheduler needs. Seeded explicitly so scheduling experiments are reproducible.

#[derive(Debug, Clone)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // splitmix64 expands the seed into the state.
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9e3779b97f4a7c15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
            x ^ (x >> 31)
        };
        Rng {
            s: [next(), next(), next(), next()],
        }
    }

    pub fn from_entropy() -> Self {
        let mut b = [0u8; 8];
        crate::ids::random_bytes(&mut b);
        Self::new(u64::from_le_bytes(b))
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        (self.f64() * n as f64) as u64 % n
    }

    /// Standard normal by Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = (1.0 - self.f64()).max(f64::MIN_POSITIVE);
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Gamma(shape, 1) by Marsaglia and Tsang's method.
    pub fn gamma(&mut self, shape: f64) -> f64 {
        if shape < 1.0 {
            let u = self.f64().max(f64::MIN_POSITIVE);
            return self.gamma(shape + 1.0) * u.powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = 1.0 + c * x;
            if v <= 0.0 {
                continue;
            }
            let v = v * v * v;
            let u = self.f64().max(f64::MIN_POSITIVE);
            if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
                return d * v;
            }
        }
    }

    /// Beta(a, b) as a ratio of gammas.
    pub fn beta(&mut self, a: f64, b: f64) -> f64 {
        let x = self.gamma(a.max(1e-6));
        let y = self.gamma(b.max(1e-6));
        if x + y == 0.0 {
            0.5
        } else {
            x / (x + y)
        }
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_a_seed() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn beta_mean_matches() {
        let mut r = Rng::new(1);
        let n = 20_000;
        let mean: f64 = (0..n).map(|_| r.beta(2.0, 6.0)).sum::<f64>() / n as f64;
        assert!((mean - 0.25).abs() < 0.01, "mean {mean}");
        let mean: f64 = (0..n).map(|_| r.beta(0.5, 0.5)).sum::<f64>() / n as f64;
        assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
    }

    #[test]
    fn uniform_in_range() {
        let mut r = Rng::new(3);
        for _ in 0..1000 {
            let x = r.f64();
            assert!((0.0..1.0).contains(&x));
            assert!(r.below(10) < 10);
        }
    }
}
