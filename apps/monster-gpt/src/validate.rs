//! Post-generation grounding check: every number in the model's answer that
//! looks like a fact (odds, money, percentages, decimals, stats) must appear in
//! the evidence it was given. Numbers that don't are reported to the user as
//! unverified; the answer is not silently rewritten.

use std::collections::HashSet;

/// Numbers as written: "$4", "-230", "+150", "65.3%", "1.5", "81%".
pub fn numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let starts = c.is_ascii_digit() || ((c == '$' || c == '+' || c == '-') && chars.get(i + 1).map(|d| d.is_ascii_digit()).unwrap_or(false));
        // a sign glued to a word ("A-1") is not a number sign
        let glued = i > 0 && (chars[i - 1].is_alphanumeric()) && !c.is_ascii_digit();
        if starts && !(i > 0 && chars[i - 1].is_ascii_digit()) && !glued {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_digit() || (chars[j] == '.' && chars.get(j + 1).map(|d| d.is_ascii_digit()).unwrap_or(false)) || chars[j] == ',') { j += 1; }
            if j < chars.len() && chars[j] == '%' { j += 1; }
            out.push(chars[i..j].iter().collect::<String>().trim_end_matches(',').to_string());
            i = j;
        } else { i += 1; }
    }
    out
}

/// Canonical value for comparison: sign and $ dropped, "%"-kept, trailing zeros trimmed.
fn canon(n: &str) -> Option<(String, bool)> {
    let pct = n.ends_with('%');
    let t = n.trim_start_matches(['$', '+', '-']).trim_end_matches('%').replace(',', "");
    let v: f64 = t.parse().ok()?;
    Some((format!("{}", (v * 100.0).round() / 100.0), pct))
}

/// The value part of [`canon`] ("$1,250.50" -> "1250.5"), for matching against records.
pub(crate) fn canon_value(n: &str) -> Option<String> { canon(n).map(|x| x.0) }

/// Which numbers in `answer` are worth checking: money, odds, percentages,
/// decimals, and integers above 10 (list numbering and counts are left alone).
pub(crate) fn checkable(n: &str) -> bool {
    if n.starts_with('$') || n.ends_with('%') || n.contains('.') { return true; }
    let t = n.trim_start_matches(['+', '-']).replace(',', "");
    match t.parse::<i64>() { Ok(v) => v > 10 && !(1900..=2100).contains(&v), Err(_) => false }
}

/// Numbers in the answer that the evidence does not contain.
pub fn unsupported(answer: &str, evidence: &str) -> Vec<String> {
    let have: HashSet<(String, bool)> = numbers(evidence).iter().filter_map(|n| canon(n)).collect();
    let have_vals: HashSet<String> = have.iter().map(|x| x.0.clone()).collect();
    let mut seen = HashSet::new();
    numbers(answer).into_iter().filter(|n| checkable(n)).filter(|n| {
        let Some((v, pct)) = canon(n) else { return false };
        // a percentage may be written from a probability in the evidence (0.81 -> 81%)
        let ok = have_vals.contains(&v) || (pct && v.parse::<f64>().map(|p| have_vals.contains(&format!("{}", (p / 100.0 * 1000.0).round() / 1000.0))).unwrap_or(false));
        !ok && seen.insert(n.clone())
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_numbers_as_written() {
        assert_eq!(numbers("Makar under 1.5 at -230, stake $4 to win $1.74; model 81% vs 65.3%."), vec!["1.5", "-230", "$4", "$1.74", "81%", "65.3%"]);
        assert_eq!(numbers("CGY-COL and B+ grade, +150 dog"), vec!["+150"]);
        assert_eq!(numbers("1,250 units"), vec!["1,250"]);
    }

    #[test]
    fn flags_numbers_missing_from_evidence() {
        let ev = "Cale Makar Under 1.5 Points -230 at DraftKings. Model 81% vs market 65%. Stake $4 to win $1.74. Total stake $12; expected profit $1.46.";
        let ans = "1. Makar under 1.5 at -230 ($4 to win $1.74) has an 81% model chance. 2. Johnson at -135 adds $1.33 of expected profit.";
        assert_eq!(unsupported(ans, ev), vec!["-135", "$1.33"]);
    }

    #[test]
    fn small_counts_and_years_are_not_checked() {
        assert!(unsupported("Here are 3 bets for the 2026 season.", "nothing").is_empty());
    }

    #[test]
    fn percentages_may_come_from_probabilities() {
        assert!(unsupported("the model gives it 81%", r#"{"p_model": 0.81}"#).is_empty());
    }
}
