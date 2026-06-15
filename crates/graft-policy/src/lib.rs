use graft_core::{ApplicationRef, Constraint, EvidenceId, EvidenceRecord, PlanId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConstraintFailure {
    Missing {
        primitive: PlanId,
    },
    NotPassed {
        primitive: PlanId,
        evidence: EvidenceId,
    },
    BothBranch {
        branch_index: usize,
        inner: Box<ConstraintFailure>,
    },
    EitherExhausted {
        branches: Vec<ConstraintFailure>,
    },
    BottomReached,
}

impl ConstraintFailure {
    pub fn first_primitive_name(&self) -> Option<&str> {
        match self {
            Self::Missing { primitive } | Self::NotPassed { primitive, .. } => {
                Some(primitive.as_str())
            }
            Self::BothBranch { inner, .. } => inner.first_primitive_name(),
            Self::EitherExhausted { branches } => {
                branches.iter().find_map(Self::first_primitive_name)
            }
            Self::BottomReached => None,
        }
    }

    pub fn path(&self) -> String {
        let mut segments = Vec::new();
        self.push_path_segments(&mut segments);
        segments.join("/")
    }

    fn push_path_segments(&self, segments: &mut Vec<String>) {
        match self {
            Self::Missing { primitive } | Self::NotPassed { primitive, .. } => {
                segments.push(format!("primitive {}", primitive));
            }
            Self::BothBranch {
                branch_index,
                inner,
            } => {
                segments.push("all_of".to_string());
                segments.push(format!("[{branch_index}]"));
                inner.push_path_segments(segments);
            }
            Self::EitherExhausted { .. } => {
                segments.push("any_of".to_string());
            }
            Self::BottomReached => segments.push("bottom".to_string()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "[E_CONSTRAINT_UNMET] missing required evidence for `{constraint}` @ Constraint failed at: {path}"
    )]
    MissingEvidence { constraint: String, path: String },
    #[error(
        "[E_CONSTRAINT_UNMET] evidence `{evidence}` for `{constraint}` did not pass @ Constraint failed at: {path}"
    )]
    EvidenceNotPassed {
        constraint: String,
        evidence: String,
        path: String,
    },
    #[error(
        "[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET] constraint is bottom and can never be satisfied @ Constraint failed at: bottom"
    )]
    BottomReached,
    #[error(
        "[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET] constraint was not satisfied @ Constraint failed at: {path}; {detail}"
    )]
    ConstraintUnsatisfied {
        path: String,
        detail: String,
        failure: ConstraintFailure,
    },
}

pub type Result<T> = std::result::Result<T, PolicyError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionDecision {
    pub accepted: bool,
}

pub fn satisfies(
    application: &ApplicationRef,
    constraint: &Constraint,
    evidence: &[EvidenceRecord],
) -> Result<AdmissionDecision> {
    satisfies_subject(application.as_subject(), constraint, evidence)
}

pub fn satisfies_subject(
    subject: &str,
    constraint: &Constraint,
    evidence: &[EvidenceRecord],
) -> Result<AdmissionDecision> {
    match evaluate(subject, constraint, evidence) {
        Ok(()) => Ok(AdmissionDecision { accepted: true }),
        Err(failure) => Err(policy_error_from_failure(failure)),
    }
}

