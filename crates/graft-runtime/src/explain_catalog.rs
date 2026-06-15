use std::path::Path;

use anyhow::Result;
use clap::CommandFactory;
use graft_core::PlanId;
use graft_store::GraftStore;

use crate::Cli;
use crate::config::{GraftConfig, load_constraint_defs, load_graft_config};
use crate::requirements::{plan_label, promotion_requirement_plan};

pub(crate) fn promote_requirement_explain_line(cwd: &Path) -> String {
    let store = GraftStore::open(cwd);
    match load_graft_config_for_explain(&store) {
        Ok(config) => match promotion_requirement_plan(&config, &[]) {
            Ok(plan) => {
                let required = plan_labels_or_core_only(&plan.constraints);
                format!(
                    "Promotion require source: {}; effective required constraints: {}; CLI `--require` overrides this for one invocation.",
                    plan.source.label(),
                    required
                )
            }
            Err(error) => format!(
                "Promotion require source: missing; {error}. CLI `--require` can supply requirements for one invocation."
            ),
        },
        Err(error) => format!("Promotion require source: unreadable-config; {error}."),
    }
}

fn load_graft_config_for_explain(store: &GraftStore) -> Result<GraftConfig> {
    if store.paths().config().exists() {
        load_graft_config(store)
    } else {
        Ok(GraftConfig::default())
    }
}

