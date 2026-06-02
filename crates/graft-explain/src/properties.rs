//! Inline metadata for builtin verifier properties.
//!
//! Each variant of `graft_validate::BuiltinCheck` carries a stable
//! `id` (the spelling used in `[properties.<name>] kind=\"builtin\" check=...`)
//! plus a single-line summary, a `requires_base` flag that downstream tools
//! can use to render targeted hints, and a list of `failure_modes` describing
//! the situations under which the verifier produces non-`passed` evidence.
//!
//! The metadata is the single source of truth used by both `graft search`
//! (to recognize known builtin names when warning about unknown properties)
//! and `graft explain` (to render the property's documentation card).

use crate::Explainable;

/// Stable ids for the four builtin verifier checks shipped with graft.
pub const HAS_CHANGE: &str = "has_change";
pub const VALID_PATCH: &str = "valid_patch";
pub const PATHS_NONE_MATCH: &str = "paths_none_match";
pub const PATHS_ALL_MATCH: &str = "paths_all_match";

/// Inline, single-line metadata for one builtin verifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinPropertyMetadata {
    /// Stable id (the `check` value in graft.toml).
    pub id: &'static str,
    /// One-line summary suitable for `graft explain` and `graft search` warnings.
    pub summary: &'static str,
    /// True when this verifier reads the patch base; used to hint that a
    /// `--from graft:empty` workaround applies in no-`.git` directories.
    pub requires_base: bool,
    /// Single-line failure-mode descriptions ordered most-likely-first.
    pub failure_modes: &'static [&'static str],
}

impl Explainable for BuiltinPropertyMetadata {
    fn id(&self) -> &'static str {
        self.id
    }

    fn summary(&self) -> &'static str {
        self.summary
    }

    fn see_also(&self) -> &'static [&'static str] {
        // Every builtin links back to the validate concept and the four-state
        // evidence vocabulary; T6 explain expands this.
        match self.id {
            VALID_PATCH => &["validate", "create", "evidence-result.unknown", "V003"],
            HAS_CHANGE => &["validate", "evidence-result.passed"],
            PATHS_NONE_MATCH => &["validate", "properties"],
            PATHS_ALL_MATCH => &["validate", "properties"],
            _ => &["validate", "properties"],
        }
    }
}

/// All builtin verifier metadata records, ordered by id.
pub const ALL_BUILTINS: &[BuiltinPropertyMetadata] = &[
    BuiltinPropertyMetadata {
        id: HAS_CHANGE,
        summary: "patch records at least one file change against its declared base",
        requires_base: false,
        failure_modes: &["candidate captured an empty diff against the declared base"],
    },
    BuiltinPropertyMetadata {
        id: VALID_PATCH,
        summary: "patch replays cleanly from declared base to declared target",
        requires_base: true,
        failure_modes: &[
            "declared base cannot be materialized (e.g. running outside a git repo) -> unknown",
            "stored change does not apply to the declared base -> failed",
            "applying the change yields a tree different from the declared target -> failed",
        ],
    },
    BuiltinPropertyMetadata {
        id: PATHS_NONE_MATCH,
        summary: "patch touches no path matching any of the configured glob patterns",
        requires_base: false,
        failure_modes: &["patch touches a path matching at least one of the patterns -> failed"],
    },
    BuiltinPropertyMetadata {
        id: PATHS_ALL_MATCH,
        summary: "every changed path matches at least one of the configured glob patterns",
        requires_base: false,
        failure_modes: &[
            "candidate captured zero changed paths -> unknown",
            "at least one changed path matches none of the patterns -> failed",
        ],
    },
];

/// Look up a builtin's metadata by `check` id (the value used in graft.toml).
pub fn metadata_for_check(id: &str) -> Option<&'static BuiltinPropertyMetadata> {
    ALL_BUILTINS.iter().find(|m| m.id == id)
}

/// Look up a builtin by either its low-level `check` id (`valid_patch`) or the
/// default property name (`ValidPatch`) shown in generated graft.toml files.
pub fn metadata_for_check_or_property_name(id: &str) -> Option<&'static BuiltinPropertyMetadata> {
    metadata_for_check(id).or_else(|| match id {
        "HasChange" => metadata_for_check(HAS_CHANGE),
        "ValidPatch" => metadata_for_check(VALID_PATCH),
        "PathsNoneMatch" => metadata_for_check(PATHS_NONE_MATCH),
        "PathsAllMatch" => metadata_for_check(PATHS_ALL_MATCH),
        _ => None,
    })
}

/// True when `name` matches the spelling of any builtin verifier id.
pub fn is_builtin_check(name: &str) -> bool {
    metadata_for_check(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_builtins_with_unique_ids_and_nonempty_summaries() {
        assert_eq!(ALL_BUILTINS.len(), 4);
        let mut ids: Vec<&str> = ALL_BUILTINS.iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        for m in ALL_BUILTINS {
            assert!(!m.summary.is_empty(), "{}: empty summary", m.id);
            assert!(!m.summary.contains('\n'), "{}: multi-line summary", m.id);
            for mode in m.failure_modes {
                assert!(!mode.contains('\n'), "{}: multi-line failure mode", m.id);
            }
        }
    }

    #[test]
    fn valid_patch_marks_base_dependency_and_links_v003() {
        let m = metadata_for_check(VALID_PATCH).expect("ValidPatch metadata");
        assert!(m.requires_base);
        assert!(m.see_also().contains(&"V003"));
        assert!(
            m.failure_modes
                .iter()
                .any(|fm| fm.contains("base cannot be materialized")),
            "ValidPatch must declare the no-base failure mode"
        );
    }

    #[test]
    fn is_builtin_check_recognizes_real_ids_only() {
        assert!(is_builtin_check(HAS_CHANGE));
        assert!(is_builtin_check(VALID_PATCH));
        assert!(is_builtin_check(PATHS_NONE_MATCH));
        assert!(is_builtin_check(PATHS_ALL_MATCH));
        assert!(!is_builtin_check("ValidPatch"), "case-sensitive");
        assert!(!is_builtin_check("TestsPass"));
        assert!(!is_builtin_check(""));
    }

    #[test]
    fn explain_lookup_accepts_default_property_names_as_aliases() {
        assert_eq!(
            metadata_for_check_or_property_name("ValidPatch").map(|m| m.id),
            Some(VALID_PATCH)
        );
        assert_eq!(
            metadata_for_check_or_property_name("valid_patch").map(|m| m.id),
            Some(VALID_PATCH)
        );
        assert!(metadata_for_check_or_property_name("TestsPass").is_none());
    }
}