fn policy_error_from_failure(failure: ConstraintFailure) -> PolicyError {
    match failure {
        ConstraintFailure::Missing { primitive } => PolicyError::MissingEvidence {
            constraint: primitive.to_string(),
            path: format!("primitive {}", primitive),
        },
        ConstraintFailure::NotPassed {
            primitive,
            evidence,
        } => PolicyError::EvidenceNotPassed {
            constraint: primitive.to_string(),
            evidence: evidence.to_string(),
            path: format!("primitive {}", primitive),
        },
        ConstraintFailure::BothBranch {
            branch_index,
            inner,
        } => match policy_error_from_failure(*inner) {
            PolicyError::MissingEvidence { constraint, path } => PolicyError::MissingEvidence {
                constraint,
                path: format!("all_of/[{branch_index}]/{path}"),
            },
            PolicyError::EvidenceNotPassed {
                constraint,
                evidence,
                path,
            } => PolicyError::EvidenceNotPassed {
                constraint,
                evidence,
                path: format!("all_of/[{branch_index}]/{path}"),
            },
            PolicyError::BottomReached => PolicyError::ConstraintUnsatisfied {
                path: format!("all_of/[{branch_index}]/bottom"),
                detail: "bottom branch can never be satisfied".to_string(),
                failure: ConstraintFailure::BottomReached,
            },
            PolicyError::ConstraintUnsatisfied {
                path,
                detail,
                failure,
            } => PolicyError::ConstraintUnsatisfied {
                path: format!("all_of/[{branch_index}]/{path}"),
                detail,
                failure,
            },
        },
        ConstraintFailure::EitherExhausted { branches } => PolicyError::ConstraintUnsatisfied {
            path: "any_of".to_string(),
            detail: format!("no branch satisfied; failures: {:?}", branches),
            failure: ConstraintFailure::EitherExhausted { branches },
        },
        ConstraintFailure::BottomReached => PolicyError::BottomReached,
    }
}

fn evaluate(
    subject: &str,
    constraint: &Constraint,
    evidence: &[EvidenceRecord],
) -> std::result::Result<(), ConstraintFailure> {
    match constraint {
        Constraint::Top => Ok(()),
        Constraint::Bottom => Err(ConstraintFailure::BottomReached),
        Constraint::Primitive { plan } => evaluate_primitive(subject, plan, evidence),
        Constraint::Both { left, right } => {
            evaluate(subject, left, evidence).map_err(|inner| ConstraintFailure::BothBranch {
                branch_index: 0,
                inner: Box::new(inner),
            })?;
            evaluate(subject, right, evidence).map_err(|inner| ConstraintFailure::BothBranch {
                branch_index: 1,
                inner: Box::new(inner),
            })
        }
        Constraint::Either { left, right } => {
            let left_failure = match evaluate(subject, left, evidence) {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            };
            let right_failure = match evaluate(subject, right, evidence) {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            };
            Err(ConstraintFailure::EitherExhausted {
                branches: vec![left_failure, right_failure],
            })
        }
    }
}

fn evaluate_primitive(
    subject: &str,
    primitive: &PlanId,
    evidence: &[EvidenceRecord],
) -> std::result::Result<(), ConstraintFailure> {
    let mut matching = evidence
        .iter()
        .filter(|record| record.subject == subject && record.plan == *primitive);
    let Some(first) = matching.next() else {
        return Err(ConstraintFailure::Missing {
            primitive: primitive.clone(),
        });
    };
    if first.result.satisfies_requirement()
        || matching.any(|record| record.result.satisfies_requirement())
    {
        Ok(())
    } else {
        Err(ConstraintFailure::NotPassed {
            primitive: primitive.clone(),
            evidence: first.id.clone(),
        })
    }
}

trait ApplicationSubject {
    fn as_subject(&self) -> &str;
}

