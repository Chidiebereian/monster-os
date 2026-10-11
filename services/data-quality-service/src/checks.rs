//! The seven quality checkpoints: Inspect -> Detect -> Fix -> Validate.
//! Each check operates on a simple column-oriented dataset and returns a
//! CheckResult. Thresholds live in QualityConfig so each sport/market can
//! tune them without code changes.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// One row = one record (a game, a market snapshot, a player line).
/// Values are optional so missing data is representable.
pub type Row = HashMap<String, Option<f64>>;

#[derive(Debug, Deserialize, Clone)]
pub struct Dataset {
    pub rows: Vec<Row>,
    /// Column that uniquely identifies a record (e.g. "game_id").
    pub key_column: Option<String>,
    /// Column holding the event time as unix seconds.
    pub event_time_column: Option<String>,
    /// Column holding the label/outcome being predicted.
    pub label_column: Option<String>,
    /// Legal label values (e.g. [0, 1] for win/loss).
    pub allowed_labels: Option<Vec<f64>>,
    /// Unix seconds at which predictions are made. Any feature row stamped
    /// after this is future information = leakage.
    pub prediction_cutoff: Option<f64>,
    /// Column used to check group representation (e.g. "team_code").
    pub group_column: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct QualityConfig {
    pub max_missing_rate: f64,
    pub outlier_z: f64,
    pub min_group_share: f64,
    /// Population Stability Index threshold; >0.25 is the usual "major shift".
    pub max_psi: f64,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self { max_missing_rate: 0.05, outlier_z: 4.0, min_group_share: 0.02, max_psi: 0.25 }
    }
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status { Pass, Warn, Fail, Skipped }

#[derive(Debug, Serialize, Clone)]
pub struct CheckResult {
    pub check: &'static str,
    pub status: Status,
    pub detail: String,
    pub flagged_rows: Vec<usize>,
}

fn result(check: &'static str, status: Status, detail: String, flagged: Vec<usize>) -> CheckResult {
    CheckResult { check, status, detail, flagged_rows: flagged }
}

fn columns(ds: &Dataset) -> Vec<String> {
    let mut cols: HashSet<String> = HashSet::new();
    for r in &ds.rows { cols.extend(r.keys().cloned()); }
    let mut v: Vec<_> = cols.into_iter().collect();
    v.sort();
    v
}

fn numeric(ds: &Dataset, col: &str) -> Vec<(usize, f64)> {
    ds.rows.iter().enumerate()
        .filter_map(|(i, r)| r.get(col).copied().flatten().map(|v| (i, v)))
        .collect()
}

fn mean_std(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    if n < 2.0 { return (xs.first().copied().unwrap_or(0.0), 0.0); }
    let m = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0);
    (m, var.sqrt())
}

/// 1. Missing values
pub fn missing_values(ds: &Dataset, cfg: &QualityConfig) -> CheckResult {
    let n = ds.rows.len().max(1) as f64;
    let mut worst = Vec::new();
    let mut flagged = HashSet::new();
    for col in columns(ds) {
        let missing: Vec<usize> = ds.rows.iter().enumerate()
            .filter(|(_, r)| !matches!(r.get(&col), Some(Some(_))))
            .map(|(i, _)| i).collect();
        let rate = missing.len() as f64 / n;
        if rate > cfg.max_missing_rate {
            worst.push(format!("{col}: {:.1}%", rate * 100.0));
            flagged.extend(missing);
        }
    }
    let mut f: Vec<_> = flagged.into_iter().collect(); f.sort();
    if worst.is_empty() {
        result("missing_values", Status::Pass, "All columns under missing-rate threshold".into(), f)
    } else {
        result("missing_values", Status::Fail, format!("Over threshold: {}", worst.join(", ")), f)
    }
}

/// 2. Duplicates (by key column, or full-row equality if no key)
pub fn duplicates(ds: &Dataset) -> CheckResult {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut flagged = Vec::new();
    for (i, r) in ds.rows.iter().enumerate() {
        let sig = match &ds.key_column {
            Some(k) => format!("{:?}", r.get(k)),
            None => {
                let mut kv: Vec<_> = r.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
                kv.sort(); kv.join("|")
            }
        };
        if seen.contains_key(&sig) { flagged.push(i); } else { seen.insert(sig, i); }
    }
    let status = if flagged.is_empty() { Status::Pass } else { Status::Fail };
    result("duplicates", status, format!("{} duplicate records", flagged.len()), flagged)
}

