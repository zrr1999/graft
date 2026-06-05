//! Stand-alone lookup engine behind `graft explain <id>`.
//!
//! Three id namespaces are routed here:
//!
//! - **Diagnostic codes**: `V003`, `A007`, etc. Source: [`super::diagnostics`].
//! - **Builtin evaluators**: `changed_paths_any_match`, `changed_paths_all_match`, etc. Source:
//!   [`super::properties`].
//! - **Concept ids**: free-form names like `admit`, `materialize`,
//!   `agent-workflow`, `properties`, `valid-patch`. The catalog is supplied by
//!   the caller (typically the CLI, so it can read clap-derived `about` strings
//!   as the single source of truth).
//!
//! Unknown ids return [`ExplainResult::Unknown`] with a small list of
//! "did you mean" suggestions ranked by Levenshtein distance.
//!
//! This module is `serde`-friendly: every `ExplainResult` is a structured
//! payload that the CLI can render as Markdown-free human text or stream as
//! `--json`.

use crate::diagnostics::{ALL_DIAGNOSTICS, DiagnosticDoc, doc_for};
use crate::properties::{ALL_BUILTINS, BuiltinEvaluatorMetadata, metadata_for_evaluator};
use crate::{DiagCode, Diagnostic};
use serde::Serialize;

/// One concept-card supplied by the caller. CLI populates this from
/// clap-derived `about`/`long_about` so the spelling and copy never drift
/// from the actual `--help` output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConceptDoc {
    /// Stable id used by `graft explain <id>`.
    pub id: String,
    /// Single-line summary; for clap subcommands this is the `about` value.
    pub summary: String,
    /// Optional elaboration; `long_about` from clap or curated workflow copy.
    /// Curated help topics may contain multiple plain-text lines.
    pub long_about: Option<String>,
    /// Related concept ids and/or diagnostic codes to nudge users toward.
    pub see_also: Vec<String>,
}

/// Result of a `graft explain <id>` lookup.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExplainResult {
    /// `id` matched a concept (typically a clap subcommand).
    Concept(ConceptDoc),
    /// `id` matched a `[Vnnn]`-style diagnostic code.
    Diagnostic(DiagnosticView),
    /// `id` matched a builtin evaluator id.
    BuiltinEvaluator(BuiltinEvaluatorView),
    /// `id` did not match any of the three namespaces.
    Unknown {
        id: String,
        /// Up to 3 closest known ids ranked by Levenshtein distance.
        did_you_mean: Vec<String>,
    },
}

/// Serializable view over a [`DiagnosticDoc`].
#[derive(Debug, Serialize)]
pub struct DiagnosticView {
    pub code: String,
    pub domain: char,
    pub summary: &'static str,
    pub fix_hints: &'static [&'static str],
    pub see_also: &'static [&'static str],
}

impl From<&'static DiagnosticDoc> for DiagnosticView {
    fn from(doc: &'static DiagnosticDoc) -> Self {
        Self {
            code: doc.code.to_string(),
            domain: doc.code.domain.letter(),
            summary: doc.summary,
            fix_hints: doc.fix_hints,
            see_also: doc.see_also,
        }
    }
}

/// Serializable view over a [`BuiltinEvaluatorMetadata`].
#[derive(Debug, Serialize)]
pub struct BuiltinEvaluatorView {
    pub id: &'static str,
    pub summary: &'static str,
    pub input: &'static str,
    pub predicate: &'static str,
    pub requires_base: bool,
    pub failure_modes: &'static [&'static str],
}

impl From<&'static BuiltinEvaluatorMetadata> for BuiltinEvaluatorView {
    fn from(meta: &'static BuiltinEvaluatorMetadata) -> Self {
        Self {
            id: meta.id,
            summary: meta.summary,
            input: meta.input,
            predicate: meta.predicate,
            requires_base: meta.requires_base,
            failure_modes: meta.failure_modes,
        }
    }
}

