//! Pure metric math: commit weights, decay, co-change accumulation, ghost
//! scoring, and evidence blending. Everything here is deterministic and
//! testable without a database.

use crate::Evidence;

pub const SECONDS_PER_DAY: f64 = 86_400.0;

/// Defaults shared by the indexer, the analyzer, and the CLI.
#[derive(Debug, Clone)]
pub struct Params {
    /// Hard exclusion threshold: commits touching more files than this get
    /// weight 0 and the TOO_LARGE flag.
    pub max_commit_files: usize,
    /// Half-life in days for recency decay of commit weight.
    pub half_life_days: f64,
    /// Minimum raw co-occurrence count before a pure co-change pair is
    /// reportable.
    pub min_cochange_n: i64,
    /// Minimum lift before a pure co-change pair is reportable.
    pub min_lift: f64,
    /// An edge must have lived at least this long for its death to count as
    /// a ghost candidate. Shorter-lived edges were scaffolding.
    pub ghost_min_lifetime_days: i64,
    /// Post-severance gates: both must hold or the pair is not a ghost.
    pub ghost_min_cochanges_since: i64,
    pub ghost_min_conf_since: f64,
    /// Rename similarity threshold, 0.0 to 1.0.
    pub rename_threshold: f64,
    /// Fraction of changed lines that must be import lines for a touch to be
    /// flagged import_only.
    pub import_only_ratio: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            max_commit_files: 50,
            half_life_days: 365.0,
            min_cochange_n: 3,
            min_lift: 1.5,
            ghost_min_lifetime_days: 90,
            ghost_min_cochanges_since: 2,
            ghost_min_conf_since: 0.25,
            rename_threshold: 0.5,
            import_only_ratio: 0.9,
        }
    }
}

/// Weight of a single commit: 1/n_files, or 0 if excluded.
pub fn commit_weight(n_files: usize, excluded: bool) -> f64 {
    if excluded || n_files == 0 {
        0.0
    } else {
        1.0 / n_files as f64
    }
}

/// Exponential recency decay: 0.5^(age_days / half_life).
pub fn decay(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    0.5_f64.powf(age_days.max(0.0) / half_life_days)
}

/// Accumulator for one unordered file pair.
#[derive(Debug, Clone, Copy, Default)]
pub struct PairAcc {
    pub n: i64,
    pub w_support: f64,
    pub w_decayed: f64,
    pub first_commit: i64,
    pub last_commit: i64,
}

impl PairAcc {
    pub fn add(&mut self, commit_id: i64, w: f64, wd: f64) {
        if self.n == 0 {
            self.first_commit = commit_id;
        }
        self.n += 1;
        self.w_support += w;
        self.w_decayed += wd;
        self.last_commit = commit_id;
    }
}

/// conf(b|a) = w(a,b) / w(a). Zero denominator yields zero, not NaN.
pub fn confidence(w_ab: f64, w_a: f64) -> f64 {
    if w_a <= 0.0 {
        0.0
    } else {
        (w_ab / w_a).min(1.0)
    }
}

/// lift(a,b) = w(a,b) / (w(a) * w(b) / W). Values above 1 mean the pair
/// co-occurs more than chance given each file's own churn.
pub fn lift(w_ab: f64, w_a: f64, w_b: f64, w_total: f64) -> f64 {
    let expected = w_a * w_b / w_total.max(f64::MIN_POSITIVE);
    if expected <= 0.0 {
        0.0
    } else {
        w_ab / expected
    }
}

/// Ranking score for a ghost edge, per plan section 4.2. The post-severance
/// gates are applied by the caller; this is ordering only.
pub fn ghost_score(
    conf_since: f64,
    cochanges_since: i64,
    severance_age_days: f64,
    lifetime_days: i64,
    half_life_days: f64,
) -> f64 {
    conf_since
        * (1.0 + cochanges_since as f64).ln()
        * decay(severance_age_days, half_life_days)
        * (1.0 + lifetime_days as f64).ln()
}

