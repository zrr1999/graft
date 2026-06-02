//! Stand-alone lookup engine behind `graft explain <id>`.
//!
//! Three id namespaces are routed here:
//!
//! - **Diagnostic codes**: `V003`, `A007`, etc. Source: [`super::diagnostics`].
//! - **Builtin properties**: `valid_patch`, `paths_none_match`, etc. Source:
//!   [`super::properties`].
//! - **Concept ids**: free-form names like `admit`, `materialize`, `properties`,
//!   `valid-patch`. The catalog is supplied by the caller (typically the CLI,
//!   so it can read clap-derived `about` strings as the single source of truth).
//!
//! Unknown ids return [`ExplainResult::Unknown`] with a small list of
//! "did you mean" suggestions ranked by Levenshtein distance.
//!
//! This module is `serde`-friendly: every `ExplainResult` is a structured
//! payload that the CLI can render as Markdown-free human text or stream as
//! `--json`.

use crate::diagnostics::{ALL_DIAGNOSTICS, DiagnosticDoc, doc_for};
use crate::properties::{
    ALL_BUILTINS, BuiltinPropertyMetadata, metadata_for_check_or_property_name,
};
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
    /// Optional single-line elaboration; `long_about` from clap when set.
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
    /// `id` matched a builtin verifier property.
    BuiltinProperty(BuiltinPropertyView),
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

/// Serializable view over a [`BuiltinPropertyMetadata`].
#[derive(Debug, Serialize)]
pub struct BuiltinPropertyView {
    pub id: &'static str,
    pub summary: &'static str,
    pub requires_base: bool,
    pub failure_modes: &'static [&'static str],
}

impl From<&'static BuiltinPropertyMetadata> for BuiltinPropertyView {
    fn from(meta: &'static BuiltinPropertyMetadata) -> Self {
        Self {
            id: meta.id,
            summary: meta.summary,
            requires_base: meta.requires_base,
            failure_modes: meta.failure_modes,
        }
    }
}

/// Resolve `id` against the supplied concept catalog and graft-explain's
/// own diagnostic and builtin-property registries.
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

    // 3. Builtin property check id (lowercase spelling) or default property name.
    if let Some(meta) = metadata_for_check_or_property_name(id) {
        return ExplainResult::BuiltinProperty(meta.into());
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
                out.push_str(&format!("  {long}\n"));
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
        ExplainResult::BuiltinProperty(b) => {
            out.push_str(&format!("builtin property: {}\n", b.id));
            out.push_str(&format!("  {}\n", b.summary));
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
    fn builtin_property_id_resolves_to_metadata() {
        let r = lookup("valid_patch", &concepts());
        match r {
            ExplainResult::BuiltinProperty(b) => {
                assert_eq!(b.id, "valid_patch");
                assert!(b.requires_base);
            }
            other => panic!("expected builtin property, got {other:?}"),
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
}
