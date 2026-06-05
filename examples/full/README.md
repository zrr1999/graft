# Full example workspace

A complete pair of `properties.roto` + `graft.toml` showing how a real
workspace would wire properties into admission/promotion.

> Like the rest of `examples/`, this is design-track for the final Roto
> property language. The currently installed runtime accepts only the
> empty default `properties.roto`.

## Files

- [`properties.roto`](./properties.roto) — eight properties: structural
  predicates (`empty_change`, `only_touches_docs`, `no_generated_artifacts`),
  command oracles (`cargo_fmt_clean`, `cargo_clippy_clean`,
  `cargo_tests_pass`, `cargo_doc_tests_pass`), and one composition
  (`safe_patch` via `requires`).
- [`graft.toml`](./graft.toml) — references three properties as admission
  base and three properties as promotion-required, demonstrating different
  policy strata for the same workspace.

## Wiring summary

```text
admission base       = no_generated_artifacts ∧ cargo_fmt_clean ∧ cargo_tests_pass
promotion required   = safe_patch ∧ cargo_clippy_clean ∧ cargo_doc_tests_pass
                      (safe_patch itself requires no_generated_artifacts ∧ cargo_tests_pass)
```

`cargo_clippy_clean` declares `Severity.Warning`, so an evidence record
with `Failure` would not block admission on its own; it only affects
promotion because `graft.toml` lists it under `[promotion]`.
