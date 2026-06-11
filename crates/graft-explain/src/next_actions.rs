//! Lifecycle-aware "next step" inference for graft candidates and patches.
//!
//! `next_actions` is a pure function: it reads only the assembled context
//! struct passed in, never the filesystem or the registry directly. CLI
//! handlers in `graft-cli` build the [`CandidateContext`] / [`PatchContext`]
//! from store reads, then call into this module for rendering.
//!
//! The output is a `Vec<NextAction>` ordered most-likely-first, with each
//! action labeled by [`NextActionKind`]. The Hole Report renderer in
//! `graft-cli` drops these straight into a block; the `--json` envelope
//! preserves the structured form.

use crate::{NextAction, NextActionKind};

/// Snapshot of a candidate's relevant lifecycle state, assembled by the
/// caller from the store. The fields are intentionally minimal — every
/// addition is a new branch in `next_actions`, so keep this lean.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateContext {
    pub id: String,
    /// Number of evidence records with `EvidenceResult::Passed`.
    pub passed: usize,
    /// Number of evidence records with `EvidenceResult::Failed`.
    pub failed: usize,
    /// Number of evidence records with `EvidenceResult::Unknown`.
    pub unknown: usize,
    /// Number of evidence records with `EvidenceResult::Skipped`.
    pub skipped: usize,
    /// Property primitives present in the candidate constraint. Surfaced in the
    /// recommended `admit --require <name>` invocation when the only
    /// passing primitive is unambiguous.
    pub constraint_primitives: Vec<String>,
}

impl CandidateContext {
    fn total_evidence(&self) -> usize {
        self.passed + self.failed + self.unknown + self.skipped
    }
}

/// Snapshot of a patch's relevant lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchContext {
    pub id: String,
    /// Property primitives present in the admitted patch constraint.
    pub constraint_primitives: Vec<String>,
    /// True when at least one materialization relation exists (the patch has
    /// produced a real Git object).
    pub materialized: bool,
    /// True when at least one promotion record exists for the patch.
    pub promoted: bool,
}

/// Infer next-step suggestions for a candidate.
pub fn next_actions(ctx: &CandidateContext) -> Vec<NextAction> {
    let mut out = Vec::new();

    if ctx.total_evidence() == 0 {
        // Stage 1: drafted, no evidence yet. With no constraint primitive,
        // application core integrity is the only default gate, so admission is available.
        if ctx.constraint_primitives.is_empty() {
            out.push(NextAction::new(
                "admit",
                format!("graft patch admit {}", ctx.id),
                NextActionKind::Recommended,
                "no property evidence is required; Graft checks application core integrity at admission",
            ));
        } else {
            out.push(NextAction::new(
                "validate",
                format!("graft patch validate {}", ctx.id),
                NextActionKind::Recommended,
                "candidate has no evidence yet; produce some by running validators",
            ));
        }
        out.push(NextAction::new(
            "show.change",
            format!("graft patch show {} --change", ctx.id),
            NextActionKind::Optional,
            "review the captured file changes before validating",
        ));
        return out;
    }

    if ctx.failed > 0 {
        // Stage 2: at least one failed evidence — repair path.
        out.push(NextAction::new(
            "amend-and-revalidate",
            format!("graft patch validate {}", ctx.id),
            NextActionKind::Recommended,
            "amend the worktree to address failed evidence, then revalidate",
        ));
        out.push(NextAction::new(
            "show.evidence",
            format!("graft patch show {} --evidence", ctx.id),
            NextActionKind::Optional,
            "inspect every evidence record to see which property failed and why",
        ));
        return out;
    }

    if ctx.unknown > 0 && ctx.passed == 0 {
        // Stage 3: only unknowns.
        out.push(NextAction::new(
            "resolve-unknown",
            format!("graft patch validate {}", ctx.id),
            NextActionKind::Recommended,
            "evidence is unknown; resolve the cause (see V003) and revalidate",
        ));
        out.push(NextAction::new(
            "show.evidence",
            format!("graft patch show {} --evidence", ctx.id),
            NextActionKind::Optional,
            "inspect each unknown reason; the `[Vnnn]` prefix tells you which",
        ));
        return out;
    }

    // Stage 4: at least one passed evidence; admission is on the table.
    let admit = if let Some(name) = single_decisive_property(ctx) {
        format!("graft patch admit {} --require {name}", ctx.id)
    } else {
        format!("graft patch admit {}", ctx.id)
    };
    out.push(NextAction::new(
        "admit",
        admit,
        NextActionKind::Recommended,
        "candidate has passing evidence; admit it into the registry",
    ));
    if ctx.unknown > 0 {
        out.push(NextAction::new(
            "validate-additional",
            format!("graft patch validate {} --expect <Property>", ctx.id),
            NextActionKind::Optional,
            "some properties are still unknown; add `--expect` to revalidate them",
        ));
    } else {
        out.push(NextAction::new(
            "validate-additional",
            format!("graft patch validate {} --expect <Property>", ctx.id),
            NextActionKind::Optional,
            "tighten the candidate by validating an additional property",
        ));
    }
    out
}