const AGENT_WORKFLOW_LONG_ABOUT: &str = concat!(
    "Recommended workflow for agents and pi-graft tools:\n",
    "1. Use `graft explain agent-workflow` or pi-graft `graft_help` when unsure; direct tool descriptions should stay short.\n",
    "2. Bootstrap and diagnose with `graft workspace init`, `graft workspace ps`, and `graft workspace doctor` before writing changes.\n",
    "3. Draft only in scratch: use `graft scratch read|write|edit|delete --base <base> ...` and continue with `--from scratch:<digest>`, or use `graft scratch capture --base <base>` to stash-like capture cwd changes into scratch; scratch is daemon-backed draft state, not a candidate, patch, sync object, or Git ref.\n",
    "4. Turn draft into reviewable state with `graft patch from-scratch scratch:<digest> --expect <Property> --message <msg>`; the patch from-scratch command generates a private local candidate and does not admit or promote it.\n",
    "5. Prove properties with `graft patch validate candidate:<digest> --expect <Property>` and inspect with `graft patch show`, `graft patch list --candidates`, or `graft patch search`.\n",
    "6. Admit only after required evidence passes: `graft patch admit candidate:<digest> --require <Property>`; admit generates a public patch and moves candidate evidence refs to the patch.\n",
    "7. Check output with `graft materialize <state-ref>` or `graft run <state-ref> -- <cmd>`; materialize writes isolated `.worktrees/<state>/` inspection output, not cwd or Git refs.\n",
    "8. External promote is low-frequency and explicit: only run `graft promote <patch-id> --to <target> --yes` when an approved patch must update an outside Git branch, PR, or release target.\n",
    "9. Low-frequency advanced write commands such as patch compose/migrate/revert, sync, repo add/sync/lock/update, bundle import, workspace gc --apply, and patch promote may use pi-graft `graft_cli_exec` argv; keep read/inspect commands on the local CLI path and keep high-frequency agents on typed scratch/patch ops plus validate/admit/show/materialize."
);

const SCRATCH_LONG_ABOUT: &str = concat!(
    "Scratch is only for draft file graph operations. Use `--base graft:empty|tree:<id>|candidate:<id>|patch:<id>` for the first read/write/edit/delete and `--from scratch:<digest>` for each continuation.\n",
    "Scratch never creates a candidate or patch, never syncs, and never updates Git refs. `graft scratch capture --base <ref>` is the explicit stash-like cwd bridge: it captures cwd into scratch and restores captured paths to the base. Encode rename as delete plus write, then leave scratch with `graft patch from-scratch`."
);

const CANDIDATE_LONG_ABOUT: &str = concat!(
    "Candidate is the local-only proposal state. `graft patch from-scratch scratch:<digest>` generates a private candidate from a scratch draft, expected properties, provenance producer, and optional message.\n",
    "A candidate is not public review history and is not synced; validate it to produce evidence, then admit it when required evidence passes."
);

const VALIDATE_LONG_ABOUT: &str = concat!(
    "Validate runs configured verifiers for an explicit candidate, patch, or change and records evidence.\n",
    "Validation proves properties but does not admit, materialize, promote, or otherwise move lifecycle state."
);

const ADMIT_LONG_ABOUT: &str = concat!(
    "Admit is the candidate to patch boundary. It checks required properties against passed evidence, then generates a public patch and moves evidence refs from private candidate storage to public patch storage.\n",
    "Admit does not capture cwd, does not materialize output, and does not update external Git targets; run scratch/candidate first and promote only as a separate explicit operation."
);

const MATERIALIZE_LONG_ABOUT: &str = concat!(
    "Materialize is inspection output for a resolved state. Inputs such as tree:<digest>, candidate:<digest>, patch:<digest>, repo:<id>@<treeish>, and workspace Git treeishes first resolve to a StateId, then write an isolated `.worktrees/<state>/` directory under the workspace.\n",
    "It is safe for checking files because it does not update cwd, evidence, branches, PRs, releases, or external refs; use promote separately only when external publication is intended."
);

