# Full example workspace

A complete pair of `constraints.roto` + `graft.toml` showing how a real
workspace wires named constraints into admission/promotion.

## Files

- [`constraints.roto`](./constraints.roto) — eight constraints: structural
  predicates (`empty_change`, `only_touches_docs`, `no_generated_artifacts`),
  command oracles (`cargo_fmt_clean`, `cargo_clippy_clean`,
  `cargo_tests_pass`, `cargo_doc_tests_pass`), and one composition
  (`safe_patch` via `all_of`).
- [`graft.toml`](./graft.toml) — references three constraints as admission
  base and three constraints as promotion-required, demonstrating different
  policy strata for the same workspace.

## Wiring summary

```text
admission base       = no_generated_artifacts ∧ cargo_fmt_clean ∧ cargo_tests_pass
promotion required   = safe_patch ∧ cargo_clippy_clean ∧ cargo_doc_tests_pass
safe_patch           = no_generated_artifacts ∧ cargo_fmt_clean ∧ cargo_tests_pass
```

Severity is no longer part of the core constraint model; blocking vs
non-blocking behavior belongs in admission/promotion policy.