impl ApplicationSubject for ApplicationRef {
    fn as_subject(&self) -> &str {
        match self {
            ApplicationRef::Stored(id) => id.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{ApplicationId, PlanId};

    fn plan(name: &str) -> PlanId {
        PlanId::new(format!("plan:{name}"))
    }

    fn primitive(name: &str) -> Constraint {
        Constraint::primitive(plan(name))
    }

    fn application() -> ApplicationRef {
        ApplicationRef::Stored(ApplicationId::new("application:demo"))
    }

    fn passed(name: &str) -> EvidenceRecord {
        EvidenceRecord::passed("application:demo", plan(name), "test-verifier").unwrap()
    }

    fn failed(name: &str) -> EvidenceRecord {
        EvidenceRecord::failed(
            "application:demo",
            plan(name),
            "test-verifier",
            "test failed",
        )
        .unwrap()
    }

    #[test]
    fn top_is_always_satisfied() {
        let decision = satisfies(&application(), &Constraint::Top, &[]).unwrap();
        assert!(decision.accepted);
    }

    #[test]
    fn bottom_is_never_satisfied() {
        let err = satisfies(&application(), &Constraint::Bottom, &[]).unwrap_err();
        assert!(matches!(err, PolicyError::BottomReached));
    }

    #[test]
    fn passed_primitive_satisfies_policy_even_after_failed_attempt() {
        let evidence = vec![failed("TestsPass"), passed("TestsPass")];

        let decision = satisfies(&application(), &primitive("TestsPass"), &evidence).unwrap();

        assert!(decision.accepted);
    }

    #[test]
    fn failed_primitive_without_a_pass_does_not_satisfy_policy() {
        let evidence = vec![failed("TestsPass")];

        let err = satisfies(&application(), &primitive("TestsPass"), &evidence).unwrap_err();

        assert!(matches!(err, PolicyError::EvidenceNotPassed { .. }));
    }

    #[test]
    fn primitive_requires_matching_application_subject() {
        let evidence = vec![
            EvidenceRecord::passed("candidate:demo", plan("TestsPass"), "test-verifier").unwrap(),
        ];

        let err = satisfies(&application(), &primitive("TestsPass"), &evidence).unwrap_err();

        assert!(matches!(err, PolicyError::MissingEvidence { .. }));
    }

    #[test]
    fn both_short_circuits_on_first_missing_branch() {
        let constraint = Constraint::all_of(vec![primitive("TestsPass"), primitive("FormatPass")]);
        let err = satisfies(&application(), &constraint, &[]).unwrap_err();
        let rendered = err.to_string();

        assert!(matches!(err, PolicyError::MissingEvidence { .. }));
        assert!(rendered.starts_with("[E_CONSTRAINT_UNMET]"), "{rendered}");
        assert!(
            rendered.contains("Constraint failed at: all_of/[0]/primitive plan:TestsPass"),
            "{rendered}"
        );
    }

    #[test]
    fn nested_failed_primitive_reports_evidence_id_and_path() {
        let constraint = Constraint::all_of(vec![primitive("TestsPass"), primitive("FormatPass")]);
        let failed = failed("FormatPass");
        let evidence_id = failed.id.to_string();
        let err =
            satisfies(&application(), &constraint, &[passed("TestsPass"), failed]).unwrap_err();
        let rendered = err.to_string();

        assert!(matches!(err, PolicyError::EvidenceNotPassed { .. }));
        assert!(rendered.starts_with("[E_CONSTRAINT_UNMET]"), "{rendered}");
        assert!(rendered.contains(&evidence_id), "{rendered}");
        assert!(
            rendered.contains("Constraint failed at: all_of/[1]/primitive plan:FormatPass"),
            "{rendered}"
        );
    }

    #[test]
    fn either_succeeds_when_any_branch_passes() {
        let constraint = Constraint::any_of(vec![Constraint::Bottom, primitive("TestsPass")]);
        let decision = satisfies(&application(), &constraint, &[passed("TestsPass")]).unwrap();

        assert!(decision.accepted);
    }

    #[test]
    fn either_reports_all_branch_failures() {
        let constraint = Constraint::any_of(vec![Constraint::Bottom, primitive("TestsPass")]);
        let err = satisfies(&application(), &constraint, &[]).unwrap_err();
        let rendered = err.to_string();

        let PolicyError::ConstraintUnsatisfied { failure, path, .. } = err else {
            panic!("expected structured constraint failure");
        };
        assert_eq!(path, "any_of");
        assert!(
            matches!(failure, ConstraintFailure::EitherExhausted { branches } if branches.len() == 2)
        );
        assert!(rendered.starts_with("[E_CONSTRAINT_UNMET]"), "{rendered}");
        assert!(rendered.contains("[E_ADMISSION_UNMET]"), "{rendered}");
        assert!(
            rendered.contains("Constraint failed at: any_of"),
            "{rendered}"
        );
    }

    #[test]
    fn nested_lattice_constraint_satisfies() {
        let constraint = Constraint::all_of(vec![
            primitive("TestsPass"),
            Constraint::any_of(vec![Constraint::Bottom, primitive("FormatPass")]),
        ]);
        let evidence = vec![passed("TestsPass"), passed("FormatPass")];

        let decision = satisfies(&application(), &constraint, &evidence).unwrap();

        assert!(decision.accepted);
    }
}
