use std::collections::BTreeMap;

use graft_core::{
    ApplicationEndpoint, ApplicationPlan, Assertion, Constraint, FileRefPlan, HistorySelector,
    ObservationPlan, OverlayPlan, Plan, PlanId, RunPlan, RunSelectorPlan, TreePlan,
};
use roto::{List, NoCtx, RotoString, Runtime, Val, library};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Application {
    plan: ApplicationPlan,
    _non_zero: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConstraintValue {
    body: Constraint,
    plans: BTreeMap<PlanId, Plan>,
}

impl ConstraintValue {
    fn primitive(observation: ObservationPlan, assertion: Assertion) -> Self {
        let plan = Plan {
            observation,
            assertion,
        };
        let plan_id = plan.plan_id().expect("test plans serialize");
        Self {
            body: Constraint::primitive(plan_id.clone()),
            plans: BTreeMap::from([(plan_id, plan)]),
        }
    }

    fn combine(
        left: ConstraintValue,
        right: ConstraintValue,
        make: impl FnOnce(Constraint, Constraint) -> Constraint,
    ) -> Self {
        let mut plans = left.plans;
        plans.extend(right.plans);
        Self {
            body: make(left.body, right.body),
            plans,
        }
    }
}

fn strings(list: List<RotoString>) -> Vec<String> {
    list.to_vec()
        .into_iter()
        .map(|value| value.to_string())
        .collect()
}

fn collect_selectors(list: List<Val<RunSelectorPlan>>) -> Vec<RunSelectorPlan> {
    list.to_vec()
        .into_iter()
        .map(|selector| selector.0)
        .collect()
}

fn collect_overlays(list: List<Val<OverlayPlan>>) -> Vec<OverlayPlan> {
    list.to_vec().into_iter().map(|overlay| overlay.0).collect()
}

#[allow(non_snake_case)]
fn runtime() -> Runtime<NoCtx> {
    Runtime::from_lib(library! {
        #[clone] type Application = Val<Application>;
        #[clone] type Constraint = Val<ConstraintValue>;
        #[clone] type Observation = Val<ObservationPlan>;
        #[clone] type Assertion = Val<Assertion>;
        #[clone] type Tree = Val<TreePlan>;
        #[clone] type Run = Val<RunPlan>;
        #[clone] type FileRef = Val<FileRefPlan>;
        #[clone] type Overlay = Val<OverlayPlan>;
        #[clone] type RunSelector = Val<RunSelectorPlan>;
        #[clone] type HistorySelector = Val<HistorySelector>;

        mod History {
            const First: Val<HistorySelector> = Val(HistorySelector::First);
            const Last: Val<HistorySelector> = Val(HistorySelector::Last);

            fn Get(index: u64) -> Val<HistorySelector> {
                Val(HistorySelector::Get { index })
            }
        }

        const stdout: Val<RunSelectorPlan> = Val(RunSelectorPlan::Stdout);
        const stderr: Val<RunSelectorPlan> = Val(RunSelectorPlan::Stderr);

        const any_match: Val<Assertion> = Val(Assertion::PathsAnyMatch);
        const all_match: Val<Assertion> = Val(Assertion::PathsAllMatch);
        const no_match: Val<Assertion> = Val(Assertion::PathsNoMatch);
        const not_all_match: Val<Assertion> = Val(Assertion::PathsNotAllMatch);
        const exit_zero: Val<Assertion> = Val(Assertion::ExitCodeIs { code: 0 });
        const exit_nonzero: Val<Assertion> = Val(Assertion::ExitCodeIsNot { code: 0 });
        const outputs_same: Val<Assertion> = Val(Assertion::OutputsSame);
        const outputs_differ: Val<Assertion> = Val(Assertion::OutputsDiffer);

        impl Val<Application> {
            fn base(app: Val<Application>) -> Val<TreePlan> {
                Val(TreePlan::Application {
                    application: app.plan.clone(),
                    endpoint: ApplicationEndpoint::Base,
                })
            }

            fn target(app: Val<Application>) -> Val<TreePlan> {
                Val(TreePlan::Application {
                    application: app.plan.clone(),
                    endpoint: ApplicationEndpoint::Target,
                })
            }

            fn changed_paths(_app: Val<Application>, patterns: List<RotoString>) -> Val<ObservationPlan> {
                Val(ObservationPlan::ChangedPaths {
                    patterns: strings(patterns),
                })
            }

            fn run(app: Val<Application>, argv: List<RotoString>) -> Val<ObservationPlan> {
                Val(ObservationPlan::Run {
                    run: RunPlan {
                        argv: strings(argv),
                        tree: TreePlan::Application {
                            application: app.plan.clone(),
                            endpoint: ApplicationEndpoint::Target,
                        },
                    },
                })
            }

            fn previous_failure(
                _app: Val<Application>,
                selector: Val<HistorySelector>,
            ) -> Val<Application> {
                Val(Application {
                    plan: ApplicationPlan::PreviousFailure {
                        selector: selector.0,
                    },
                    _non_zero: 1,
                })
            }
        }

        impl Val<TreePlan> {
            fn file(tree: Val<TreePlan>, path: RotoString) -> Val<FileRefPlan> {
                Val(FileRefPlan::TreeFile {
                    tree: Box::new(tree.0),
                    path: path.to_string(),
                })
            }

            fn with_overlay(tree: Val<TreePlan>, overlays: List<Val<OverlayPlan>>) -> Val<TreePlan> {
                Val(TreePlan::WithOverlay {
                    base: Box::new(tree.0),
                    overlays: collect_overlays(overlays),
                })
            }
        }

        fn call(argv: List<RotoString>, tree: Val<TreePlan>) -> Val<RunPlan> {
            Val(RunPlan {
                argv: strings(argv),
                tree: tree.0,
            })
        }

        fn observe_run(run: Val<RunPlan>) -> Val<ObservationPlan> {
            Val(ObservationPlan::Run { run: run.0 })
        }

        fn same_output(
            left: Val<RunPlan>,
            right: Val<RunPlan>,
            selectors: List<Val<RunSelectorPlan>>,
        ) -> Val<ObservationPlan> {
            Val(ObservationPlan::SameOutput {
                left: left.0,
                right: right.0,
                selectors: collect_selectors(selectors),
            })
        }

        fn post_file(path: RotoString) -> Val<RunSelectorPlan> {
            Val(RunSelectorPlan::PostFile {
                path: path.to_string(),
            })
        }

        fn replace_file(path: RotoString, file: Val<FileRefPlan>) -> Val<OverlayPlan> {
            Val(OverlayPlan::ReplaceFile {
                path: path.to_string(),
                file: file.0,
            })
        }

        fn primitive(
            observation: Val<ObservationPlan>,
            assertion: Val<Assertion>,
            _description: RotoString,
        ) -> Val<ConstraintValue> {
            Val(ConstraintValue::primitive(observation.0, assertion.0))
        }

        fn both(left: Val<ConstraintValue>, right: Val<ConstraintValue>) -> Val<ConstraintValue> {
            Val(ConstraintValue::combine(left.0, right.0, |left, right| {
                Constraint::Both {
                    left: Box::new(left),
                    right: Box::new(right),
                }
            }))
        }

        fn either(left: Val<ConstraintValue>, right: Val<ConstraintValue>) -> Val<ConstraintValue> {
            Val(ConstraintValue::combine(left.0, right.0, |left, right| {
                Constraint::Either {
                    left: Box::new(left),
                    right: Box::new(right),
                }
            }))
        }
    })
    .expect("roto library should register")
}

fn current_app() -> Val<Application> {
    Val(Application {
        plan: ApplicationPlan::Current,
        _non_zero: 1,
    })
}

fn compile_fixture() -> roto::Package<NoCtx> {
    runtime()
        .compile("tests/fixtures/constraints.roto")
        .expect("three-layer constraints.roto should type-check")
}

#[test]
fn roto_constraint_functions_return_primitive_plan_leaves() {
    let mut compiled = compile_fixture();
    let app = current_app();

    let no_generated = compiled
        .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>("no_generated_artifacts")
        .expect("no_generated_artifacts function")
        .call(app.clone())
        .0;
    assert_eq!(no_generated.plans.len(), 1);
    let plan = no_generated.plans.values().next().unwrap();
    assert_eq!(
        plan.observation,
        ObservationPlan::ChangedPaths {
            patterns: vec![
                "target/**".to_string(),
                "dist/**".to_string(),
                "build/**".to_string(),
            ],
        }
    );
    assert_eq!(plan.assertion, Assertion::PathsNoMatch);

    let cargo_tests = compiled
        .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>("cargo_tests_pass")
        .expect("cargo_tests_pass function")
        .call(app)
        .0;
    let plan = cargo_tests.plans.values().next().unwrap();
    assert_eq!(plan.assertion, Assertion::ExitCodeIs { code: 0 });
    assert!(matches!(plan.observation, ObservationPlan::Run { .. }));
}

#[test]
fn roto_composes_constraints_without_requires_registry() {
    let mut compiled = compile_fixture();

    let safe_patch = compiled
        .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>("safe_patch")
        .expect("safe_patch function")
        .call(current_app())
        .0;

    assert!(matches!(safe_patch.body, Constraint::Both { .. }));
    assert_eq!(safe_patch.plans.len(), 2);
    assert!(
        compiled
            .get_function::<fn() -> ()>("constraint_registry")
            .is_err(),
        "three-layer fixtures must not expose constraint_registry()"
    );
}

#[test]
fn roto_relational_and_historical_plans_are_symbolic() {
    let mut compiled = compile_fixture();
    let app = current_app();

    let precision = compiled
        .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>("precision_invariance")
        .expect("precision_invariance function")
        .call(app.clone())
        .0;
    let plan = precision.plans.values().next().unwrap();
    assert_eq!(plan.assertion, Assertion::OutputsSame);
    assert!(matches!(
        plan.observation,
        ObservationPlan::SameOutput { ref selectors, .. } if selectors == &vec![
            RunSelectorPlan::PostFile { path: "./alignment/expected.json".to_string() },
            RunSelectorPlan::Stdout,
        ]
    ));

    let training = compiled
        .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>("training_alignment")
        .expect("training_alignment function")
        .call(app)
        .0;

    assert!(matches!(training.body, Constraint::Both { .. }));
    assert!(training.plans.values().any(|plan| matches!(
        plan.observation,
        ObservationPlan::Run {
            run: RunPlan {
                tree: TreePlan::WithOverlay { .. },
                ..
            }
        }
    )));
    assert!(training.plans.values().any(|plan| {
        matches!(
            plan.observation,
            ObservationPlan::Run {
                run: RunPlan {
                    tree: TreePlan::WithOverlay { ref base, .. },
                    ..
                }
            } if matches!(
                base.as_ref(),
                TreePlan::Application {
                    application: ApplicationPlan::PreviousFailure { selector: HistorySelector::First },
                    endpoint: ApplicationEndpoint::Target,
                }
            )
        )
    }));
}
