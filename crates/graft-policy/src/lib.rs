use graft_core::{EvidenceRecord, PropertyRef};
use graft_explain::diagnostics::{a001_missing_required_evidence, a002_failed_required_evidence};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("{}", a001_missing_required_evidence(property).format_reason())]
    MissingEvidence { property: String },
    #[error("{}", a002_failed_required_evidence(property).format_reason())]
    EvidenceNotPassed { property: String },
}

pub type Result<T> = std::result::Result<T, PolicyError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionDecision {
    pub accepted: bool,
}

pub fn require_passed_evidence(
    required: &[PropertyRef],
    evidence: &[EvidenceRecord],
) -> Result<AdmissionDecision> {
    for property in required {
        let mut matching = evidence
            .iter()
            .filter(|record| record.property == property.id);
        let Some(first) = matching.next() else {
            return Err(PolicyError::MissingEvidence {
                property: property.name.clone(),
            });
        };
        if !first.result.satisfies_requirement()
            && !matching.any(|record| record.result.satisfies_requirement())
        {
            return Err(PolicyError::EvidenceNotPassed {
                property: property.name.clone(),
            });
        }
    }
    Ok(AdmissionDecision { accepted: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::PropertyId;

    fn property(name: &str) -> PropertyRef {
        PropertyRef::new(PropertyId::new(format!("property:{name}")), name)
    }

    #[test]
    fn passed_evidence_satisfies_policy() {
        let property = property("TestsPass");
        let evidence =
            EvidenceRecord::passed("candidate:demo", property.id.clone(), "test-verifier").unwrap();
        let decision = require_passed_evidence(&[property], &[evidence]).unwrap();
        assert!(decision.accepted);
    }

    #[test]
    fn passed_evidence_satisfies_policy_even_after_failed_attempt() {
        let property = property("TestsPass");
        let evidence = vec![
            EvidenceRecord::failed(
                "candidate:demo",
                property.id.clone(),
                "test-verifier",
                "first run failed",
            )
            .unwrap(),
            EvidenceRecord::passed("candidate:demo", property.id.clone(), "test-verifier").unwrap(),
        ];

        let decision = require_passed_evidence(&[property], &evidence).unwrap();

        assert!(decision.accepted);
    }

    #[test]
    fn failed_evidence_without_a_pass_does_not_satisfy_policy() {
        let property = property("TestsPass");
        let evidence = vec![
            EvidenceRecord::failed(
                "candidate:demo",
                property.id.clone(),
                "test-verifier",
                "test failed",
            )
            .unwrap(),
        ];

        let err = require_passed_evidence(&[property], &evidence).unwrap_err();

        assert!(matches!(err, PolicyError::EvidenceNotPassed { .. }));
    }

    #[test]
    fn evidence_for_different_property_id_does_not_satisfy_requirement() {
        let required = property("TestsPass");
        let old_property = property("TestsPassOld");
        let evidence = vec![
            EvidenceRecord::passed("candidate:demo", old_property.id, "test-verifier").unwrap(),
        ];

        let err = require_passed_evidence(&[required], &evidence).unwrap_err();

        assert!(matches!(err, PolicyError::MissingEvidence { .. }));
    }
}
