use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use anyhow::{Context, Result, bail};
use graft_core::{
    ApplicationEndpoint, ApplicationPlan, CheckPlan, FileRefPlan, HistorySelector, OverlayPlan,
    PathSetPlan, ProbePlan, ProbePolarity, PropertyName, PropertyPlan, PropertySourceRef,
    PropertySpec, RunPlan, RunSelectorPlan, Severity, TreePlan,
};
use roto::{List, NoCtx, RotoString, Runtime, Val, library};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Application {
    plan: ApplicationPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Property {
    plan: PropertyPlan,
    description: String,
    severity: Severity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathSet {
    plan: PathSetPlan,
    // Roto 0.11 custom-value interop is unsafe for zero-sized host values.
    // Keep this wrapper non-zero while still lowering to graft-core's semantic
    // PathSetPlan inside methods.
    _non_zero: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RotoFunction {
    name: String,
    normalized_signature: String,
}

pub(crate) fn load_roto_property_specs(path: &Path) -> Result<BTreeMap<String, PropertySpec>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let sanitized = sanitize_roto_for_scanning(&text);
    if sanitized.chars().all(char::is_whitespace) {
        return Ok(BTreeMap::new());
    }

    let property_functions = collect_roto_functions(&text, &sanitized)?
        .into_iter()
        .filter(|function| {
            function.normalized_signature
                == format!("fn{}(app:Application)->Property", function.name)
        })
        .collect::<Vec<_>>();

    let compile_result = catch_unwind(AssertUnwindSafe(|| runtime().compile(path)));
    let mut package = match compile_result {
        Ok(Ok(package)) => package,
        Ok(Err(error)) => {
            bail!(
                "[E_PROPERTY_COMPILE] {} failed to compile: {error}",
                path.display()
            )
        }
        Err(_) => bail!(
            "[E_PROPERTY_COMPILE] {} triggered a Roto compiler panic",
            path.display()
        ),
    };

    let mut out = BTreeMap::new();
    for function in property_functions {
        let compiled = package
            .get_function::<fn(Val<Application>) -> Val<Property>>(&function.name)
            .with_context(|| {
                format!(
                    "[E_PROPERTY_COMPILE] top-level property function `{}` does not match `fn name(app: Application) -> Property` after typechecking",
                    function.name
                )
            })?;
        let property = compiled.call(current_app()).0;
        let spec = PropertySpec {
            name: PropertyName::new(function.name.clone()),
            plan: property.plan,
            description: property.description,
            severity: property.severity,
            source_ref: Some(PropertySourceRef {
                path: path.display().to_string(),
                function: PropertyName::new(function.name.clone()),
            }),
        };
        if out.insert(function.name.clone(), spec).is_some() {
            bail!(
                "[E_DUPLICATE_PROPERTY] duplicate property function `{}`",
                function.name
            );
        }
    }
    Ok(out)
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

            #[allow(non_snake_case)]
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
    .expect("roto property library should register")
}

fn current_app() -> Val<Application> {
    Val(Application {
        plan: ApplicationPlan::Current,
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
        let path = dir.join("properties.roto");
        std::fs::write(&path, source).unwrap();
        path
    }

    #[test]
    fn registered_roto_host_values_are_not_zero_sized() {
        assert_ne!(std::mem::size_of::<Application>(), 0);
        assert_ne!(std::mem::size_of::<Property>(), 0);
        assert_ne!(std::mem::size_of::<Severity>(), 0);
        assert_ne!(std::mem::size_of::<CheckPlan>(), 0);
        assert_ne!(std::mem::size_of::<ProbePlan>(), 0);
        assert_ne!(std::mem::size_of::<PathSet>(), 0);
        assert_ne!(std::mem::size_of::<TreePlan>(), 0);
        assert_ne!(std::mem::size_of::<RunPlan>(), 0);
        assert_ne!(std::mem::size_of::<FileRefPlan>(), 0);
        assert_ne!(std::mem::size_of::<OverlayPlan>(), 0);
        assert_ne!(std::mem::size_of::<RunSelectorPlan>(), 0);
        assert_ne!(std::mem::size_of::<HistorySelector>(), 0);
    }

    #[test]
    fn loads_v2_roto_property_specs() {
        let path = temp_roto(
            "basic",
            r#"
fn empty_change(app: Application) -> Property {
    property(
        [app.changed_paths().any_match(["**"]).failure()],
        "the change touches no paths",
        Severity.Blocking,
        [],
    )
}

fn docs_only(app: Application) -> Property {
    property(
        [app.changed_paths().all_match(["docs/**", "README.md"]).success()],
        "docs only",
        Severity.Warning,
        ["empty_change"],
    )
}
"#,
        );

        let specs = load_roto_property_specs(&path).unwrap();
        assert_eq!(
            specs.keys().cloned().collect::<Vec<_>>(),
            vec!["docs_only", "empty_change"]
        );
        let docs = specs.get("docs_only").unwrap();
        assert_eq!(docs.name.as_str(), "docs_only");
        assert_eq!(docs.description, "docs only");
        assert_eq!(docs.severity, Severity::Warning);
        assert_eq!(docs.plan.requires, vec![PropertyName::new("empty_change")]);
        assert!(
            docs.property_id()
                .unwrap()
                .as_str()
                .starts_with("property:")
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn ignores_non_property_helper_functions_for_discovery() {
        let path = temp_roto(
            "helper",
            r#"
fn helper(app: Application) -> Check {
    unavailable("not yet")
}

fn real_property(app: Application) -> Property {
    property([unavailable("not yet")], "real", Severity.Info, [])
}
"#,
        );

        let specs = load_roto_property_specs(&path).unwrap();
        assert_eq!(
            specs.keys().cloned().collect::<Vec<_>>(),
            vec!["real_property"]
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn property_helpers_can_pass_registered_application_without_roto_ice() {
        let path = temp_roto(
            "helper-application",
            r#"
fn docs_change(app: Application) -> Check {
    app.changed_paths().all_match(["docs/**", "README.md"]).success()
}

fn docs_only(app: Application) -> Property {
    property([docs_change(app)], "docs only", Severity.Info, [])
}
"#,
        );

        let specs = load_roto_property_specs(&path).unwrap();
        assert_eq!(specs.keys().cloned().collect::<Vec<_>>(), vec!["docs_only"]);
        let docs = specs.get("docs_only").unwrap();
        assert_eq!(docs.plan.checks.len(), 1);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
