# Graft constraint language examples

> Status: examples for the Roto constraint language. Production loader/lock
> code discovers top-level `fn name(app: Application) -> Constraint` functions
> from `constraints.roto`; primitive leaves point to content-addressed `Plan`s.

Each file under `examples/constraints/` is a self-contained pattern intended
to be readable in isolation. `examples/full/constraints.roto` shows several
patterns composed in one workspace source file the way a real project would
write it.

## Conventions

- A named constraint is a top-level function `fn name(app: Application) -> Constraint`.
  The function name is the name exposed to graft. There is no separate
  `EmptyChange` PascalCase alias and no `constraint_registry()`.
- A primitive leaf is built with `primitive(observation, assertion, description)`.
  The `Plan { observation, assertion }` is content-addressed; `description` is
  display text and does not feed identity.
- Use `both(left, right)` / `either(left, right)` or n-ary `all_of([...])` /
  `either_any([...])` to compose constraints. There is no separate `requires`
  list.
- Common assertions include `any_match`, `all_match`, `no_match`, `exit_zero`,
  `exit_nonzero`, `outputs_same`, and `outputs_differ`.
- Runtime-dependent values are symbolic plan references: `tree.file(path)`
  builds a `FileRef`, and `app.previous_failure(History.First/Last/Get(n))`
  builds a historical `Application` reference. Missing files or witnesses
  evaluate to `Unknown` when a run needs them.
- Sandbox defaults: no timeout, network allowed, filesystem outside the input
  tree readable. Determinism is the constraint author's responsibility.

## Index

| File | Pattern |
| --- | --- |
| `constraints/empty_change.roto` | structural predicate over the path set |
| `constraints/only_touches_docs.roto` | path policy with whitelist match |
| `constraints/no_generated_artifacts.roto` | path policy with denylist match |
| `constraints/cargo_tests_pass.roto` | command oracle on `app.target()` |
| `constraints/cargo_clippy_clean.roto` | command oracle for clippy |
| `constraints/precision_invariance.roto` | relational `same_output` over base/target |
| `constraints/training_alignment.roto` | falsifiable command oracle with previous-failure witness |
| `constraints/safe_patch.roto` | composition via `both` |
| `full/constraints.roto` | multiple constraints in one file |