const PROMOTE_LONG_ABOUT: &str = concat!(
    "Promote is the explicit external side-effect boundary for a patch. It projects an admitted patch to a configured or command-line Git branch, PR, or release target and only applies when the user supplies the `--yes` gate.\n",
    "Treat promote as a low-frequency advanced operation; agents should usually inspect with materialize and run promote manually through the CLI or pi-graft `graft_cli_exec` when publication is truly requested."
);

/// Stable topic cards intended for `graft_help` and `graft explain <topic>`.
pub fn agent_help_concepts() -> Vec<ConceptDoc> {
    vec![
        ConceptDoc {
            id: "agent-workflow".to_string(),
            summary: "recommended Graft lifecycle for pi-graft agents and tool callers".to_string(),
            long_about: Some(AGENT_WORKFLOW_LONG_ABOUT.to_string()),
            see_also: vec![
                "scratch".to_string(),
                "candidate".to_string(),
                "validate".to_string(),
                "admit".to_string(),
                "materialize".to_string(),
                "promote".to_string(),
            ],
        },
        ConceptDoc {
            id: "workflow".to_string(),
            summary: "alias for the recommended agent workflow help topic".to_string(),
            long_about: Some(AGENT_WORKFLOW_LONG_ABOUT.to_string()),
            see_also: vec!["agent-workflow".to_string()],
        },
    ]
}

/// Curated elaboration for high-value lifecycle concepts. The runtime layers
/// this onto clap-derived command cards so `graft explain <command>` can carry
/// walkthrough-level guidance without duplicating the command spelling.
pub fn curated_concept_long_about(id: &str) -> Option<&'static str> {
    match id {
        "scratch" => Some(SCRATCH_LONG_ABOUT),
        "candidate" => Some(CANDIDATE_LONG_ABOUT),
        "validate" => Some(VALIDATE_LONG_ABOUT),
        "admit" => Some(ADMIT_LONG_ABOUT),
        "materialize" => Some(MATERIALIZE_LONG_ABOUT),
        "promote" => Some(PROMOTE_LONG_ABOUT),
        _ => None,
    }
}

/// Resolve `id` against the supplied concept catalog and graft-explain's
/// own diagnostic and builtin-evaluator registries.
pub fn lookup(id: &str, concepts: &[ConceptDoc]) -> ExplainResult {
    // 1. Exact concept match.
    if let Some(c) = concepts.iter().find(|c| c.id == id) {
        return ExplainResult::Concept(c.clone());
    }

    // 2. Diagnostic code (e.g. V003, A007).
    if let Some(code) = DiagCode::parse(id)
        && let Some(doc) = doc_for(code)
    {
        return ExplainResult::Diagnostic(doc.into());
    }
    // If the id parses as a code but has no catalog entry, still treat it as
    // unknown so the user gets did-you-mean for adjacent codes.

    // 3. Builtin evaluator id (lowercase spelling).
    if let Some(meta) = metadata_for_evaluator(id) {
        return ExplainResult::BuiltinEvaluator(meta.into());
    }

    // 4. Unknown — collect did-you-mean candidates from all three namespaces.
    let mut suggestions: Vec<(usize, String)> = Vec::new();
    for c in concepts {
        let dist = levenshtein(id, &c.id);
        if dist <= 3 {
            suggestions.push((dist, c.id.clone()));
        }
    }
    for d in ALL_DIAGNOSTICS {
        let code = d.code.to_string();
        let dist = levenshtein(id, &code);
        if dist <= 3 {
            suggestions.push((dist, code));
        }
    }
    for b in ALL_BUILTINS {
        let dist = levenshtein(id, b.id);
        if dist <= 3 {
            suggestions.push((dist, b.id.to_string()));
        }
    }
    suggestions.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    suggestions.dedup_by(|a, b| a.1 == b.1);
    suggestions.truncate(3);

    ExplainResult::Unknown {
        id: id.to_string(),
        did_you_mean: suggestions.into_iter().map(|(_, s)| s).collect(),
    }
}

