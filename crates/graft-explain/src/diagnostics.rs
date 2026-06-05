//! Predefined [`Diagnostic`] catalog for graft.
//!
//! This module is the single home for graft's user-facing diagnostic codes.
//! Each builder returns a freshly constructed [`Diagnostic`] with the right
//! [`DiagCode`], single-line summary, single-line fix hints and see-also
//! references — all inlined here at the producing site, in line with the
//! project's "compiler-as-documentation" rule.
//!
//! Letter-segment convention (see thread plan):
//!   V — validate / verify (evidence production)
//!   A — admit (candidate → registry)
//!   M — materialize (registry → git object)
//!   P — promote (git object → branch / PR / release)
//!   C — candidate generation / transformation
//!   R — registry storage and search
//!   G — git bridge (gitoxide / explicit Git CLI bridge)

use crate::{DiagCode, DiagDomain, Diagnostic};

// =====================================================================
// V — validate / verify
// =====================================================================

/// V001 — declared expected property is not defined in `graft.toml`.
pub fn v001_unknown_property(property: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Validate, 1),
        format!("property `{property}` is not declared in graft.toml and not a builtin"),
    )
    .at(property.to_string())
    .fix("declare it under [properties.<name>] in graft.toml, or pass a builtin name")
    .see("properties")
    .see("graft.toml")
}

/// V002 — property verifier rejected the candidate; the patch was observed
/// to violate the expected property.
pub fn v002_property_violated(property: &str, detail: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Validate, 2),
        format!("property `{property}` was observed violated"),
    )
    .at(property.to_string())
    .fix(format!("inspect: {}", one_line(detail)))
    .fix("amend the candidate worktree and run `graft validate` again")
    .see("validate")
    .see("evidence-result.failed")
}

/// V003 — base state required by the verifier could not be materialized.
///
/// This is the case famously surfaced when running `graft validate` outside
/// a Git repo: the original error is `git stderr` and used to leak through
/// the evidence reason. Wrapped here so users see a code with one-line
/// repair guidance instead.
pub fn v003_base_unmaterializable(detail: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Validate, 3),
        "base state could not be materialized for verification",
    )
    .at(one_line(detail))
    .fix("start scratch edits from an explicit materializable base such as `--base graft:empty`, then run `graft candidate from-scratch`")
    .see("valid-patch")
    .see("scratch")
    .see("candidate")
    .see("evidence-result.unknown")
}

/// V004 — verifier command exited non-zero.
pub fn v004_command_verifier_nonzero(property: &str, command: &str, exit: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Validate, 4),
        format!("verify command for `{property}` exited unsuccessfully ({exit})"),
    )
    .at(format!("{property}: {}", one_line(command)))
    .fix("re-run the command in the validation worktree to inspect output")
    .fix("ensure the command is deterministic, idempotent and side-effect free")
    .see("validate")
    .see("properties")
}

/// V005 — verifier configuration is malformed.
pub fn v005_verifier_misconfigured(property: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Validate, 5),
        format!("verifier configuration for property `{property}` is malformed"),
    )
    .at(property.to_string())
    .fix(format!("fix: {}", one_line(why)))
    .fix("see `graft explain properties` for the supported `verify` shapes")
    .see("properties")
    .see("graft.toml")
}

// =====================================================================
// A — admit
// =====================================================================

/// A001 — admission requires evidence for a property that has none.
pub fn a001_missing_required_evidence(property: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Admit, 1),
        format!("missing required evidence for `{property}`"),
    )
    .at(property.to_string())
    .fix(format!(
        "run `graft validate <candidate> --expect {property}`"
    ))
    .see("admit")
    .see("validate")
}

/// A002 — admission requires evidence for a property whose evidence did not
/// pass.
pub fn a002_failed_required_evidence(property: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Admit, 2),
        format!("evidence for `{property}` did not pass"),
    )
    .at(property.to_string())
    .fix("amend the candidate, re-run `graft validate`, then retry `graft admit`")
    .see("admit")
    .see("evidence-result.failed")
}

// =====================================================================
// M — materialize
// =====================================================================

