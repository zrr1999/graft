# Roto property-language PoC

Status: production `properties.roto` loading lives in `graft-runtime`; this PoC document tracks the isolated `graft-validate` host-binding regression coverage for `roto = 0.11.0`.

## Goal

Validate and document the boundary between the production property source (`properties.roto` loaded by `graft-runtime`) and the isolated Roto host-binding PoC tests. Graft uses a single `properties.roto` source file whose top-level property functions return host-owned static property plans:

```roto
fn no_generated_artifacts(app: Application) -> Property {
    property(
        [
            app.changed_paths().any_match([
                "target/**",
                "dist/**",
                "build/**",
            ]).failure(),
        ],
        "patch does not contain generated build artifacts",
        Severity.Blocking,
        [],
    )
}

fn cargo_tests_pass(app: Application) -> Property {
    let run = call(["cargo", "test", "--all-targets"], app.target());

    property(
        [
            run.exit_code_is(0).success(),
        ],
        "cargo test --all-targets passes on the target tree",
        Severity.Blocking,
        [],
    )
}

fn safe_patch(app: Application) -> Property {
    property(
        [],
        "patch passes both artifact policy and the test suite",
        Severity.Blocking,
        [
            "no_generated_artifacts",
            "cargo_tests_pass",
        ],
    )
}
```

The boundary is:

```text
properties.roto source
  -> compile/typecheck with Graft-provided Roto runtime
  -> execute property functions against symbolic host Application values
  -> return Graft-owned PropertyPlan / CheckPlan templates
  -> Graft runtime evaluates plans and creates EvaluationRecord/EvidenceRecord
```

Roto source constructs plans only. It cannot create `EvaluationRecord`, `EvidenceRecord`, ids, admissions, or patches.

## Dependency status

`graft-runtime` has the production Roto dependency used by the workspace property loader:

```toml
[dependencies]
roto = "0.11.0"
```

`graft-validate` keeps an isolated dev-dependency for the host-binding PoC/regression fixture:

```toml
[dev-dependencies]
roto = "0.11.0"
```

## Passing evidence

Production loader/config boundary:

```text
$ cargo test -p graft-runtime config --locked
running 23 tests
...
test result: ok. 23 passed; 0 failed; 0 ignored
```

Isolated PoC host-binding boundary:

```text
$ cargo test -q -p graft-validate --test roto_property_language_poc
running 3 tests
...
test result: ok. 3 passed; 0 failed; 0 ignored
```

The passing integration test demonstrates:

1. Top-level `fn name(app: Application) -> Property` functions can return host-owned `Property` values.
2. There is no `property_registry()` in the v2 fixture; composition uses `requires` by exact property name.
3. The host constructor `property(checks, description, severity, requires)` lowers to `graft_core::PropertyPlan` plus display metadata.
4. Metadata-only drift (`description`, `severity`) does not change `PropertyId`; semantic drift (`name`, `checks`, `requires`) does.
5. Structural probes work through host methods:
   - `app.changed_paths().any_match([...]).success()` / `.failure()`
   - `app.changed_paths().all_match([...]).success()`
6. Command probes work through deferred run plans:
   - `call([...], app.target()).exit_code_is(0).success()`
7. Relational probes work through symbolic run selectors:
   - `same_output(base_run, target_run, [post_file("..."), stdout]).success()`
8. Historical and file references are symbolic:
   - `app.previous_failure(History.First)`
   - `app.target().file("...")`
   - `prev.target().with_overlay([replace_file("...", file)])`

## Current fixture shape

Production smoke/templates use top-level `properties.roto` files under the workspace/templates. The representative isolated PoC fixture is:

```text
crates/graft-validate/tests/fixtures/properties.roto
```

It covers:

- `no_generated_artifacts`: path denylist via `any_match(...).failure()`;
- `cargo_tests_pass`: command exit-code probe on `app.target()`;
- `safe_patch`: empty local checks plus `requires` dependencies;
- `precision_invariance`: `same_output` over base/target runs;
- `training_alignment`: symbolic historical failure + file overlay + `any_of`.

## Roto 0.11 interop constraints discovered

### Host `Property`, not Roto `record Property`

Roto-native records compile, but they do not cross the Rust host boundary through `get_function`. Roto FFI accepts built-in `Value` types and registered custom `Val<T>` types. Therefore Graft cannot retrieve a workspace-declared `record Property { ... }` as a Rust `PropertySpec` without a separate AST lowerer.

The v2 production surface uses the host constructor instead:

```roto
property(checks, description, Severity.Blocking, requires) -> Property
```

### Host methods, not host fields

Registered custom host types expose methods, not fields. Therefore endpoints are method calls:

```roto
app.base()
app.target()
prev.target()
```

This keeps `Application` and `Tree` host-owned symbolic plan values.

### `match` is reserved

`match` is a Roto keyword, and the `library!` macro cannot register raw identifiers such as `r#match` as user-visible method names. The path probe spelling is therefore:

```roto
paths.any_match(patterns)
paths.all_match(patterns)
```

`any_match(...).failure()` is the canonical “no path matches these patterns” shape.

### Public namespaces may differ from internal host type names

Roto modules and type names share a namespace. To expose `Severity.Blocking` while still passing a host severity value to `property(...)`, the host binding may register the underlying type under an internal name and expose a public `Severity` module of constants. Users normally rely on inference and do not annotate this internal type.

### Avoid zero-sized custom host values

A one-variant zero-sized `PathSetPlan` wrapped directly as `Val<PathSetPlan>` triggered a Roto 0.11 segmentation fault when used through `changed_paths().any_match(...)`. The PoC uses a non-zero host wrapper for `PathSet` and lowers it to the core semantic `PathSetPlan` inside the method.

## Known Roto ICE retained as upstream context

Roto 0.11.0 can still ICE when a Roto function calls another user-defined Roto function returning a custom host plan type. The minimized old fixture remains as upstream context:

```text
crates/graft-validate/tests/fixtures/properties_check_composition_ice.roto
```

The v2 design avoids property-to-property function calls for policy composition. Composition is expressed with `requires`, and helper functions returning custom host plan types should be treated with care until the loader/runtime has explicit tests for the desired helper surface.

There is no v2 preflight that rewrites this into a legacy Graft error; production loading should either rely on Roto diagnostics or catch/report compiler panics as loader errors without mutating workspace state.
