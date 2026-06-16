use std::path::PathBuf;

use graft_core::{Constraint, EvidenceRecord, EvidenceResult, Plan, PlanId};

#[derive(Debug, thiserror::Error)]
pub enum ValidateError {
    #[error(transparent)]
    Core(#[from] graft_core::CoreError),
}

pub type Result<T> = std::result::Result<T, ValidateError>;

#[derive(Clone, Debug)]
pub struct ValidationSubject {
    pub id: String,
    pub changed_paths: Vec<String>,
    pub base_worktree: Option<PathBuf>,
    pub target_worktree: Option<PathBuf>,
}

impl ValidationSubject {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            changed_paths: Vec::new(),
            base_worktree: None,
            target_worktree: None,
        }
    }

    pub fn with_change(id: impl Into<String>, changed_paths: Vec<String>) -> Self {
        Self {
            id: id.into(),
            changed_paths,
            base_worktree: None,
            target_worktree: None,
        }
    }

    pub fn with_base_worktree(mut self, path: impl Into<PathBuf>) -> Self {
        self.base_worktree = Some(path.into());
        self
    }

    pub fn with_target_worktree(mut self, path: impl Into<PathBuf>) -> Self {
        self.target_worktree = Some(path.into());
        self
    }

    pub fn with_validation_worktree(self, path: impl Into<PathBuf>) -> Self {
        self.with_target_worktree(path)
    }
}

pub fn validate_plan(
    subject: &ValidationSubject,
    plan: &Plan,
    result: EvidenceResult,
) -> Result<EvidenceRecord> {
    let plan_id = plan.plan_id()?;
    evidence_for_plan_id(subject, plan_id, result)
}

pub fn evidence_for_plan_id(
    subject: &ValidationSubject,
    plan_id: PlanId,
    result: EvidenceResult,
) -> Result<EvidenceRecord> {
    EvidenceRecord::new(
        subject.id.clone(),
        plan_id.clone(),
        verifier_id_for_plan(&plan_id),
        result,
    )
    .map_err(ValidateError::from)
}

pub fn validate_constraint(
    subject: &ValidationSubject,
    constraint: &Constraint,
    evidence: &[EvidenceRecord],
) -> std::result::Result<graft_policy::AdmissionDecision, graft_policy::PolicyError> {
    graft_policy::satisfies_subject(&subject.id, constraint, evidence)
}

pub fn verifier_id_for_plan(plan_id: &PlanId) -> String {
    format!("plan@{plan_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{
        ApplicationEndpoint, ApplicationPlan, Assertion, ObservationPlan, RunPlan, TreePlan,
    };

    fn run_plan(argv: &[&str]) -> Plan {
        Plan {
            observation: ObservationPlan::Run {
                run: RunPlan {
                    argv: argv.iter().map(|value| (*value).to_string()).collect(),
                    tree: TreePlan::Application {
                        application: ApplicationPlan::Current,
                        endpoint: ApplicationEndpoint::Target,
                    },
                },
            },
            assertion: Assertion::ExitCodeIs { code: 0 },
        }
    }

    #[test]
    fn validate_plan_emits_one_evidence_for_content_addressed_plan() {
        let subject = ValidationSubject::new("candidate:demo");
        let plan = run_plan(&["cargo", "test"]);
        let plan_id = plan.plan_id().unwrap();

        let evidence = validate_plan(&subject, &plan, EvidenceResult::Passed).unwrap();

        assert_eq!(evidence.subject, "candidate:demo");
        assert_eq!(evidence.plan, plan_id);
        assert_eq!(evidence.verifier, format!("plan@{}", evidence.plan));
        assert_eq!(evidence.result, EvidenceResult::Passed);
    }

    #[test]
    fn evidence_for_plan_id_preserves_single_leaf_identity() {
        let subject = ValidationSubject::new("candidate:demo");
        let plan_id = PlanId::new("plan:tests_pass");

        let evidence =
            evidence_for_plan_id(&subject, plan_id.clone(), EvidenceResult::Passed).unwrap();

        assert_eq!(evidence.plan, plan_id);
        assert_eq!(evidence.verifier, "plan@plan:tests_pass");
    }

    #[test]
    fn validate_constraint_delegates_to_policy_with_subject_id() {
        let subject = ValidationSubject::new("candidate:demo");
        let plan_id = PlanId::new("plan:tests_pass");
        let constraint = Constraint::primitive(plan_id);

        let error = validate_constraint(&subject, &constraint, &[])
            .unwrap_err()
            .to_string();

        assert!(error.starts_with("[E_CONSTRAINT_UNMET]"), "{error}");
        assert!(
            error.contains("Constraint failed at: primitive plan:tests_pass"),
            "{error}"
        );
    }
}