/// Map a single piece of evidence to a strength in [0, 1) for ranking.
/// The evidence itself is always emitted alongside; this only orders rows.
pub fn evidence_strength(e: &Evidence) -> f64 {
    match e {
        Evidence::Structural { kind, alive, .. } => {
            let base = match kind {
                crate::EdgeKind::Import => 0.55,
                crate::EdgeKind::Reexport => 0.50,
                crate::EdgeKind::Call => 0.45,
                crate::EdgeKind::TypeRef => 0.40,
                crate::EdgeKind::DocRef => 0.25,
            };
            if *alive {
                base
            } else {
                0.0
            }
        }
        Evidence::Ghost {
            confidence_since,
            cochanges_since,
            ..
        } => {
            let s = confidence_since * (1.0 + *cochanges_since as f64).ln() / 3.0_f64.ln();
            (0.35 + 0.55 * s.min(1.0)).min(0.9)
        }
        Evidence::Cochange {
            confidence, lift, ..
        } => (confidence * (lift / 5.0).min(1.0)).min(0.85),
        Evidence::Transitive { union_dist, .. } => 0.25 / (*union_dist as f64).max(1.0),
        Evidence::DocDrift { commits_behind, .. } => {
            (0.1 + 0.05 * (*commits_behind as f64).ln_1p()).min(0.4)
        }
    }
}

/// Blend evidence strengths with noisy-or so independent signals reinforce
/// without any single one saturating the rank.
pub fn blend_rank(evidence: &[Evidence]) -> f64 {
    let mut miss = 1.0;
    for e in evidence {
        miss *= 1.0 - evidence_strength(e).clamp(0.0, 0.999);
    }
    1.0 - miss
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_and_decay() {
        assert_eq!(commit_weight(4, false), 0.25);
        assert_eq!(commit_weight(4, true), 0.0);
        assert!((decay(365.0, 365.0) - 0.5).abs() < 1e-12);
        assert_eq!(decay(0.0, 365.0), 1.0);
    }

    #[test]
    fn lift_flags_chance_cooccurrence() {
        // Two files each in 40% of weighted commits, co-occurring at the
        // rate chance predicts, must get lift about 1.0.
        let w_total = 100.0;
        let w_a = 40.0;
        let w_b = 40.0;
        let w_ab = 16.0;
        assert!((lift(w_ab, w_a, w_b, w_total) - 1.0).abs() < 1e-12);
        // A genuinely coupled pair beats chance.
        assert!(lift(30.0, 40.0, 40.0, 100.0) > 1.5);
    }

    #[test]
    fn confidence_handles_zero() {
        assert_eq!(confidence(1.0, 0.0), 0.0);
        assert_eq!(confidence(1.0, 2.0), 0.5);
    }

    #[test]
    fn ghost_score_ordering() {
        // Stronger post-severance coupling must outrank weaker, all else equal.
        let hi = ghost_score(0.8, 9, 100.0, 600, 365.0);
        let lo = ghost_score(0.3, 2, 100.0, 600, 365.0);
        assert!(hi > lo);
        // Older severance decays.
        let recent = ghost_score(0.5, 5, 30.0, 600, 365.0);
        let ancient = ghost_score(0.5, 5, 2000.0, 600, 365.0);
        assert!(recent > ancient);
    }

    #[test]
    fn blend_is_monotone_and_bounded() {
        let g = Evidence::Ghost {
            kind: crate::EdgeKind::Import,
            severed_at: "abc".into(),
            severed_date: "2024-11-03".into(),
            lifetime_days: 612,
            cochanges_since: 9,
            confidence_since: 0.64,
            severing_subject: "extract iface".into(),
            direction: crate::Direction::Out,
        };
        let c = Evidence::Cochange {
            n: 9,
            support: 2.0,
            confidence: 0.64,
            lift: 4.1,
            last: "2026-06-21".into(),
        };
        let one = blend_rank(std::slice::from_ref(&g));
        let two = blend_rank(&[g, c]);
        assert!(two > one);
        assert!(two < 1.0);
    }
}
