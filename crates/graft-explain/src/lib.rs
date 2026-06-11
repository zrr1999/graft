//! User-facing UX rendering layer for graft.
//!
//! This crate is the single home for "compiler-as-documentation" output:
//! - [`Explainable`] surfaces structure-resident, single-line metadata about a
//!   concept (a builtin evaluator, a CLI subcommand, a state, a diagnostic, ...).
//! - [`NextAction`] is the unit consumed by the Hole Report block that replaces
//!   the legacy `next:` line on every successful command.
//! - [`Diagnostic`] is the three-layer error/unknown carrier (precise locus →
//!   one-line fix hint → see-also list of related ids/codes).
//!
//! Structure-resident metadata stays concise, while curated `graft explain`
//! workflow topics carry the longer plain-text guidance shared by CLI users and
//! pi-graft tools.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

pub mod diagnostics;
pub mod evidence_result;
pub mod explain;
pub mod next_actions;
pub mod properties;

/// A node in graft's user-visible vocabulary that can describe itself.
///
/// Implementors must rely only on data already carried by the structure (enum
/// variants, struct fields, clap derives). External documentation tables are
/// disallowed by the project's "compiler-as-documentation" rule.
pub trait Explainable {
    /// Stable identifier used by `graft explain <id>` and as a `see_also`
    /// target. Examples: `"admit"`, `"V003"`, `"EmptyChange"`,
    /// `"evidence-result.unknown"`.
    fn id(&self) -> &'static str;

    /// One-line summary. Must be plain text, no Markdown, no newlines.
    fn summary(&self) -> &'static str;

    /// Related ids surfaced as `see also:` in rendered output. Default empty.
    fn see_also(&self) -> &'static [&'static str] {
        &[]
    }

    /// Optional concise elaboration. Default empty; richer workflow walkthroughs
    /// are modeled as curated `graft explain` concept cards.
    fn narrative(&self) -> Option<&'static str> {
        None
    }
}

/// What kind of next step a [`NextAction`] is, used to render Hole Report
/// labels and guide non-interactive command consumers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextActionKind {
    /// The single most likely next step in the lifecycle.
    Recommended,
    /// A useful alternative or refinement; may appear zero or many times.
    Optional,
    /// A terminal step (no further actions follow it on the happy path).
    Terminal,
    /// A step that mutates real Git state, removes data, or otherwise warrants
    /// confirmation; rendered with a warning marker.
    Dangerous,
}

impl NextActionKind {
    /// Short bracketed label used by the human-text Hole Report renderer.
    pub fn label(self) -> &'static str {
        match self {
            Self::Recommended => "[recommended]",
            Self::Optional => "[optional]",
            Self::Terminal => "[terminal]",
            Self::Dangerous => "[dangerous]",
        }
    }
}

/// One row in a Hole Report block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextAction {
    /// Stable id for machine consumers (`--json` uses this verbatim).
    pub id: String,
    /// Concrete invocation hint shown to the user, e.g.
    /// `"graft patch validate candidate:..."`.
    pub label: String,
    pub kind: NextActionKind,
    /// One-line rationale: why this is the right next step right now.
    pub why: String,
}

impl NextAction {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        kind: NextActionKind,
        why: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            why: why.into(),
        }
    }
}

/// Subsystem letter segment for a diagnostic code.
///
/// Code naming follows lifecycle locality: a user reading `V003` knows the
/// diagnostic was raised in the validate/verify path without consulting a
/// table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiagDomain {
    /// `V` — validate / verify (evidence production).
    Validate,
    /// `A` — admit (candidate → registry).
    Admit,
    /// `M` — materialize (registry → git object).
    Materialize,
    /// `P` — promote (git object → branch / PR / release).
    Promote,
    /// `C` — candidate generation / transformation.
    Create,
    /// `R` — registry storage and search.
    Registry,
    /// `G` — git bridge (gitoxide / explicit Git CLI bridge).
    GitBridge,
}

impl DiagDomain {
    /// Single-letter prefix used in rendered codes (e.g. `'V'` for `V003`).
    pub fn letter(self) -> char {
        match self {
            Self::Validate => 'V',
            Self::Admit => 'A',
            Self::Materialize => 'M',
            Self::Promote => 'P',
            Self::Create => 'C',
            Self::Registry => 'R',
            Self::GitBridge => 'G',
        }
    }

    /// Inverse of [`Self::letter`]. Used by `graft explain V003` lookup.
    pub fn from_letter(c: char) -> Option<Self> {
        match c.to_ascii_uppercase() {
            'V' => Some(Self::Validate),
            'A' => Some(Self::Admit),
            'M' => Some(Self::Materialize),
            'P' => Some(Self::Promote),
            'C' => Some(Self::Create),
            'R' => Some(Self::Registry),
            'G' => Some(Self::GitBridge),
            _ => None,
        }
    }