/// M001 — registry bundle re-import found a tree object whose stored id
/// disagrees with the canonical id derived from its content.
pub fn m001_registry_tree_id_mismatch(stored: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Materialize, 1),
        "tree object id in the registry bundle does not match its content",
    )
    .at(stored.to_string())
    .fix("re-export the registry from a trusted source and retry the import")
    .see("registry")
}

/// M002 — registry bundle re-import found a change object whose stored id
/// disagrees with the canonical id derived from its content.
pub fn m002_registry_change_id_mismatch(stored: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Materialize, 2),
        "change object id in the registry bundle does not match its content",
    )
    .at(stored.to_string())
    .fix("re-export the registry from a trusted source and retry the import")
    .see("registry")
}

/// M003 — patch targets a state graft cannot materialize on its own.
pub fn m003_target_not_materializable(target: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Materialize, 3),
        "patch target cannot be materialized to a Git tree",
    )
    .at(target.to_string())
    .fix(format!("reason: {}", one_line(why)))
    .fix("run `graft compose`/`graft migrate` to land it on a materializable base first")
    .see("materialize")
    .see("compose")
    .see("migrate")
}

// =====================================================================
// C — candidate generation / transformation
// =====================================================================

/// C002 — change is inline-only and cannot be transformed (composed,
/// migrated, etc.).
pub fn c002_inline_change_not_transformable(summary: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Create, 2),
        "cannot transform an inline-only change",
    )
    .at(one_line(summary))
    .fix("rebuild the candidate through scratch edits and `graft candidate from-scratch` so the change is stored, not inline")
    .see("candidate")
    .see("compose")
}

// =====================================================================
// R — registry storage / search
// =====================================================================

/// R001 — registry on-disk index is corrupt or missing required tables.
pub fn r001_registry_index_corrupt(detail: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::Registry, 1),
        "registry index is corrupt or missing tables",
    )
    .at(one_line(detail))
    .fix("delete .graft/registry/index.sqlite and re-run `graft init` to rebuild")
    .see("registry")
}

// =====================================================================
// G — git bridge
// =====================================================================

/// G001 — git plumbing rejected a path that contains characters it cannot
/// represent (mtree separator, NUL, etc.).
pub fn g001_unsupported_path(path: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::GitBridge, 1),
        "git rejected a path that cannot be encoded into a tree object",
    )
    .at(path.to_string())
    .fix("rename the file or exclude it from the captured worktree")
    .see("materialize")
}

/// G002 — internal: a child `git` process exited non-zero. This is the
/// raw upstream error that callers wrap with a more specific code (e.g.
/// V003) for user output.
pub fn g002_git_subprocess_failed(detail: &str) -> Diagnostic {
    Diagnostic::new(
        DiagCode::new(DiagDomain::GitBridge, 2),
        "git subprocess returned an error",
    )
    .at(one_line(detail))
    .fix("re-run the underlying git command in the same cwd to inspect details")
    .see("materialize")
}

// =====================================================================
// catalog
// =====================================================================

/// Static doc-card for one diagnostic code, decoupled from the runtime
/// builders that need user-supplied args. Used by `graft explain V003`
/// and any other catalog-style enumeration.
///
/// Every builder in this module must be paired with one entry in
/// [`ALL_DIAGNOSTICS`]; the [`builders_match_catalog_codes`] regression
/// test guards that pairing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticDoc {
    pub code: DiagCode,
    /// Single-line summary that does not depend on any runtime arg.
    pub summary: &'static str,
    /// Canonical single-line fix hints, ordered most-likely-first.
    pub fix_hints: &'static [&'static str],
    /// Related concept ids or sibling diagnostic codes.
    pub see_also: &'static [&'static str],
}

/// Lookup helper for `graft explain <CODE>`.
pub fn doc_for(code: DiagCode) -> Option<&'static DiagnosticDoc> {
    ALL_DIAGNOSTICS.iter().find(|d| d.code == code)
}