/// Build the concept catalog used by `graft explain <id>` from the live
/// clap derive plus curated workflow topics. Every subcommand's `about`
/// becomes a concept summary, and selected lifecycle commands receive richer
/// repository-maintained elaboration for `graft_help`/agent use.
pub(crate) fn build_concept_catalog(cwd: &Path) -> Vec<graft_explain::explain::ConceptDoc> {
    let promote_line = promote_requirement_explain_line(cwd);
    let mut out = Vec::new();
    let cmd = Cli::command();
    for sub in cmd.get_subcommands() {
        let id = sub.get_name().to_string();
        if id == "help" || id == "explain" {
            // `help` is auto-generated; `explain` would be self-referential.
            continue;
        }
        let summary = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
        let mut long_about = sub
            .get_long_about()
            .map(|s| s.to_string())
            .filter(|long| long != &summary);
        if let Some(curated) = graft_explain::explain::curated_concept_long_about(&id) {
            long_about = Some(curated.to_string());
        }
        if id == "promote" {
            long_about = Some(match long_about {
                Some(existing) => format!("{existing}\n{promote_line}"),
                None => promote_line.clone(),
            });
        }
        let see_also = related_concepts(&id);
        out.push(graft_explain::explain::ConceptDoc {
            id,
            summary,
            long_about,
            see_also,
        });
    }
    out.extend(graft_explain::explain::agent_help_concepts());
    out.extend(constraint_concepts(cwd));

    // Add a few concept-only ids that are not clap subcommands but show up in
    // diagnostic see-also references; their summaries come from inline copy.
    out.push(graft_explain::explain::ConceptDoc {
        id: "patch-integrity".to_string(),
        summary: "core invariant: applying a stored change to its base must produce its target"
            .to_string(),
        long_about: Some(
            "Patch integrity is Graft mechanism, not a workspace constraint. It is checked before validation, admission, materialization, and promotion; constraints express additional local policy."
                .to_string(),
        ),
        see_also: vec!["validate".to_string(), "V003".to_string()],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "constraints".to_string(),
        summary: "how graft.toml declares verifiable constraints for candidates and patches"
            .to_string(),
        long_about: Some(
            "Each configured constraint is a top-level `fn name(app: Application) -> Constraint` in constraints.roto. Graft loads a ConstraintDef for the display name and one or more content-addressed Plan leaves; evidence is keyed by PlanId, while CLI flags only filter or require what the file declares."
                .to_string(),
        ),
        see_also: vec![
            "validate".to_string(),
            "admit".to_string(),
            "changed_paths_any_match".to_string(),
            "changed_paths_all_match".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "graft.toml".to_string(),
        summary:
            "project-level graft configuration: [admission.required], [promotion.required], [repos], [promote_targets]"
                .to_string(),
        long_about: None,
        see_also: vec![
            "admit".to_string(),
            "promote".to_string(),
            "constraints".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.unknown".to_string(),
        summary: "evidence verdict: verifier could not decide; treat as not-yet-proven".to_string(),
        long_about: None,
        see_also: vec![
            "validate".to_string(),
            "patch-integrity".to_string(),
            "V003".to_string(),
        ],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.failed".to_string(),
        summary: "evidence verdict: verifier observed the constraint violated for this candidate"
            .to_string(),
        long_about: None,
        see_also: vec!["validate".to_string(), "candidates".to_string()],
    });
    out.push(graft_explain::explain::ConceptDoc {
        id: "evidence-result.passed".to_string(),
        summary: "evidence verdict: verifier observed the constraint holding for this candidate"
            .to_string(),
        long_about: None,
        see_also: vec!["admit".to_string(), "promote".to_string()],
    });
    out
}

pub(crate) fn plan_labels_or_core_only(constraints: &[PlanId]) -> String {
    if constraints.is_empty() {
        "none (core integrity only)".to_string()
    } else {
        constraints
            .iter()
            .map(plan_label)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn constraint_concepts(cwd: &Path) -> Vec<graft_explain::explain::ConceptDoc> {
    let store = GraftStore::open(cwd);
    let Ok(constraints) = load_constraint_defs(&store) else {
        return Vec::new();
    };
    constraints
        .into_iter()
        .filter(|(id, _)| graft_explain::constraints::metadata_for_evaluator(id).is_none())
        .map(|(id, constraint)| graft_explain::explain::ConceptDoc {
            id,
            summary: constraint_summary(&constraint),
            long_about: Some(constraint_long_about(&constraint)),
            see_also: constraint_see_also(&constraint),
        })
        .collect()
}

fn constraint_summary(def: &graft_core::ConstraintDef) -> String {
    format!(
        "configured constraint: body {}, description {} byte(s)",
        def.body_id().unwrap_or_else(|_| "<invalid>".to_string()),
        def.description.len()
    )
}

fn constraint_long_about(def: &graft_core::ConstraintDef) -> String {
    format!(
        "Constraint `{}` is loaded from constraints.roto as `fn name(app: Application) -> Constraint`. Graft derives identity from the constraint body; name and description are labels.",
        def.name
    )
}

fn constraint_see_also(_def: &graft_core::ConstraintDef) -> Vec<String> {
    vec!["constraints".to_string(), "constraints.roto".to_string()]
}

/// Hand-curated, single-line list of related concept ids per subcommand.
/// Kept tiny on purpose: the structural relations between commands are not
/// derivable from clap, so this is the one place where we accept manual
/// upkeep, in line with the project's "compiler-as-documentation" rule.
fn related_concepts(id: &str) -> Vec<String> {
    let pairs: &[(&str, &[&str])] = &[
        (
            "init",
            &["agent-workflow", "scratch", "candidate", "graft.toml"],
        ),
        ("scratch", &["agent-workflow", "candidate"]),
        (
            "candidate",
            &["agent-workflow", "scratch", "validate", "candidates"],
        ),
        ("candidates", &["candidate", "validate", "show"]),
        ("show", &["candidate", "evidence"]),
        (
            "validate",
            &[
                "agent-workflow",
                "candidate",
                "admit",
                "patch-integrity",
                "V003",
            ],
        ),
        (
            "admit",
            &[
                "agent-workflow",
                "validate",
                "search",
                "materialize",
                "A001",
                "A002",
            ],
        ),
        ("search", &["admit", "constraints"]),
        ("compose", &["candidate", "migrate"]),
        ("migrate", &["compose"]),
        ("revert", &["candidate", "admit"]),
        ("materialize", &["agent-workflow", "admit", "promote"]),
        (
            "promote",
            &["agent-workflow", "materialize", "admit", "graft.toml"],
        ),
        ("registry", &["admit", "search"]),
        ("cache", &["candidate", "candidates"]),
        ("evidence", &["validate", "admit", "constraints"]),
        ("gc", &["evidence", "candidates", "registry"]),
    ];
    pairs
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}