/// Render an [`ExplainResult`] as a single human-readable string. The output
/// is plain text suitable for terminal display, with no Markdown.
pub fn render_human(result: &ExplainResult) -> String {
    let mut out = String::new();
    match result {
        ExplainResult::Concept(c) => {
            out.push_str(&format!("concept: {}\n", c.id));
            out.push_str(&format!("  {}\n", c.summary));
            if let Some(long) = &c.long_about
                && long != &c.summary
            {
                push_indented(&mut out, long);
            }
            if !c.see_also.is_empty() {
                out.push_str(&format!("  see also: {}\n", c.see_also.join(", ")));
            }
        }
        ExplainResult::Diagnostic(d) => {
            out.push_str(&format!("diagnostic: {}\n", d.code));
            out.push_str(&format!("  {}\n", d.summary));
            for hint in d.fix_hints {
                out.push_str(&format!("  fix: {hint}\n"));
            }
            if !d.see_also.is_empty() {
                out.push_str(&format!("  see also: {}\n", d.see_also.join(", ")));
            }
        }
        ExplainResult::BuiltinEvaluator(b) => {
            out.push_str(&format!("builtin evaluator: {}\n", b.id));
            out.push_str(&format!("  {}\n", b.summary));
            out.push_str(&format!("  input: {}\n", b.input));
            out.push_str(&format!("  predicate: {}\n", b.predicate));
            if b.requires_base {
                out.push_str("  requires base: yes\n");
            }
            for mode in b.failure_modes {
                out.push_str(&format!("  failure mode: {mode}\n"));
            }
        }
        ExplainResult::Unknown { id, did_you_mean } => {
            out.push_str(&format!("unknown explain id: {id}\n"));
            if !did_you_mean.is_empty() {
                out.push_str(&format!("did you mean: {}\n", did_you_mean.join(", ")));
            } else {
                out.push_str("no near matches\n");
            }
        }
    }
    out
}

fn push_indented(out: &mut String, text: &str) {
    for line in text.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
}