/// 3. Data leakage: feature rows timestamped after the prediction cutoff,
/// or a feature column identical to the label (target leakage).
pub fn data_leakage(ds: &Dataset) -> CheckResult {
    let mut flagged = Vec::new();
    let mut notes = Vec::new();
    if let (Some(tc), Some(cut)) = (&ds.event_time_column, ds.prediction_cutoff) {
        for (i, t) in numeric(ds, tc) { if t > cut { flagged.push(i); } }
        if !flagged.is_empty() { notes.push(format!("{} rows after prediction cutoff", flagged.len())); }
    }
    if let Some(lc) = &ds.label_column {
        for col in columns(ds) {
            if &col == lc || Some(&col) == ds.key_column.as_ref() { continue; }
            let pairs: Vec<_> = ds.rows.iter()
                .filter_map(|r| Some((r.get(&col).copied().flatten()?, r.get(lc).copied().flatten()?)))
                .collect();
            if pairs.len() >= 10 && pairs.iter().all(|(a, b)| (a - b).abs() < 1e-12) {
                notes.push(format!("column '{col}' is identical to label"));
            }
        }
    }
    if ds.event_time_column.is_none() && ds.label_column.is_none() {
        return result("data_leakage", Status::Skipped, "No time or label column provided".into(), vec![]);
    }
    let status = if notes.is_empty() { Status::Pass } else { Status::Fail };
    let detail = if notes.is_empty() { "No leakage detected".into() } else { notes.join("; ") };
    result("data_leakage", status, detail, flagged)
}

/// 4. Wrong labels: missing or outside the allowed label set.
pub fn wrong_labels(ds: &Dataset) -> CheckResult {
    let Some(lc) = &ds.label_column else {
        return result("wrong_labels", Status::Skipped, "No label column provided".into(), vec![]);
    };
    let flagged: Vec<usize> = ds.rows.iter().enumerate().filter(|(_, r)| {
        match r.get(lc).copied().flatten() {
            None => true,
            Some(v) => match &ds.allowed_labels {
                Some(allowed) => !allowed.iter().any(|a| (a - v).abs() < 1e-12),
                None => !v.is_finite(),
            },
        }
    }).map(|(i, _)| i).collect();
    let status = if flagged.is_empty() { Status::Pass } else { Status::Fail };
    result("wrong_labels", status, format!("{} invalid labels", flagged.len()), flagged)
}

/// 5. Outliers: |z| above threshold. Warn, don't fail — in sports data a
/// 60-point game is real and should be investigated, not auto-deleted.
pub fn outliers(ds: &Dataset, cfg: &QualityConfig) -> CheckResult {
    let mut flagged = HashSet::new();
    let mut hits = Vec::new();
    for col in columns(ds) {
        if Some(&col) == ds.key_column.as_ref() || Some(&col) == ds.event_time_column.as_ref() { continue; }
        let vals = numeric(ds, &col);
        let xs: Vec<f64> = vals.iter().map(|(_, v)| *v).collect();
        let (m, s) = mean_std(&xs);
        if s == 0.0 { continue; }
        let out: Vec<usize> = vals.iter().filter(|(_, v)| ((v - m) / s).abs() > cfg.outlier_z).map(|(i, _)| *i).collect();
        if !out.is_empty() { hits.push(format!("{col}: {}", out.len())); flagged.extend(out); }
    }
    let mut f: Vec<_> = flagged.into_iter().collect(); f.sort();
    if hits.is_empty() {
        result("outliers", Status::Pass, "No extreme values".into(), f)
    } else {
        result("outliers", Status::Warn, format!("Investigate: {}", hits.join(", ")), f)
    }
}

