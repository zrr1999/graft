use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use anyhow::{Context, Result, bail};
use graft_core::{
    ApplicationEndpoint, ApplicationPlan, Assertion, Constraint, ConstraintDef, FileRefPlan,
    HistorySelector, Observation, OverlayPlan, Plan, PlanId, RunPlan, RunSelectorPlan, TreePlan,
};
use roto::{List, NoCtx, RotoString, Runtime, Val, library};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Application {
    plan: ApplicationPlan,
    // Roto 0.11 custom-value interop is unsafe for zero-sized host values.
    _non_zero: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConstraintValue {
    body: Constraint,
    description: Option<String>,
    plans: BTreeMap<PlanId, Plan>,
}

impl ConstraintValue {
    fn new(body: Constraint, description: Option<String>, plans: BTreeMap<PlanId, Plan>) -> Self {
        Self {
            body,
            description,
            plans,
        }
    }

    fn combine(
        left: ConstraintValue,
        right: ConstraintValue,
        make: impl FnOnce(Constraint, Constraint) -> Constraint,
    ) -> Self {
        let mut plans = left.plans;
        plans.extend(right.plans);
        Self::new(make(left.body, right.body), None, plans)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RotoFunction {
    name: String,
    normalized_signature: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LoadedConstraints {
    pub(crate) defs: BTreeMap<String, ConstraintDef>,
    pub(crate) plans: BTreeMap<PlanId, Plan>,
}

pub(crate) fn load_roto_constraint_defs(path: &Path) -> Result<LoadedConstraints> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let sanitized = sanitize_roto_for_scanning(&text);
    if sanitized.chars().all(char::is_whitespace) {
        return Ok(LoadedConstraints::default());
    }

    let constraint_functions = collect_roto_functions(&text, &sanitized)?
        .into_iter()
        .filter(|function| {
            function.normalized_signature
                == format!("fn{}(app:Application)->Constraint", function.name)
        })
        .collect::<Vec<_>>();

    let compile_result = catch_unwind(AssertUnwindSafe(|| runtime().compile(path)));
    let mut package = match compile_result {
        Ok(Ok(package)) => package,
        Ok(Err(error)) => {
            bail!(
                "[E_CONSTRAINT_COMPILE] {} failed to compile: {error}",
                path.display()
            )
        }
        Err(_) => bail!(
            "[E_CONSTRAINT_COMPILE] {} triggered a Roto compiler panic",
            path.display()
        ),
    };

    let mut out = LoadedConstraints::default();
    for function in constraint_functions {
        let compiled = package
            .get_function::<fn(Val<Application>) -> Val<ConstraintValue>>(&function.name)
            .with_context(|| {
                format!(
                    "[E_CONSTRAINT_COMPILE] top-level constraint function `{}` does not match `fn name(app: Application) -> Constraint` after typechecking",
                    function.name
                )
            })?;
        let value = compiled.call(current_app()).0;
        let def = ConstraintDef {
            name: function.name.clone(),
            description: value
                .description
                .clone()
                .unwrap_or_else(|| function.name.clone()),
            body: value.body,
        };
        if out.defs.insert(function.name.clone(), def).is_some() {
            bail!(
                "[E_DUPLICATE_CONSTRAINT] duplicate constraint function `{}`",
                function.name
            );
        }
        out.plans.extend(value.plans);
    }
    Ok(out)
}

fn strings(list: List<RotoString>) -> Vec<String> {
    list.to_vec()
        .into_iter()
        .map(|value| value.to_string())
        .collect()
}

fn collect_constraints(list: List<Val<ConstraintValue>>) -> Vec<ConstraintValue> {
    list.to_vec()
        .into_iter()
        .map(|constraint| constraint.0)
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

fn make_primitive_constraint(
    observation: Val<Observation>,
    assertion: Val<Assertion>,
    description: RotoString,
) -> Val<ConstraintValue> {
    let plan = Plan {
        observation: observation.0,
        assertion: assertion.0,
    };
    let plan_id = plan
        .plan_id()
        .expect("Plan serialization should be infallible for Roto-built plans");
    Val(ConstraintValue::new(
        Constraint::primitive(plan_id.clone()),
        Some(description.to_string()),
        BTreeMap::from([(plan_id, plan)]),
    ))
}

fn combine_many(
    items: List<Val<ConstraintValue>>,
    empty: Constraint,
    make: impl Fn(Vec<Constraint>) -> Constraint,
) -> Val<ConstraintValue> {
    let values = collect_constraints(items);
    let mut plans = BTreeMap::new();
    let mut bodies = Vec::new();
    for value in values {
        plans.extend(value.plans);
        bodies.push(value.body);
    }
    let body = if bodies.is_empty() {
        empty
    } else {
        make(bodies)
    };
    Val(ConstraintValue::new(body, None, plans))
}

#[allow(non_snake_case)]
fn runtime() -> Runtime<NoCtx> {
    Runtime::from_lib(library! {
        #[clone] type Application = Val<Application>;
        #[clone] type Constraint = Val<ConstraintValue>;
        #[clone] type Observation = Val<Observation>;
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

            #[allow(non_snake_case)]
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

            fn run(app: Val<Application>, argv: List<RotoString>) -> Val<Observation> {
                Val(Observation::Run {
                    run: RunPlan {
                        argv: strings(argv),
                        tree: TreePlan::Application {
                            application: app.plan.clone(),
                            endpoint: ApplicationEndpoint::Target,
                        },
                    },
                })
            }

            fn changed_paths(_app: Val<Application>, patterns: List<RotoString>) -> Val<Observation> {
                Val(Observation::ChangedPaths {
                    patterns: strings(patterns),
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

        fn observe_run(run: Val<RunPlan>) -> Val<Observation> {
            Val(Observation::Run { run: run.0 })
        }

        fn same_output(
            left: Val<RunPlan>,
            right: Val<RunPlan>,
            selectors: List<Val<RunSelectorPlan>>,
        ) -> Val<Observation> {
            Val(Observation::SameOutput {
                left: left.0,
                right: right.0,
                selectors: collect_selectors(selectors),
            })
        }

        fn exit_code_is(code: i32) -> Val<Assertion> {
            Val(Assertion::ExitCodeIs { code })
        }

        fn exit_code_is_not(code: i32) -> Val<Assertion> {
            Val(Assertion::ExitCodeIsNot { code })
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
            observation: Val<Observation>,
            assertion: Val<Assertion>,
            description: RotoString,
        ) -> Val<ConstraintValue> {
            make_primitive_constraint(observation, assertion, description)
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

        fn all_of(items: List<Val<ConstraintValue>>) -> Val<ConstraintValue> {
            combine_many(items, Constraint::Top, Constraint::all_of)
        }

        fn either_any(items: List<Val<ConstraintValue>>) -> Val<ConstraintValue> {
            combine_many(items, Constraint::Bottom, Constraint::any_of)
        }

        fn unavailable(reason: RotoString) -> Val<ConstraintValue> {
            make_primitive_constraint(
                Val(Observation::Unavailable { reason: reason.to_string() }),
                Val(Assertion::Unavailable),
                reason,
            )
        }
    })
    .expect("roto constraint library should register")
}

fn current_app() -> Val<Application> {
    Val(Application {
        plan: ApplicationPlan::Current,
        _non_zero: 1,
    })
}

fn sanitize_roto_for_scanning(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut sanitized = bytes.to_vec();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    sanitized[i] = b' ';
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let quote = bytes[i];
                sanitized[i] = b' ';
                i += 1;
                while i < bytes.len() {
                    let escaped = bytes[i] == b'\\';
                    sanitized[i] = if bytes[i] == b'\n' { b'\n' } else { b' ' };
                    i += 1;
                    if escaped && i < bytes.len() {
                        sanitized[i] = if bytes[i] == b'\n' { b'\n' } else { b' ' };
                        i += 1;
                    } else if bytes.get(i - 1) == Some(&quote) {
                        break;
                    }
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(sanitized).expect("sanitized Roto source remains valid UTF-8")
}

fn collect_roto_functions(source: &str, sanitized: &str) -> Result<Vec<RotoFunction>> {
    let bytes = sanitized.as_bytes();
    let mut functions = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 && keyword_at(bytes, i, b"fn") => {
                let start = i;
                i += 2;
                i = skip_ascii_whitespace(bytes, i);
                let name_start = i;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                if name_start == i {
                    bail!("[E_PROPERTIES_ROTO_PARSE] expected function name after `fn`");
                }
                let name = sanitized[name_start..i].to_string();
                let open = bytes[i..]
                    .iter()
                    .position(|byte| *byte == b'{')
                    .map(|offset| i + offset)
                    .with_context(|| {
                        format!("[E_PROPERTIES_ROTO_PARSE] function `{name}` is missing a body")
                    })?;
                let close = matching_brace(bytes, open).with_context(|| {
                    format!("[E_PROPERTIES_ROTO_PARSE] function `{name}` has an unterminated body")
                })?;
                let normalized_signature = source[start..open]
                    .chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>();
                functions.push(RotoFunction {
                    name,
                    normalized_signature,
                });
                i = close + 1;
            }
            _ => i += 1,
        }
    }
    Ok(functions)
}

fn keyword_at(bytes: &[u8], index: usize, keyword: &[u8]) -> bool {
    bytes.get(index..index + keyword.len()) == Some(keyword)
        && index
            .checked_sub(1)
            .is_none_or(|before| !is_ident_continue(bytes[before]))
        && bytes
            .get(index + keyword.len())
            .is_none_or(|after| !is_ident_continue(*after))
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn is_ident_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_ident_continue(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_roto(name: &str, source: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "graft-runtime-roto-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("constraints.roto");
        std::fs::write(&path, source).unwrap();
        path
    }

    #[test]
    fn registered_roto_host_values_are_not_zero_sized() {
        assert_ne!(std::mem::size_of::<Application>(), 0);
        assert_ne!(std::mem::size_of::<ConstraintValue>(), 0);
        assert_ne!(std::mem::size_of::<Observation>(), 0);
        assert_ne!(std::mem::size_of::<Assertion>(), 0);
        assert_ne!(std::mem::size_of::<TreePlan>(), 0);
        assert_ne!(std::mem::size_of::<RunPlan>(), 0);
        assert_ne!(std::mem::size_of::<FileRefPlan>(), 0);
        assert_ne!(std::mem::size_of::<OverlayPlan>(), 0);
        assert_ne!(std::mem::size_of::<RunSelectorPlan>(), 0);
        assert_ne!(std::mem::size_of::<HistorySelector>(), 0);
    }

    #[test]
    fn loads_three_layer_constraint_defs() {
        let path = temp_roto(
            "basic",
            r#"
fn empty_change(app: Application) -> Constraint {
    primitive(app.changed_paths(["**"]), no_match, "the change touches no paths")
}

fn cargo_tests_pass(app: Application) -> Constraint {
    primitive(app.run(["cargo", "test", "--all-targets"]), exit_zero, "cargo tests pass")
}

fn safe_patch(app: Application) -> Constraint {
    both(empty_change(app), cargo_tests_pass(app))
}
"#,
        );

        let loaded = load_roto_constraint_defs(&path).unwrap();
        assert_eq!(
            loaded.defs.keys().cloned().collect::<Vec<_>>(),
            vec!["cargo_tests_pass", "empty_change", "safe_patch"]
        );
        assert_eq!(
            loaded.defs["empty_change"].description,
            "the change touches no paths"
        );
        assert!(matches!(
            loaded.defs["safe_patch"].body,
            Constraint::Both { .. }
        ));
        assert_eq!(loaded.plans.len(), 2);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn constraint_helpers_can_call_other_constraint_functions() {
        let path = temp_roto(
            "helper-application",
            r#"
fn docs_change(app: Application) -> Constraint {
    primitive(app.changed_paths(["docs/**", "README.md"]), all_match, "docs only")
}

fn docs_or_empty(app: Application) -> Constraint {
    either(docs_change(app), primitive(app.changed_paths(["**"]), no_match, "empty"))
}
"#,
        );

        let loaded = load_roto_constraint_defs(&path).unwrap();
        assert_eq!(loaded.plans.len(), 2);
        assert!(matches!(
            loaded.defs["docs_or_empty"].body,
            Constraint::Either { .. }
        ));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
