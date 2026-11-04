//! Monte Carlo error with parameter draws (batch means).
//!
//! The simulators play `n` games in `draws` batches, each batch with its own draw of
//! the model's parameters. Games in one batch share those parameters, so they are not
//! independent, and the binomial error sqrt(p(1-p)/n) understates how much a result
//! moves with the seed (measured 2026-11-03: 2.1-2.4x for win shares,
//! docs/architecture/monster-gpt-v2.md). Each histogram keeps every batch's count and
//! sum; the spread of batch means gives the variance of the overall mean, and its
//! ratio to the i.i.d. variance (the design effect) scales the binomial error of every
//! line on that histogram. Using the mean's design effect for a line's probability is
//! an approximation; `nba_mcse_matches_the_seed_to_seed_spread` checks it against reruns.

/// Per-batch (games, sum) of one histogram, closed with [`Batches::end`].
#[derive(Clone, Debug, Default)]
pub struct Batches { b: Vec<(f64, f64)>, at: (f64, f64) }

impl Batches {
    /// Close the batch that ends now, given the histogram's running count and sum.
    pub fn end(&mut self, n: f64, sum: f64) {
        let (dn, ds) = (n - self.at.0, sum - self.at.1);
        if dn > 0.0 { self.b.push((dn, ds)); }
        self.at = (n, sum);
    }
    pub fn merge(&mut self, o: &Batches) { self.b.extend_from_slice(&o.b); }
    /// Variance of the overall mean from the batch means (ratio estimator, unequal batch
    /// sizes) over the i.i.d. variance `var / n`; at least 1, and 1 with fewer than 10 batches.
    pub fn deff(&self, n: f64, mean: f64, var: f64) -> f64 {
        let k = self.b.len() as f64;
        if k < 10.0 || n <= 0.0 || var <= 0.0 { return 1.0; }
        let nbar = n / k;
        let v = self.b.iter().map(|(nb, sb)| (sb - mean * nb).powi(2)).sum::<f64>() / (k * (k - 1.0) * nbar * nbar);
        (v / (var / n)).clamp(1.0, 100.0)
    }
}

/// Monte Carlo standard error of a simulated share `p` over `n` games with design effect `deff`.
pub fn mcse(p: f64, n: f64, deff: f64) -> f64 { if n > 0.0 { (p * (1.0 - p) / n * deff).sqrt() } else { 0.0 } }

/// Variance of a histogram's values: `counts[i]` games had value `i + offset`.
pub fn var_of<C: Copy + Into<f64>>(counts: &[C], offset: f64, n: f64, mean: f64) -> f64 {
    if n <= 0.0 { return 0.0; }
    counts.iter().enumerate().map(|(i, c)| { let c: f64 = (*c).into(); c * (i as f64 + offset - mean).powi(2) }).sum::<f64>() / n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_batches_have_no_design_effect_and_shared_shifts_do() {
        // 40 batches of 1,000 games of a fair coin with no batch effect: deff ~ 1
        let mut rng = crate::mlb_sim::Rng::new(7);
        let (mut b, mut n, mut s) = (Batches::default(), 0.0, 0.0);
        for _ in 0..40 { for _ in 0..1000 { n += 1.0; s += (rng.f64() < 0.5) as u8 as f64; } b.end(n, s); }
        let m = s / n;
        let d = b.deff(n, m, m * (1.0 - m));
        assert!(d < 2.0, "independent games: deff {d}");
        // every batch shifted by its own parameter draw (p = 0.4 or 0.6): deff well above 1
        let (mut b2, mut n2, mut s2) = (Batches::default(), 0.0, 0.0);
        for k in 0..40 { let p = if k % 2 == 0 { 0.4 } else { 0.6 }; for _ in 0..1000 { n2 += 1.0; s2 += (rng.f64() < p) as u8 as f64; } b2.end(n2, s2); }
        let m2 = s2 / n2;
        assert!(b2.deff(n2, m2, m2 * (1.0 - m2)) > 20.0);
        assert_eq!(Batches::default().deff(10.0, 0.5, 0.25), 1.0, "too few batches: binomial");
    }

    #[test]
    fn mcse_scales_with_the_design_effect() {
        assert!((mcse(0.5, 100_000.0, 1.0) - 0.0015811).abs() < 1e-6);
        assert!((mcse(0.5, 100_000.0, 4.0) - 2.0 * 0.0015811).abs() < 1e-6);
    }
}