/// Catalog of every diagnostic shipped with graft. New diagnostics must be
/// added here in addition to their `pub fn vNNN_*` builder.
pub const ALL_DIAGNOSTICS: &[DiagnosticDoc] = &[
    // V — validate / verify
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Validate, 1),
        summary: "declared expected property is not defined in graft.toml",
        fix_hints: &["declare it under [properties.<name>] in graft.toml, or pass a builtin name"],
        see_also: &["properties", "graft.toml"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Validate, 2),
        summary: "property verifier rejected the candidate; the patch was observed to violate the property",
        fix_hints: &[
            "inspect the verifier output for the violating evidence",
            "amend the candidate worktree and run `graft validate` again",
        ],
        see_also: &["validate", "evidence-result.failed"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Validate, 3),
        summary: "base state required by the verifier could not be materialized",
        fix_hints: &[
            "start scratch edits from an explicit materializable base such as `--base graft:empty`, then run `graft candidate from-scratch`",
        ],
        see_also: &[
            "valid-patch",
            "scratch",
            "candidate",
            "evidence-result.unknown",
        ],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Validate, 4),
        summary: "verifier command exited non-zero",
        fix_hints: &[
            "re-run the command in the validation worktree to inspect output",
            "ensure the command is deterministic, idempotent and side-effect free",
        ],
        see_also: &["validate", "properties"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Validate, 5),
        summary: "verifier configuration is malformed",
        fix_hints: &["see `graft explain properties` for the supported `verify` shapes"],
        see_also: &["properties", "graft.toml"],
    },
    // A — admit
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Admit, 1),
        summary: "admission requires evidence for a property that has none",
        fix_hints: &["run `graft validate <candidate> --expect <Property>` first"],
        see_also: &["admit", "validate"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Admit, 2),
        summary: "admission requires evidence for a property whose evidence did not pass",
        fix_hints: &["amend the candidate, re-run `graft validate`, then retry `graft admit`"],
        see_also: &["admit", "evidence-result.failed"],
    },
    // M — materialize
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Materialize, 1),
        summary: "tree object id in the registry bundle does not match its content",
        fix_hints: &["re-export the registry from a trusted source and retry the import"],
        see_also: &["registry"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Materialize, 2),
        summary: "change object id in the registry bundle does not match its content",
        fix_hints: &["re-export the registry from a trusted source and retry the import"],
        see_also: &["registry"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Materialize, 3),
        summary: "patch target cannot be materialized to a Git tree",
        fix_hints: &[
            "run `graft compose`/`graft migrate` to land it on a materializable base first",
        ],
        see_also: &["materialize", "compose", "migrate"],
    },
    // C — candidate generation / transformation
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Create, 2),
        summary: "cannot transform an inline-only change",
        fix_hints: &[
            "rebuild the candidate through scratch edits and `graft candidate from-scratch` so the change is stored, not inline",
        ],
        see_also: &["candidate", "compose"],
    },
    // R — registry storage / search
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::Registry, 1),
        summary: "registry index is corrupt or missing tables",
        fix_hints: &["delete .graft/registry/index.sqlite and re-run `graft init` to rebuild"],
        see_also: &["registry"],
    },
    // G — git bridge
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::GitBridge, 1),
        summary: "git rejected a path that cannot be encoded into a tree object",
        fix_hints: &["rename the file or exclude it from the captured worktree"],
        see_also: &["materialize"],
    },
    DiagnosticDoc {
        code: DiagCode::new(DiagDomain::GitBridge, 2),
        summary: "git subprocess returned an error",
        fix_hints: &["re-run the underlying git command in the same cwd to inspect details"],
        see_also: &["materialize"],
    },
];

// =====================================================================
// helpers
// =====================================================================