/// Infer next-step suggestions for an admitted patch.
pub fn next_actions_patch(ctx: &PatchContext) -> Vec<NextAction> {
    let mut out = Vec::new();

    if !ctx.materialized {
        // Stage 5: admitted but not yet materialized.
        out.push(NextAction::new(
            "materialize.dry-run",
            format!("graft patch materialize {} --dry-run", ctx.id),
            NextActionKind::Recommended,
            "preview the isolated state inspection output",
        ));
        out.push(NextAction::new(
            "materialize.inspect",
            format!("graft patch materialize {}", ctx.id),
            NextActionKind::Optional,
            "write the isolated state inspection output under .worktrees/",
        ));
        if let Some(prop) = ctx.constraint_primitives.first() {
            out.push(NextAction::new(
                "search.same-property",
                format!("graft patch search --property {prop}"),
                NextActionKind::Optional,
                "find sibling patches admitted with the same property",
            ));
        }
        return out;
    }

    if !ctx.promoted {
        // Stage 6: materialized but not promoted.
        out.push(NextAction::new(
            "promote.dry-run",
            format!("graft patch promote {} --to <branch>", ctx.id),
            NextActionKind::Recommended,
            "draft a promotion plan; promote is the only command that mutates Git refs",
        ));
        out.push(NextAction::new(
            "promote.apply",
            format!("graft patch promote {} --to <branch> --yes", ctx.id),
            NextActionKind::Dangerous,
            "apply the promotion now; this updates a real Git ref",
        ));
        return out;
    }

    // Stage 7: promoted — terminal on the happy path.
    out.push(NextAction::new(
        "search.related",
        format!(
            "graft patch search --property {}",
            ctx.constraint_primitives
                .first()
                .map(String::as_str)
                .unwrap_or("<P>"),
        ),
        NextActionKind::Optional,
        "find related patches admitted with the same property",
    ));
    out.push(NextAction::new(
        "lifecycle.complete",
        format!("graft patch show {}", ctx.id),
        NextActionKind::Terminal,
        "patch has been admitted, materialized, and promoted; lifecycle complete",
    ));
    out
}