    /// Iteration helper for tests and for `graft explain` listing modes.
    pub const ALL: &'static [DiagDomain] = &[
        Self::Validate,
        Self::Admit,
        Self::Materialize,
        Self::Promote,
        Self::Create,
        Self::Registry,
        Self::GitBridge,
    ];
}

/// A diagnostic code such as `V003`. Always rendered as `<letter><3-digit>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagCode {
    pub domain: DiagDomain,
    pub number: u16,
}

impl DiagCode {
    pub const fn new(domain: DiagDomain, number: u16) -> Self {
        Self { domain, number }
    }

    /// Parse a string like `"V003"`. Returns `None` for malformed inputs.
    pub fn parse(s: &str) -> Option<Self> {
        let mut chars = s.chars();
        let letter = chars.next()?;
        let domain = DiagDomain::from_letter(letter)?;
        let rest: String = chars.collect();
        if rest.is_empty() || rest.len() > 4 {
            return None;
        }
        let number: u16 = rest.parse().ok()?;
        Some(Self { domain, number })
    }

    /// True when `prefix` is a single letter matching this code's domain.
    /// Used by `graft explain V` style listing.
    pub fn matches_domain_prefix(&self, prefix: char) -> bool {
        prefix.to_ascii_uppercase() == self.domain.letter()
    }
}

impl fmt::Display for DiagCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{:03}", self.domain.letter(), self.number)
    }
}

/// A user-visible diagnostic. Rendered in three layers:
/// 1. precise locus (`loc`),
/// 2. one-line fix hint(s),
/// 3. see-also list of related concept ids or sibling codes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub code: DiagCode,
    /// Single-line summary of what this diagnostic means. No newlines.
    pub summary: String,
    /// Optional precise locus: candidate id, file path, property name, etc.
    /// Free-form because graft's "location" varies by subsystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loc: Option<String>,
    /// Single-line repair hints. Order matters; the first one is shown most
    /// prominently. Multi-line strings are considered a violation of the
    /// project's narrative rule and should be split into multiple hints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_hints: Vec<String>,
    /// Related concept ids (e.g. `"valid-patch"`) or sibling diagnostic codes
    /// (e.g. `"V004"`). Resolved by `graft explain` at render time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub see_also: Vec<String>,
}

impl Diagnostic {
    pub fn new(code: DiagCode, summary: impl Into<String>) -> Self {
        Self {
            code,
            summary: summary.into(),
            loc: None,
            fix_hints: Vec::new(),
            see_also: Vec::new(),
        }
    }

    pub fn at(mut self, loc: impl Into<String>) -> Self {
        self.loc = Some(loc.into());
        self
    }

    pub fn fix(mut self, hint: impl Into<String>) -> Self {
        self.fix_hints.push(hint.into());
        self
    }

    pub fn see(mut self, id: impl Into<String>) -> Self {
        self.see_also.push(id.into());
        self
    }

    /// Render this diagnostic as a single-line string suitable for embedding
    /// in user-facing fields that are themselves single-line (the most
    /// common one is `EvidenceResult::Unknown { reason }` /
    /// `EvidenceResult::Failed { reason }`).
    ///
    /// Format: `[CODE] summary @ loc — fix1 — fix2 — see: id1, id2`
    /// Sections after `[CODE] summary` are only emitted when present.
    /// Newlines and tabs in inputs are collapsed to single spaces.
    pub fn format_reason(&self) -> String {
        let mut out = format!("[{}] {}", self.code, collapse(&self.summary));
        if let Some(loc) = &self.loc {
            out.push_str(" @ ");
            out.push_str(&collapse(loc));
        }
        for hint in &self.fix_hints {
            out.push_str(" \u{2014} ");
            out.push_str(&collapse(hint));
        }
        if !self.see_also.is_empty() {
            out.push_str(" \u{2014} see: ");
            out.push_str(&self.see_also.join(", "));
        }
        out
    }

    /// Pull a [`DiagCode`] back out of a string previously produced by
    /// [`Self::format_reason`]. Returns `None` when the string does not
    /// start with a recognized `[CODE]` prefix.
    pub fn extract_code(reason: &str) -> Option<DiagCode> {
        let rest = reason.strip_prefix('[')?;
        let end = rest.find(']')?;
        let code = &rest[..end];
        DiagCode::parse(code)
    }
}