/// 6. Sampling bias: any group below the minimum share of rows.
pub fn sampling_bias(ds: &Dataset, cfg: &QualityConfig) -> CheckResult {
    let Some(gc) = &ds.group_column else {
        return result("sampling_bias", Status::Skipped, "No group column provided".into(), vec![]);
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in &ds.rows { *counts.entry(format!("{:?}", r.get(gc).copied().flatten())).or_default() += 1; }
    let n = ds.rows.len().max(1) as f64;
    let mut thin: Vec<_> = counts.iter().filter(|(_, c)| (**c as f64 / n) < cfg.min_group_share)
        .map(|(g, c)| format!("{g}={c}")).collect();
    thin.sort();
    if thin.is_empty() {
        result("sampling_bias", Status::Pass, format!("{} groups, all adequately represented", counts.len()), vec![])
    } else {
        result("sampling_bias", Status::Warn, format!("Underrepresented: {}", thin.join(", ")), vec![])
    }
}

/// Population Stability Index over 10 bins defined on the reference sample.
pub fn psi(reference: &[f64], current: &[f64]) -> f64 {
    if reference.is_empty() || current.is_empty() { return 0.0; }
    let mut sorted = reference.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let edges: Vec<f64> = (1..10).map(|k| sorted[(k * sorted.len() / 10).min(sorted.len() - 1)]).collect();
    let bin = |x: f64| edges.iter().take_while(|e| x > **e).count();
    let hist = |xs: &[f64]| {
        let mut h = [0f64; 10];
        for &x in xs { h[bin(x)] += 1.0; }
        h.iter().map(|c| (c / xs.len() as f64).max(1e-4)).collect::<Vec<_>>()
    };
    let (r, c) = (hist(reference), hist(current));
    r.iter().zip(&c).map(|(r, c)| (c - r) * (c / r).ln()).sum()
}

/// 7. Distribution shift: PSI of each shared numeric column, train vs production.
pub fn distribution_shift(train: &Dataset, prod: Option<&Dataset>, cfg: &QualityConfig) -> CheckResult {
    let Some(prod) = prod else {
        return result("distribution_shift", Status::Skipped, "No production sample provided".into(), vec![]);
    };
    let prod_cols: HashSet<String> = columns(prod).into_iter().collect();
    let mut shifted = Vec::new();
    for col in columns(train) {
        if !prod_cols.contains(&col) || Some(&col) == train.key_column.as_ref() { continue; }
        let a: Vec<f64> = numeric(train, &col).into_iter().map(|(_, v)| v).collect();
        let b: Vec<f64> = numeric(prod, &col).into_iter().map(|(_, v)| v).collect();
        let p = psi(&a, &b);
        if p > cfg.max_psi { shifted.push(format!("{col}: PSI {p:.2}")); }
    }
    if shifted.is_empty() {
        result("distribution_shift", Status::Pass, "Train and production distributions aligned".into(), vec![])
    } else {
        result("distribution_shift", Status::Fail, shifted.join(", "), vec![])
    }
}

#[derive(Debug, Serialize)]
pub struct QualityReport {
    pub passed: bool,
    pub rows: usize,
    pub checks: Vec<CheckResult>,
}

/// Run all seven checkpoints. `passed` is false if any check Fails;
/// Warns are surfaced but don't block training.
pub fn run_all(train: &Dataset, prod: Option<&Dataset>, cfg: &QualityConfig) -> QualityReport {
    let checks = vec![
        missing_values(train, cfg),
        duplicates(train),
        data_leakage(train),
        wrong_labels(train),
        outliers(train, cfg),
        sampling_bias(train, cfg),
        distribution_shift(train, prod, cfg),
    ];
    let passed = !checks.iter().any(|c| c.status == Status::Fail);
    QualityReport { passed, rows: train.rows.len(), checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, Option<f64>)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn base() -> Dataset {
        Dataset {
            rows: (0..20).map(|i| row(&[
                ("game_id", Some(i as f64)), ("pts", Some(100.0 + (i % 7) as f64)),
                ("ts", Some(1000.0 + i as f64)), ("won", Some((i % 2) as f64)),
                ("team", Some((i % 4) as f64)),
            ])).collect(),
            key_column: Some("game_id".into()), event_time_column: Some("ts".into()),
            label_column: Some("won".into()), allowed_labels: Some(vec![0.0, 1.0]),
            prediction_cutoff: Some(5000.0), group_column: Some("team".into()),
        }
    }

    #[test]
    fn clean_data_passes() {
        let ds = base();
        let r = run_all(&ds, Some(&ds), &QualityConfig::default());
        assert!(r.passed, "{:?}", r.checks);
    }

    #[test]
    fn catches_duplicates_and_bad_labels() {
        let mut ds = base();
        ds.rows.push(ds.rows[0].clone());
        ds.rows[3].insert("won".into(), Some(2.0));
        assert_eq!(duplicates(&ds).flagged_rows, vec![20]);
        assert_eq!(wrong_labels(&ds).flagged_rows, vec![3]);
    }

    #[test]
    fn catches_leakage() {
        let mut ds = base();
        ds.rows[5].insert("ts".into(), Some(9999.0));
        let r = data_leakage(&ds);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.flagged_rows, vec![5]);
    }

    #[test]
    fn catches_missing_and_shift() {
        let mut ds = base();
        for i in 0..5 { ds.rows[i].insert("pts".into(), None); }
        assert_eq!(missing_values(&ds, &QualityConfig::default()).status, Status::Fail);
        let mut prod = base();
        for r in prod.rows.iter_mut() { r.insert("pts".into(), Some(300.0)); }
        assert_eq!(distribution_shift(&base(), Some(&prod), &QualityConfig::default()).status, Status::Fail);
    }
}