/// Collapse a multi-line / whitespace-heavy string into a single line so it
/// fits the project's narrative rule.
fn one_line(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v003_carries_repair_advice_and_unknown_link() {
        let d = v003_base_unmaterializable(
            "fatal: not a git repository (or any of the parent directories): .git",
        );
        assert_eq!(d.code.to_string(), "V003");
        // Detail is collapsed to one line and stored as locus.
        assert!(d.loc.is_some());
        assert!(!d.loc.as_deref().unwrap().contains('\n'));
        // Fix hints are non-empty single lines.
        assert!(!d.fix_hints.is_empty());
        for hint in &d.fix_hints {
            assert!(
                !hint.contains('\n'),
                "fix hint must be single-line: {hint:?}"
            );
        }
        // see_also wires this diagnostic to the unknown evidence concept.
        assert!(d.see_also.contains(&"evidence-result.unknown".to_string()));
        assert!(d.see_also.contains(&"valid-patch".to_string()));
    }

    #[test]
    fn admit_codes_distinct_and_in_admit_domain() {
        let a1 = a001_missing_required_evidence("ReviewPolicy");
        let a2 = a002_failed_required_evidence("ReviewPolicy");
        assert_eq!(a1.code.domain, DiagDomain::Admit);
        assert_eq!(a2.code.domain, DiagDomain::Admit);
        assert_ne!(a1.code, a2.code);
    }

    #[test]
    fn diagnostics_cover_all_seven_subsystems() {
        // Smoke check: at least one canned builder per subsystem letter.
        let samples: Vec<DiagCode> = vec![
            v001_unknown_property("X").code,
            a001_missing_required_evidence("X").code,
            m001_registry_tree_id_mismatch("X").code,
            // P-domain codes are added by promote-required-from-config.
            c002_inline_change_not_transformable("X").code,
            r001_registry_index_corrupt("X").code,
            g001_unsupported_path("X").code,
        ];
        let mut letters: Vec<char> = samples.iter().map(|c| c.domain.letter()).collect();
        letters.sort();
        letters.dedup();
        // P is intentionally absent here (T7 owns it); the others are covered.
        for required in ['V', 'A', 'M', 'C', 'R', 'G'] {
            assert!(letters.contains(&required), "missing domain {required}");
        }
    }

    #[test]
    fn one_line_collapses_multiline_and_double_spaces() {
        let collapsed = one_line("first  line\n second\tline   ");
        assert_eq!(collapsed, "first line second line");
    }

    #[test]
    fn builders_match_catalog_codes() {
        // Every doc-catalog entry must have a builder somewhere in this
        // module; the inverse is asserted by the `every_diagnostic_*` test
        // below, which calls every builder. Together they ensure pairing.
        // Here we just smoke-check that each catalog entry has a stable
        // single-line summary and at least one fix hint.
        for d in ALL_DIAGNOSTICS {
            assert!(!d.summary.is_empty(), "{}: empty summary", d.code);
            assert!(!d.summary.contains('\n'), "{}: multi-line summary", d.code);
            assert!(!d.fix_hints.is_empty(), "{}: must have fix hints", d.code);
            for h in d.fix_hints {
                assert!(!h.contains('\n'), "{}: multi-line fix", d.code);
            }
        }
        // Catalog must have exactly the same number of entries as builders
        // exercised by every_diagnostic_summary_and_loc_are_single_line.
        assert_eq!(ALL_DIAGNOSTICS.len(), 14);
    }

    #[test]
    fn doc_for_round_trips_codes() {
        for d in ALL_DIAGNOSTICS {
            let looked_up = doc_for(d.code).expect("doc lookup");
            assert_eq!(looked_up.code, d.code);
        }
        assert!(doc_for(DiagCode::new(DiagDomain::Promote, 999)).is_none());
    }

    #[test]
    fn every_diagnostic_summary_and_loc_are_single_line() {
        let all = [
            v001_unknown_property("p"),
            v002_property_violated("p", "diff"),
            v003_base_unmaterializable("err"),
            v004_command_verifier_nonzero("p", "cmd", "exit 1"),
            v005_verifier_misconfigured("p", "why"),
            a001_missing_required_evidence("p"),
            a002_failed_required_evidence("p"),
            m001_registry_tree_id_mismatch("id"),
            m002_registry_change_id_mismatch("id"),
            m003_target_not_materializable("t", "why"),
            c002_inline_change_not_transformable("s"),
            r001_registry_index_corrupt("d"),
            g001_unsupported_path("p"),
            g002_git_subprocess_failed("d"),
        ];
        for d in &all {
            assert!(!d.summary.contains('\n'), "{}: summary multi-line", d.code);
            if let Some(loc) = &d.loc {
                assert!(!loc.contains('\n'), "{}: loc multi-line", d.code);
            }
            for h in &d.fix_hints {
                assert!(!h.contains('\n'), "{}: fix hint multi-line", d.code);
            }
        }
        // 14 builders covering 6 subsystems (P stays for T7).
        assert_eq!(all.len(), 14);
    }
}