// Tiny in-tree Levenshtein. Avoids pulling a new dependency for a 6-line
// helper that only fires on misses. Returns edit distance between two strs.
fn levenshtein(a: &str, b: &str) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let (m, n) = (av.len(), bv.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if av[i - 1].eq_ignore_ascii_case(&bv[j - 1]) {
                0
            } else {
                1
            };
            curr[j] = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Helper for callers that have a runtime [`Diagnostic`] in hand and want a
/// matching catalog entry for richer rendering.
pub fn doc_from_runtime(diag: &Diagnostic) -> Option<&'static DiagnosticDoc> {
    doc_for(diag.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concepts() -> Vec<ConceptDoc> {
        vec![
            ConceptDoc {
                id: "admit".to_string(),
                summary: "Admit a candidate into the registry once required evidence is present"
                    .to_string(),
                long_about: None,
                see_also: vec!["validate".to_string(), "search".to_string()],
            },
            ConceptDoc {
                id: "materialize".to_string(),
                summary:
                    "Materialize an admitted patch as a Git object without updating any branch"
                        .to_string(),
                long_about: None,
                see_also: vec!["promote".to_string(), "admit".to_string()],
            },
        ]
    }

    #[test]
    fn concept_id_is_returned_verbatim() {
        let r = lookup("admit", &concepts());
        match r {
            ExplainResult::Concept(c) => assert_eq!(c.id, "admit"),
            other => panic!("expected concept, got {other:?}"),
        }
    }

    #[test]
    fn diagnostic_code_resolves_to_catalog_entry() {
        let r = lookup("V003", &concepts());
        match r {
            ExplainResult::Diagnostic(d) => {
                assert_eq!(d.code, "V003");
                assert!(d.summary.contains("base"));
                assert!(d.see_also.contains(&"valid-patch"));
            }
            other => panic!("expected diagnostic, got {other:?}"),
        }
    }

    #[test]
    fn builtin_evaluator_id_resolves_to_metadata() {
        let r = lookup("changed_paths_any_match", &concepts());
        match r {
            ExplainResult::BuiltinEvaluator(b) => {
                assert_eq!(b.id, "changed_paths_any_match");
                assert!(!b.requires_base);
            }
            other => panic!("expected builtin evaluator, got {other:?}"),
        }
    }

    #[test]
    fn typo_yields_did_you_mean_with_close_concept() {
        // "admt" — missing one char from "admit".
        let r = lookup("admt", &concepts());
        match r {
            ExplainResult::Unknown { did_you_mean, .. } => {
                assert!(
                    did_you_mean.iter().any(|s| s == "admit"),
                    "did_you_mean: {did_you_mean:?}"
                );
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn typo_in_diagnostic_code_suggests_real_one() {
        // V099 does not exist in the catalog; expect Unknown with V003-ish suggestions.
        let r = lookup("V099", &concepts());
        match r {
            ExplainResult::Unknown { did_you_mean, .. } => {
                assert!(
                    !did_you_mean.is_empty(),
                    "did_you_mean should suggest at least one near code"
                );
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_id_with_no_close_match_still_returns_structured() {
        let r = lookup("zzzzzzzzzzzzzzzz", &concepts());
        match r {
            ExplainResult::Unknown { did_you_mean, id } => {
                assert_eq!(id, "zzzzzzzzzzzzzzzz");
                assert!(did_you_mean.len() <= 3);
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn human_render_contains_three_sections_for_diagnostic() {
        let r = lookup("V003", &concepts());
        let txt = render_human(&r);
        assert!(txt.starts_with("diagnostic: V003"));
        assert!(txt.contains("fix:"));
        assert!(txt.contains("see also:"));
    }

    #[test]
    fn human_render_for_unknown_lists_suggestions() {
        let r = lookup("V099", &concepts());
        let txt = render_human(&r);
        assert!(txt.contains("unknown explain id"), "render: {txt:?}");
        assert!(
            txt.contains("did you mean") || txt.contains("no near matches"),
            "render: {txt:?}"
        );
    }

    #[test]
    fn agent_workflow_topic_is_available_from_curated_catalog() {
        let r = lookup("agent-workflow", &agent_help_concepts());
        match r {
            ExplainResult::Concept(c) => {
                assert_eq!(c.id, "agent-workflow");
                let long = c.long_about.expect("agent workflow long help");
                assert!(long.contains("graft patch from-scratch"));
                assert!(long.contains("admit generates a public patch"));
                assert!(long.contains("repo add/sync/lock/update"));
                assert!(long.contains("bundle import"));
                assert!(long.contains("workspace gc --apply"));
                assert!(long.contains("read/inspect commands on the local CLI path"));
                assert!(long.contains("graft_cli_exec"));
            }
            other => panic!("expected agent workflow concept, got {other:?}"),
        }
    }

    #[test]
    fn agent_workflow_help_avoids_retired_main_paths() {
        let txt = render_human(&lookup("agent-workflow", &agent_help_concepts()));
        for retired in [
            "graft create",
            "graft candidate from-scratch",
            "graft validate candidate:",
            "graft admit candidate:",
            "graft materialize patch:",
            "graft promote patch:",
            "scratch open",
            "scratch promote",
            "registry import",
            "graft gc --apply",
            "admit --capture",
        ] {
            assert!(!txt.contains(retired), "retired path leaked: {retired}");
        }
    }

    #[test]
    fn human_render_indents_multiline_concept_help() {
        let txt = render_human(&ExplainResult::Concept(ConceptDoc {
            id: "demo".to_string(),
            summary: "demo".to_string(),
            long_about: Some("first line\nsecond line".to_string()),
            see_also: Vec::new(),
        }));
        assert!(
            txt.contains("  first line\n  second line\n"),
            "render: {txt:?}"
        );
    }
}