fn collapse(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diag_code_renders_with_three_digits() {
        assert_eq!(DiagCode::new(DiagDomain::Validate, 3).to_string(), "V003");
        assert_eq!(DiagCode::new(DiagDomain::Promote, 12).to_string(), "P012");
        assert_eq!(
            DiagCode::new(DiagDomain::GitBridge, 999).to_string(),
            "G999"
        );
    }

    #[test]
    fn diag_code_round_trips_through_parse() {
        for &domain in DiagDomain::ALL {
            for number in [0u16, 1, 7, 42, 999] {
                let code = DiagCode::new(domain, number);
                let rendered = code.to_string();
                let parsed = DiagCode::parse(&rendered).expect("parse");
                assert_eq!(parsed, code, "round trip failed for {rendered}");
            }
        }
    }

    #[test]
    fn diag_code_rejects_malformed_inputs() {
        for bad in ["", "X001", "V", "V0001a", "VV001", "003"] {
            assert!(DiagCode::parse(bad).is_none(), "expected None for {bad:?}");
        }
    }

    #[test]
    fn all_seven_domains_enumerated() {
        // Enforce the 7-letter contract from the thread plan: V/A/M/P/C/R/G.
        assert_eq!(DiagDomain::ALL.len(), 7);
        let letters: Vec<char> = DiagDomain::ALL.iter().map(|d| d.letter()).collect();
        assert_eq!(letters, ['V', 'A', 'M', 'P', 'C', 'R', 'G']);
    }

    #[test]
    fn next_action_kind_serializes_as_snake_case() {
        let action = NextAction::new(
            "validate",
            "graft patch validate candidate:xyz",
            NextActionKind::Recommended,
            "no evidence yet",
        );
        let json = serde_json::to_string(&action).expect("serialize");
        assert!(
            json.contains("\"kind\":\"recommended\""),
            "kind should serialize snake_case: {json}"
        );
        assert!(json.contains("\"id\":\"validate\""));
        assert!(json.contains("\"why\":\"no evidence yet\""));
    }

    #[test]
    fn diagnostic_json_rejects_unknown_fields() {
        let top_level_error = serde_json::from_str::<Diagnostic>(
            r#"{"code":{"domain":"VALIDATE","number":3},"summary":"base unmaterializable","surprise":true}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            top_level_error.contains("unknown field `surprise`"),
            "{top_level_error}"
        );

        let nested_error = serde_json::from_str::<Diagnostic>(
            r#"{"code":{"domain":"VALIDATE","number":3,"surprise":true},"summary":"base unmaterializable"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            nested_error.contains("unknown field `surprise`"),
            "{nested_error}"
        );
    }

    #[test]
    fn next_action_kind_labels_are_bracketed() {
        for (kind, expected) in [
            (NextActionKind::Recommended, "[recommended]"),
            (NextActionKind::Optional, "[optional]"),
            (NextActionKind::Terminal, "[terminal]"),
            (NextActionKind::Dangerous, "[dangerous]"),
        ] {
            assert_eq!(kind.label(), expected);
        }
    }

    #[test]
    fn diagnostic_builder_collects_fixes_and_see_also() {
        let diag = Diagnostic::new(
            DiagCode::new(DiagDomain::Validate, 3),
            "base unmaterializable",
        )
        .at("candidate:bd96ac3cd1ae")
        .fix("start scratch edits from --base graft:empty")
        .fix("turn the scratch into a candidate with graft candidate from-scratch")
        .see("valid-patch")
        .see("V004");
        assert_eq!(diag.code.to_string(), "V003");
        assert_eq!(diag.loc.as_deref(), Some("candidate:bd96ac3cd1ae"));
        assert_eq!(diag.fix_hints.len(), 2);
        assert_eq!(diag.see_also, vec!["valid-patch", "V004"]);
    }

    #[test]
    fn format_reason_is_single_line_with_code_prefix() {
        let diag = Diagnostic::new(
            DiagCode::new(DiagDomain::Validate, 3),
            "base could not be materialized",
        )
        .at("candidate:x")
        .fix("re-run inside a git repo")
        .see("valid-patch");
        let reason = diag.format_reason();
        assert!(
            reason.starts_with("[V003] "),
            "reason must start with [code]: {reason:?}"
        );
        assert!(
            !reason.contains('\n'),
            "reason must be single-line: {reason:?}"
        );
        assert!(reason.contains(" @ candidate:x"));
        assert!(reason.contains("re-run inside a git repo"));
        assert!(reason.contains("see: valid-patch"));
    }

    #[test]
    fn format_reason_collapses_multiline_inputs() {
        let diag = Diagnostic::new(
            DiagCode::new(DiagDomain::GitBridge, 2),
            "git subprocess returned an error",
        )
        .at("fatal: not a git repository\n  (or any of the parent directories)");
        let reason = diag.format_reason();
        assert!(!reason.contains('\n'), "reason: {reason:?}");
        assert!(reason.contains("fatal: not a git repository (or any of the parent directories)"));
    }

    #[test]
    fn extract_code_round_trips_format_reason() {
        let diag = Diagnostic::new(DiagCode::new(DiagDomain::Admit, 7), "need evidence");
        let reason = diag.format_reason();
        let code = Diagnostic::extract_code(&reason).expect("code");
        assert_eq!(code, diag.code);
    }

    #[test]
    fn extract_code_rejects_non_prefixed_strings() {
        assert!(Diagnostic::extract_code("not a code at all").is_none());
        assert!(Diagnostic::extract_code("[NOTREAL] foo").is_none());
        assert!(Diagnostic::extract_code("[V003 foo").is_none());
    }
}
