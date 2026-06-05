# Graft property language examples

> Status: examples for the **final** Roto property language. Production
> loader/lock code discovers top-level `fn name(app: Application) -> Property`
> functions from `properties.roto` and derives `PropertyId` from the static
> property plan identity.

Each file under `examples/properties/` is a self-contained pattern intended
to be readable in isolation. `examples/full/properties.roto` shows several
patterns composed in one workspace source file the way a real project would
write it.

## Conventions

- A property is a top-level function `fn name(app: Application) -> Property`.
  The function name is the `PropertyName` exposed to graft. There is no
  separate `EmptyChange` PascalCase alias and no `property_registry()`.
- The host constructor `property(checks, description, severity, requires)`
  returns the `Property` plan. The `requires` argument is a required list of
  other property names that must hold before this property is evaluated; pass
  `[]` when the property has no dependencies. `description` and `severity` are
  display metadata and do not feed `PropertyId`; `checks` and `requires` do.
- `all_of([...])` and `any_of([...])` are the only logical combinators. Both
  are lazy; evidence records `branch_short_circuited_at`.
- Negation is expressed at the leaf via `probe.success()` / `probe.failure()`;
  there is no `not()` combinator.
- Runtime-dependent values are symbolic plan references: `tree.file(path)`
  builds a `FileRef`, and `app.previous_failure(History.First/Last/Get(n))`
  builds a historical `Application` reference. Missing files or witnesses
  evaluate to `Error` when a run/probe needs them. Ordinary Roto `Option` is
  allowed only for values known while building the static template, not for
  runtime history/file existence.
- `Severity` values: `Severity.Blocking`, `Severity.Warning`, `Severity.Info`.
- Sandbox defaults: no timeout, network allowed, filesystem outside the input
  tree readable. Determinism is the property author's responsibility.

## Index

| File | Pattern |
| --- | --- |
| `properties/empty_change.roto` | structural predicate over the path set |
| `properties/only_touches_docs.roto` | path policy with whitelist match |
| `properties/no_generated_artifacts.roto` | path policy with denylist match |
| `properties/cargo_tests_pass.roto` | command oracle on `app.target()` |
| `properties/cargo_clippy_clean.roto` | command oracle with `Severity.Warning` |
| `properties/precision_invariance.roto` | relational `same_output` over base/target |
| `properties/training_alignment.roto` | falsifiable command oracle with previous-failure witness |
| `properties/safe_patch.roto` | composition via `requires` |
| `full/properties.roto` | multiple properties in one file |
