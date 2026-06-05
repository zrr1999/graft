use graft_core::{
    ApplicationEndpoint, ApplicationPlan, CheckPlan, FileRefPlan, HistorySelector, OverlayPlan,
    PathSetPlan, ProbePlan, ProbePolarity, PropertyName, PropertyPlan, PropertySourceRef,
    PropertySpec, RunPlan, RunSelectorPlan, Severity, TreePlan,
};
use roto::{List, NoCtx, RotoString, Runtime, Val, library};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Application {
    plan: ApplicationPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Property {
    plan: PropertyPlan,
    description: String,
    severity: Severity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathSet {
    plan: PathSetPlan,
    // Roto 0.11 custom-value interop is unsafe for zero-sized host values.
    // Keep this wrapper non-zero while still lowering to graft-core's
    // semantic PathSetPlan.
    _non_zero: u8,
}

fn strings(list: List<RotoString>) -> Vec<String> {
    list.to_vec()
        .into_iter()
        .map(|value| value.to_string())
        .collect()
}

fn collect_checks(list: List<Val<CheckPlan>>) -> Vec<CheckPlan> {
    list.to_vec().into_iter().map(|check| check.0).collect()
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

fn property_names(list: List<RotoString>) -> Vec<PropertyName> {
    strings(list).into_iter().map(PropertyName::new).collect()
}

fn expect_probe(probe: Val<ProbePlan>, polarity: ProbePolarity) -> Val<CheckPlan> {
    Val(CheckPlan::Expect {
        probe: probe.0,
        polarity,
    })
}

#[allow(non_snake_case)]
fn runtime() -> Runtime<NoCtx> {
    Runtime::from_lib(library! {
        #[clone] type Application = Val<Application>;
        #[clone] type Property = Val<Property>;
        #[clone] type SeverityValue = Val<Severity>;
        #[clone] type Check = Val<CheckPlan>;
        #[clone] type Probe = Val<ProbePlan>;
        #[clone] type PathSet = Val<PathSet>;
        #[clone] type Tree = Val<TreePlan>;
        #[clone] type Run = Val<RunPlan>;
        #[clone] type FileRef = Val<FileRefPlan>;
        #[clone] type Overlay = Val<OverlayPlan>;
        #[clone] type RunSelector = Val<RunSelectorPlan>;
        #[clone] type HistorySelector = Val<HistorySelector>;

        mod Severity {
            const Blocking: Val<Severity> = Val(Severity::Blocking);
            const Warning: Val<Severity> = Val(Severity::Warning);
            const Info: Val<Severity> = Val(Severity::Info);
        }

        mod History {
            const First: Val<HistorySelector> = Val(HistorySelector::First);
            const Last: Val<HistorySelector> = Val(HistorySelector::Last);

            fn Get(index: u64) -> Val<HistorySelector> {
                Val(HistorySelector::Get { index })
            }
        }

        const stdout: Val<RunSelectorPlan> = Val(RunSelectorPlan::Stdout);
        const stderr: Val<RunSelectorPlan> = Val(RunSelectorPlan::Stderr);

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

            fn changed_paths(_app: Val<Application>) -> Val<PathSet> {
                Val(PathSet {
                    plan: PathSetPlan::ChangedPaths,
                    _non_zero: 1,
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
                })
            }
        }

        impl Val<PathSet> {
            fn any_match(paths: Val<PathSet>, patterns: List<RotoString>) -> Val<ProbePlan> {
                Val(ProbePlan::PathMatch {
                    paths: paths.plan.clone(),
                    patterns: strings(patterns),
                })
            }

            fn all_match(paths: Val<PathSet>, patterns: List<RotoString>) -> Val<ProbePlan> {
                Val(ProbePlan::PathAllMatch {
                    paths: paths.plan.clone(),
                    patterns: strings(patterns),
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

        impl Val<RunPlan> {
            fn exit_code_is(run: Val<RunPlan>, code: i32) -> Val<ProbePlan> {
                Val(ProbePlan::RunExitCodeIs { run: run.0, code })
            }
        }

        impl Val<ProbePlan> {
            fn success(probe: Val<ProbePlan>) -> Val<CheckPlan> {
                expect_probe(probe, ProbePolarity::Success)
            }

            fn failure(probe: Val<ProbePlan>) -> Val<CheckPlan> {
                expect_probe(probe, ProbePolarity::Failure)
            }
        }

        fn call(argv: List<RotoString>, tree: Val<TreePlan>) -> Val<RunPlan> {
            Val(RunPlan {
                argv: strings(argv),
                tree: tree.0,
            })
        }

        fn same_output(
            left: Val<RunPlan>,
            right: Val<RunPlan>,
            selectors: List<Val<RunSelectorPlan>>,
        ) -> Val<ProbePlan> {
            Val(ProbePlan::SameOutput {
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

        fn all_of(checks: List<Val<CheckPlan>>) -> Val<CheckPlan> {
            Val(CheckPlan::AllOf {
                checks: collect_checks(checks),
            })
        }

        fn any_of(checks: List<Val<CheckPlan>>) -> Val<CheckPlan> {
            Val(CheckPlan::AnyOf {
                checks: collect_checks(checks),
            })
        }

        fn unavailable(reason: RotoString) -> Val<CheckPlan> {
            Val(CheckPlan::Unavailable {
                reason: reason.to_string(),
            })
        }

        fn property(
            checks: List<Val<CheckPlan>>,
            description: RotoString,
            severity: Val<Severity>,
            requires: List<RotoString>,
        ) -> Val<Property> {
            Val(Property {
                plan: PropertyPlan {
                    checks: collect_checks(checks),
                    requires: property_names(requires),
                },
                description: description.to_string(),
                severity: severity.0,
            })
        }
    })
    .expect("roto library should register")
}

fn current_app() -> Val<Application> {
    Val(Application {
        plan: ApplicationPlan::Current,
    })
}

fn compile_fixture() -> roto::Package<NoCtx> {
    runtime()
        .compile("tests/fixtures/properties.roto")
        .expect("v2 properties.roto should type-check")
}

fn property_spec(function_name: &str, property: Property) -> PropertySpec {
    PropertySpec {
        name: PropertyName::new(function_name),
        plan: property.plan,
        description: property.description,
        severity: property.severity,
        source_ref: Some(PropertySourceRef {
            path: "tests/fixtures/properties.roto".to_string(),
            function: PropertyName::new(function_name),
        }),
    }
}

#[test]
fn roto_v2_property_functions_return_static_property_plans() {
    let mut compiled = compile_fixture();
    let app = current_app();

    let no_generated = compiled
        .get_function::<fn(Val<Application>) -> Val<Property>>("no_generated_artifacts")
        .expect("no_generated_artifacts function")
        .call(app.clone())
        .0;
    assert_eq!(
        no_generated.description,
        "patch does not contain generated build artifacts"
    );
    assert_eq!(no_generated.severity, Severity::Blocking);
    assert_eq!(no_generated.plan.requires, Vec::<PropertyName>::new());
    assert_eq!(
        no_generated.plan.checks,
        vec![CheckPlan::Expect {
            probe: ProbePlan::PathMatch {
                paths: PathSetPlan::ChangedPaths,
                patterns: vec![
                    "target/**".to_string(),
                    "dist/**".to_string(),
                    "build/**".to_string(),
                ],
            },
            polarity: ProbePolarity::Failure,
        }]
    );

    let cargo_tests = compiled
        .get_function::<fn(Val<Application>) -> Val<Property>>("cargo_tests_pass")
        .expect("cargo_tests_pass function")
        .call(app.clone())
        .0;
    assert_eq!(
        cargo_tests.plan.checks,
        vec![CheckPlan::Expect {
            probe: ProbePlan::RunExitCodeIs {
                run: RunPlan {
                    argv: vec![
                        "cargo".to_string(),
                        "test".to_string(),
                        "--all-targets".to_string()
                    ],
                    tree: TreePlan::Application {
                        application: ApplicationPlan::Current,
                        endpoint: ApplicationEndpoint::Target,
                    },
                },
                code: 0,
            },
            polarity: ProbePolarity::Success,
        }]
    );
}

#[test]
fn roto_v2_requires_replace_registry_and_property_function_composition() {
    let mut compiled = compile_fixture();

    let safe_patch = compiled
        .get_function::<fn(Val<Application>) -> Val<Property>>("safe_patch")
        .expect("safe_patch function")
        .call(current_app())
        .0;

    assert_eq!(safe_patch.plan.checks, Vec::<CheckPlan>::new());
    assert_eq!(
        safe_patch.plan.requires,
        vec![
            PropertyName::new("no_generated_artifacts"),
            PropertyName::new("cargo_tests_pass"),
        ]
    );

    let spec = property_spec("safe_patch", safe_patch);
    let id_with_metadata = spec.property_id().expect("property id");
    let mut changed_metadata = spec.clone();
    changed_metadata.description = "metadata-only change".to_string();
    changed_metadata.severity = Severity::Info;
    assert_eq!(
        id_with_metadata,
        changed_metadata
            .property_id()
            .expect("metadata-only drift must not affect property id")
    );

    assert!(
        compiled
            .get_function::<fn() -> ()>("property_registry")
            .is_err(),
        "v2 fixtures must not expose property_registry()"
    );
}

#[test]
fn roto_v2_relational_and_historical_plans_are_symbolic() {
    let mut compiled = compile_fixture();
    let app = current_app();

    let precision = compiled
        .get_function::<fn(Val<Application>) -> Val<Property>>("precision_invariance")
        .expect("precision_invariance function")
        .call(app.clone())
        .0;
    assert_eq!(
        precision.plan.checks,
        vec![CheckPlan::Expect {
            probe: ProbePlan::SameOutput {
                left: RunPlan {
                    argv: vec!["bash".to_string(), "./run.sh".to_string()],
                    tree: TreePlan::Application {
                        application: ApplicationPlan::Current,
                        endpoint: ApplicationEndpoint::Base,
                    },
                },
                right: RunPlan {
                    argv: vec!["bash".to_string(), "./run.sh".to_string()],
                    tree: TreePlan::Application {
                        application: ApplicationPlan::Current,
                        endpoint: ApplicationEndpoint::Target,
                    },
                },
                selectors: vec![
                    RunSelectorPlan::PostFile {
                        path: "./alignment/expected.json".to_string(),
                    },
                    RunSelectorPlan::Stdout,
                ],
            },
            polarity: ProbePolarity::Success,
        }]
    );

    let training = compiled
        .get_function::<fn(Val<Application>) -> Val<Property>>("training_alignment")
        .expect("training_alignment function")
        .call(app)
        .0;

    let CheckPlan::AnyOf { checks } = &training.plan.checks[1] else {
        panic!("second training_alignment check should be any_of");
    };
    assert_eq!(checks.len(), 2);
    assert_eq!(
        checks[1],
        CheckPlan::Expect {
            probe: ProbePlan::RunExitCodeIs {
                run: RunPlan {
                    argv: vec!["bash".to_string(), "./check_diff.sh".to_string()],
                    tree: TreePlan::WithOverlay {
                        base: Box::new(TreePlan::Application {
                            application: ApplicationPlan::PreviousFailure {
                                selector: HistorySelector::First,
                            },
                            endpoint: ApplicationEndpoint::Target,
                        }),
                        overlays: vec![OverlayPlan::ReplaceFile {
                            path: "./check_diff.sh".to_string(),
                            file: FileRefPlan::TreeFile {
                                tree: Box::new(TreePlan::Application {
                                    application: ApplicationPlan::Current,
                                    endpoint: ApplicationEndpoint::Target,
                                }),
                                path: "./check_diff.sh".to_string(),
                            },
                        }],
                    },
                },
                code: 0,
            },
            polarity: ProbePolarity::Failure,
        }
    );
}