/// When a candidate's evidence shows exactly one decisively-passing property
/// primitive (and no failures), surface it as the `--require` argument in the
/// recommended `admit` command. This lets users run a tighter admit without
/// having to type the property name from memory.
fn single_decisive_property(ctx: &CandidateContext) -> Option<&str> {
    if ctx.failed == 0 && ctx.passed == 1 && ctx.constraint_primitives.len() == 1 {
        ctx.constraint_primitives.first().map(|s| s.as_str())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_candidate() -> CandidateContext {
        CandidateContext {
            id: "candidate:demo".into(),
            passed: 0,
            failed: 0,
            unknown: 0,
            skipped: 0,
            constraint_primitives: vec![],
        }
    }

    #[test]
    fn drafted_candidate_without_constraint_primitives_recommends_admit() {
        let ctx = empty_candidate();
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].id, "admit");
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(
            actions[0]
                .label
                .contains("graft patch admit candidate:demo")
        );
        // Must include an Optional for --change review.
        assert!(actions.iter().any(|a| a.kind == NextActionKind::Optional));
    }

    #[test]
    fn drafted_candidate_with_constraint_primitives_recommends_validate() {
        let ctx = CandidateContext {
            constraint_primitives: vec!["ReviewPolicy".into()],
            ..empty_candidate()
        };
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].id, "validate");
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(
            actions[0]
                .label
                .contains("graft patch validate candidate:demo")
        );
    }

    #[test]
    fn failed_evidence_routes_to_amend() {
        let ctx = CandidateContext {
            failed: 1,
            ..empty_candidate()
        };
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(actions[0].why.contains("amend"));
        // No admit suggestion at all in this stage.
        assert!(
            actions.iter().all(|a| !a.id.starts_with("admit")),
            "no admit should be offered when there is a failed evidence"
        );
    }

    #[test]
    fn unknown_only_routes_through_validate_and_links_v003() {
        let ctx = CandidateContext {
            unknown: 1,
            ..empty_candidate()
        };
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(
            actions[0].why.contains("V003"),
            "unknown stage why must point at V003: {:?}",
            actions[0].why
        );
    }

    #[test]
    fn passed_evidence_recommends_admit_with_optional_validate_more() {
        let ctx = CandidateContext {
            passed: 1,
            constraint_primitives: vec!["ReviewPolicy".into()],
            ..empty_candidate()
        };
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].id, "admit");
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(actions[0].label.contains("--require ReviewPolicy"));
        assert!(actions.iter().any(|a| a.id == "validate-additional"));
    }

    #[test]
    fn admit_recommendation_drops_require_when_property_is_ambiguous() {
        let ctx = CandidateContext {
            passed: 2,
            constraint_primitives: vec!["ReviewPolicy".into(), "TestsPass".into()],
            ..empty_candidate()
        };
        let actions = next_actions(&ctx);
        assert_eq!(actions[0].id, "admit");
        assert!(!actions[0].label.contains("--require"));
    }

    #[test]
    fn admitted_patch_recommends_dry_run_materialize() {
        let ctx = PatchContext {
            id: "patch:demo".into(),
            constraint_primitives: vec!["ReviewPolicy".into()],
            materialized: false,
            promoted: false,
        };
        let actions = next_actions_patch(&ctx);
        assert_eq!(actions[0].id, "materialize.dry-run");
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(
            actions.iter().any(|a| a.id == "search.same-property"),
            "should offer sibling search when at least one property exists"
        );
    }

    #[test]
    fn materialized_patch_recommends_promote_dry_run_with_dangerous_apply() {
        let ctx = PatchContext {
            id: "patch:demo".into(),
            constraint_primitives: vec!["ReviewPolicy".into()],
            materialized: true,
            promoted: false,
        };
        let actions = next_actions_patch(&ctx);
        assert_eq!(actions[0].id, "promote.dry-run");
        assert_eq!(actions[0].kind, NextActionKind::Recommended);
        assert!(
            actions.iter().any(|a| a.kind == NextActionKind::Dangerous),
            "an explicit dangerous --yes variant must be offered"
        );
    }

    #[test]
    fn promoted_patch_is_terminal() {
        let ctx = PatchContext {
            id: "patch:demo".into(),
            constraint_primitives: vec!["ReviewPolicy".into()],
            materialized: true,
            promoted: true,
        };
        let actions = next_actions_patch(&ctx);
        assert!(
            actions.iter().any(|a| a.kind == NextActionKind::Terminal),
            "promoted lifecycle must contain a terminal marker"
        );
    }

    #[test]
    fn no_panic_for_default_inputs() {
        // This is a regression guard for the "post-init empty candidate /
        // freshly-promoted patch" edge cases called out in the task plan.
        let _ = next_actions(&empty_candidate());
        let _ = next_actions_patch(&PatchContext {
            id: "patch:demo".into(),
            constraint_primitives: vec![],
            materialized: false,
            promoted: false,
        });
    }
}
