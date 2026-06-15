//! Inline metadata for builtin evaluators.
//!
//! Builtin evaluators are low-level verifier primitives, not workspace
//! constraints. A workspace constraint binds a query, one evaluator, and a judge.
//!
//! Atomicity rule: every builtin evaluator exposes one stable snake_case id and
//! answers exactly one primitive question over the query subject plus its own
//! options. It must not encode a policy alias, a conjunction of independently
//! nameable policy requirements, or any workspace-specific default.

use crate::Explainable;

/// Stable ids for the builtin path-set evaluators shipped with graft.
pub const CHANGED_PATHS_ANY_MATCH: &str = "changed_paths_any_match";
pub const CHANGED_PATHS_ALL_MATCH: &str = "changed_paths_all_match";

/// Inline, single-line metadata for one builtin evaluator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinEvaluatorMetadata {
    /// Stable evaluator id used as `[evaluator] kind = "builtin", name = ...`.
    pub id: &'static str,
    /// One-line summary suitable for `graft explain` and search warnings.
    pub summary: &'static str,
    /// Input fact this evaluator consumes. Kept explicit so new builtins cannot
    /// quietly grow into multi-input policy bundles.
    pub input: &'static str,
    /// Primitive boolean question answered by this evaluator.
    pub predicate: &'static str,
    /// True when this evaluator reads the patch base; used to hint that an
    /// explicit `--base graft:empty` scratch start applies in no-`.git` dirs.
    pub requires_base: bool,
    /// Single-line failure-mode descriptions ordered most-likely-first.
    pub failure_modes: &'static [&'static str],
}

impl Explainable for BuiltinEvaluatorMetadata {
    fn id(&self) -> &'static str {
        self.id
    }

    fn summary(&self) -> &'static str {
        self.summary
    }

    fn see_also(&self) -> &'static [&'static str] {
        match self.id {
            CHANGED_PATHS_ANY_MATCH => &["validate", "constraints", "evidence-result.passed"],
            CHANGED_PATHS_ALL_MATCH => &["validate", "constraints", "evidence-result.passed"],
            _ => &["validate", "constraints"],
        }
    }
}

/// All builtin evaluator metadata records, ordered by id.
pub const ALL_BUILTINS: &[BuiltinEvaluatorMetadata] = &[
    BuiltinEvaluatorMetadata {
        id: CHANGED_PATHS_ALL_MATCH,
        summary: "answers whether every changed path matches at least one configured glob pattern",
        input: "changed path set plus evaluator glob options",
        predicate: "for every changed path, at least one glob matches",
        requires_base: false,
        failure_modes: &["at least one changed path matches none of the patterns -> failed"],
    },
    BuiltinEvaluatorMetadata {
        id: CHANGED_PATHS_ANY_MATCH,
        summary: "answers whether at least one changed path matches a configured glob pattern",
        input: "changed path set plus evaluator glob options",
        predicate: "there exists a changed path such that at least one glob matches",
        requires_base: false,
        failure_modes: &["no changed paths match any configured pattern -> failed"],
    },
];

/// Look up a builtin evaluator's metadata by evaluator id.
pub fn metadata_for_evaluator(id: &str) -> Option<&'static BuiltinEvaluatorMetadata> {
    ALL_BUILTINS.iter().find(|m| m.id == id)
}

/// True when `name` matches the spelling of any builtin evaluator id.
pub fn is_builtin_evaluator(name: &str) -> bool {
    metadata_for_evaluator(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_builtins_with_unique_ids_and_nonempty_metadata() {
        assert_eq!(ALL_BUILTINS.len(), 2);
        let mut ids: Vec<&str> = ALL_BUILTINS.iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2);
        for m in ALL_BUILTINS {
            assert!(!m.summary.is_empty(), "{}: empty summary", m.id);
            assert!(!m.summary.contains('\n'), "{}: multi-line summary", m.id);
            assert!(!m.input.is_empty(), "{}: empty input", m.id);
            assert!(!m.predicate.is_empty(), "{}: empty predicate", m.id);
            assert!(
                !m.requires_base,
                "{}: path-set evaluator must not require base",
                m.id
            );
            for mode in m.failure_modes {
                assert!(!mode.contains('\n'), "{}: multi-line failure mode", m.id);
            }
        }
    }

    #[test]
    fn is_builtin_evaluator_recognizes_evaluator_ids_only() {
        assert!(is_builtin_evaluator(CHANGED_PATHS_ANY_MATCH));
        assert!(is_builtin_evaluator(CHANGED_PATHS_ALL_MATCH));
        assert!(!is_builtin_evaluator("EmptyChange"));
        assert!(!is_builtin_evaluator("ReviewPolicy"));
        assert!(!is_builtin_evaluator(""));
    }
}
