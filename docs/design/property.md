# §2.5–§2.6 Constraint language and evidence

## 2.5 Constraint language: `constraints.roto` → `ConstraintDef` → `Constraint` → `Plan`

Graft uses one workspace constraint source file:

```text
constraints.roto
```

`constraints.roto` is typechecked by Graft with a host-provided Roto runtime. A top-level function with this shape defines one named constraint definition:

```roto
fn cargo_tests_pass(app: Application) -> Constraint {
    primitive(
        observe_run(call(["cargo", "test", "--workspace"], app.target())),
        exit_code_is(0),
        "workspace tests pass",
    )
}
```

The final implementation model is three-layered:

```text
ConstraintDef { name, description, body: Constraint }

Constraint =
  | Top
  | Bottom
  | Primitive { plan: PlanId }
  | Both { left: Constraint, right: Constraint }
  | Either { left: Constraint, right: Constraint }

Plan { observation, assertion }
PlanId = blake3(canonical(observation, assertion))
```

Important separation rules:

- `ConstraintDef.name` is the user-visible top-level function name and lock/config key. It is **not** part of primitive identity.
- `ConstraintDef.description` is display/help metadata. It is **not** part of primitive identity.
- `Constraint::Primitive` points directly to a content-addressed `PlanId`.
- `PlanId` is derived only from the canonical observation/assertion pair.
- Logical composition is represented only by `Constraint::{Both,Either}`; there is no separate check-level `all_of` / `any_of` identity layer.
- There are no out-of-band priority or dependency fields in the current model. Required policy is expressed by composing constraints at admission/promotion sites.

### 2.5.1 Constraint plan identity

Invariant 2.5.1 (ConstraintPlanIdentity)

```text
PlanId = blake3(canonical(observation, assertion))
```

Names, descriptions, lock keys, source locations, policy location, and display text do not enter `PlanId`. Renaming a top-level Roto function changes how humans and config refer to a constraint, but it does not change the identity of any primitive whose observation/assertion body is unchanged.

This gives Graft two stable axes:

1. **Human/config axis** — `ConstraintDef.name` and `graft.lock [constraints.<name>]` let users refer to constraints by name.
2. **Evidence/admission axis** — evidence references the `PlanId` leaf actually observed and asserted by the verifier.

### 2.5.2 Roto host surface

The production Roto surface is deliberately small:

```roto
fn name(app: Application) -> Constraint

primitive(observation, assertion, description) -> Constraint
both(left, right) -> Constraint
either(left, right) -> Constraint
all_of(list) -> Constraint
either_any(list) -> Constraint
observe_run(run) -> Observation
call(argv, tree) -> RunPlan
```

Representative examples:

```roto
fn changed_docs(app: Application) -> Constraint {
    primitive(
        app.changed_paths(["docs/**"]),
        any_match,
        "docs changed",
    )
}

fn safe_patch(app: Application) -> Constraint {
    both(
        changed_docs(app),
        primitive(
            observe_run(call(["cargo", "test", "--workspace"], app.target())),
            exit_code_is(0),
            "tests pass",
        ),
    )
}
```

A top-level Roto function may call another top-level function returning `Constraint`; composition remains explicit in the returned `Constraint` AST.

### 2.5.3 Constraint lattice

`Constraint` is the admission/promotion expression state:

```text
Top        always satisfied
Bottom     never satisfied
Primitive  satisfied iff there is passing evidence for its PlanId
Both       left ∧ right
Either     left ∨ right
```

The kernel model in `formal/kernel.lean` keeps this lattice independent from verifier implementation details. Runtime evaluation can short-circuit, memoize, or batch observations, but the semantic object remains the same `Constraint` tree.

### 2.5.4 Stable composition semantics

`Stable c` means constraint `c` is closed under sequential composition of applications. The kernel includes:

- `stable_top`
- `stable_bottom`
- `stable_both`
- no `stable_either`

Evidence reuse for composed applications is gated, not universal. A stable primitive may authorize derived admission evidence for a composed application only by re-deriving through the public Compose relation plus parent passing evidence. If the relevant plan/body/policy drifts, if stable is withdrawn, or if parent evidence is unavailable, reuse must fail loud and the composed application must be validated directly.

Stable implementation details remain outside the Lean kernel trusted base; the kernel only models the semantic relation.

## 2.6 Evidence

Evidence is a runtime-generated record for a concrete subject and one primitive plan:

```text
EvidenceRecord {
  id,
  subject,   // candidate/patch/application scope
  plan,      // PlanId
  verifier,
  result,    // passed / failed / error / not_applicable
  ...
}
```

`graft-validate` provides plan/evidence helpers and constraint satisfaction checks; `graft-runtime` owns store-aware plan execution and command materialization. The runtime may memoize identical run observations by `(argv, materialized tree id)` so one command execution can feed multiple assertions, but each evidence record still points at the primitive `PlanId` it supports.

### 2.6.1 Evidence content addressing

Evidence identity is content-addressed from the canonical subject/plan/verifier/result payload. Local debug details such as wall-clock duration, sandbox path, raw logs, or host-specific paths are not part of the stable identity unless explicitly declared as relevant output.

### 2.6.2 Observational reproducibility

Graft does not treat evidence as a hand-written proof. A local verifier can rebuild evidence by rerunning the same plan under the same execution contract and comparing canonical outputs.

This is observational reproducibility:

```text
(subject, plan, verifier, execution_contract) + canonical result
  -> same EvidenceId
```

Admission checks require passing evidence for every primitive leaf demanded by the candidate/patch constraint and any additional admission/promotion policy constraints. Missing or failed evidence is reported as a structural constraint failure and rendered by runtime/explain as the appropriate user-facing diagnostic.

### 2.6.3 Evidence refs and rebuild

Public patch records store evidence refs as owner-indexed records. The evidence body may be absent in a fresh clone until rebuilt or fetched. If refs mention an evidence id whose body is unavailable, admit/promote fails loud and asks the user to run validation for the missing constraint rather than silently accepting the ref.

### 2.6.4 Promotion effects are not evidence

Promotion is an explicit external side-effect boundary. `graft patch promote --yes` may write a Git ref, local file, or remote target and produce a `PromotionRecord`, but this does not prove a constraint and does not mutate an `EvidenceRecord`.
