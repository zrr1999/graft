//! Trial [`Explainable`] impl for [`graft_core::EvidenceResult`].
//!
//! `EvidenceResult` is graft's most user-facing status enum (every `validate`
//! output is rendered through it), so it is the right shape to validate the
//! [`Explainable`] trait against. The four variants — `Passed`, `Failed`,
//! `Unknown`, `Skipped` — each carry a stable id under the
//! `"evidence-result.*"` namespace so `graft explain evidence-result.unknown`
//! is well-defined once the explain subcommand lands (T6).
//!
//! All summaries here are single-line on purpose: the project rule is that
//! narrative content lives in the structure, not in external docs, and never
//! spans multiple lines.

use crate::Explainable;
use graft_core::EvidenceResult;

/// Wrapper that exposes the user-facing status classification of an
/// `EvidenceResult`. We wrap rather than `impl Explainable for EvidenceResult`
/// directly to keep `graft-core` free of UX-layer responsibilities.
#[derive(Clone, Copy, Debug)]
pub struct EvidenceStatus<'a>(pub &'a EvidenceResult);

impl<'a> EvidenceStatus<'a> {
    pub fn new(result: &'a EvidenceResult) -> Self {
        Self(result)
    }
}

impl<'a> Explainable for EvidenceStatus<'a> {
    fn id(&self) -> &'static str {
        match self.0 {
            EvidenceResult::Passed => "evidence-result.passed",
            EvidenceResult::Failed { .. } => "evidence-result.failed",
            EvidenceResult::Unknown { .. } => "evidence-result.unknown",
            EvidenceResult::Skipped { .. } => "evidence-result.skipped",
        }
    }

    fn summary(&self) -> &'static str {
        match self.0 {
            EvidenceResult::Passed => "verifier observed the constraint holding for this candidate",
            EvidenceResult::Failed { .. } => {
                "verifier observed the constraint violated for this candidate"
            }
            EvidenceResult::Unknown { .. } => "verifier could not decide; treat as not-yet-proven",
            EvidenceResult::Skipped { .. } => {
                "verifier intentionally did not run for this constraint"
            }
        }
    }

    fn see_also(&self) -> &'static [&'static str] {
        match self.0 {
            EvidenceResult::Passed => &["admit", "promote"],
            EvidenceResult::Failed { .. } => &["validate", "drafts"],
            EvidenceResult::Unknown { .. } => &["validate", "valid-patch", "V003"],
            EvidenceResult::Skipped { .. } => &["validate", "constraints"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_has_unique_id_and_nonempty_summary() {
        let cases = [
            EvidenceResult::Passed,
            EvidenceResult::Failed {
                reason: "diff".into(),
            },
            EvidenceResult::Unknown {
                reason: "no base".into(),
            },
            EvidenceResult::Skipped {
                reason: "policy".into(),
            },
        ];
        let mut seen_ids = Vec::new();
        for result in &cases {
            let status = EvidenceStatus::new(result);
            let id = status.id();
            assert!(
                id.starts_with("evidence-result."),
                "id should be namespaced: {id}"
            );
            assert!(
                !seen_ids.contains(&id),
                "duplicate id across variants: {id}"
            );
            seen_ids.push(id);
            assert!(!status.summary().is_empty());
            assert!(
                !status.summary().contains('\n'),
                "summary must be single-line: {:?}",
                status.summary()
            );
        }
        assert_eq!(seen_ids.len(), 4);
    }

    #[test]
    fn unknown_variant_sees_validate_path_concepts() {
        // The unknown variant is used for validation results that cannot be
        // decided yet; its see_also
        // must include the relevant repair surface.
        let result = EvidenceResult::Unknown {
            reason: "base unmaterializable".into(),
        };
        let status = EvidenceStatus::new(&result);
        let see = status.see_also();
        assert!(see.contains(&"validate"), "see_also: {see:?}");
        assert!(see.contains(&"V003"), "see_also: {see:?}");
    }
}
