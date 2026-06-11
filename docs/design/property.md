# Graft 设计 · 属性与证据（§2.5–§2.6）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`../graft-kernel.lean`](../graft-kernel.lean)。

### 2.5 Property language: `properties.roto` → Property plan → PropertyId

Graft uses one workspace property source file:

```text
properties.roto
```

`properties.roto` is typechecked by Graft with a host-provided Roto runtime.
The final user-facing surface is deliberately small:

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
        "patch passes both artifact policy and tests",
        Severity.Blocking,
        [
            "no_generated_artifacts",
            "cargo_tests_pass",
        ],
    )
}
```

A top-level function with signature `fn name(app: Application) -> Property` is
a property. The function name is the property name. There is no
`property_registry()`, no PascalCase alias, and no comment metadata such as
`// property:` or `// title:`. Comments, whitespace, and local variable names do
not affect behavior.

Roto source constructs **plans** only. It cannot construct `EvaluationRecord`,
`EvidenceRecord`, `EvidenceId`, `ApplicationId`, `PatchId`, or any admission
result; those are runtime-owned. User-facing source also does not expose the
legacy `query/evaluator/judge` triple. Internally, Graft still lowers the Roto
plan into canonical `observe -> compute -> decide` nodes.

There is no user-facing "built-in property" category. Runtime primitives are
ordinary plan builders (`changed_paths`, `any_match`, `call`, `exit_code_is`,
`same_output`, etc.). Generated presets, templates, or examples are normal
`fn name(app) -> Property` definitions once present in `properties.roto`.

#### Property constructor

`Property` is a Graft host-owned plan type, not a workspace-declared Roto
`record`. Roto code constructs it with the host constructor:

```roto
property(
    checks: List[Check],
    description: String,
    severity: Severity,
    requires: List[String],
) -> Property
```

`Severity` is exposed through host constants:

```roto
Severity.Blocking
Severity.Warning
Severity.Info
```

- `checks` is the finite proof obligation for this property.
- `description` is human-readable display metadata.
- `severity` controls reporting/admission policy. `Blocking` gates admission or
promotion when required; `Warning` and `Info` produce evidence without
becoming gates unless a command explicitly requires them.
- `requires` names other properties that must succeed before this property's
own `checks` are evaluated.

`description` and `severity` are not semantic identity. `checks`, `requires`,
and the top-level function name are semantic identity.

#### Check, Probe, and polarity

A property is not a Boolean. It is a proof obligation whose leaves are
**probes** with explicit polarity:

```text
ProbeResult = Success | Failure | Error
Check       = probe.success() | probe.failure() | all_of([...]) | any_of([...]) | unavailable(reason)
```

Rules:

- `probe.success()` is satisfied only by `ProbeResult::Success`.
- `probe.failure()` is satisfied only by `ProbeResult::Failure`.
- `ProbeResult::Error` satisfies neither polarity.
- There is no `not(check)` combinator. Negation is pushed to the leaf by
choosing `.failure()` instead of `.success()`.
- `unavailable(reason)` constructs an explicit `Check` whose evaluation result
is `Error`; use it for statically expressed branches that intentionally have
no valid proof input. Runtime-dependent file/history absence is represented by
symbolic references that evaluate to `Error` when consumed.

`all_of([...])` and `any_of([...])` are the only compound combinators. Both are
lazy:

- `all_of` stops at the first child that is not satisfied.
- `any_of` stops at the first child that is satisfied.
- Evidence records the short-circuit index as `branch_short_circuited_at`.
- Empty `all_of([])` / `any_of([])` is rejected at load time; an empty
`Property.checks` list is allowed only when `requires` carries the policy.

#### Core host types and primitives

```roto
// Application
app.base()                    // Tree before the change
app.target()                  // Tree after the change
app.changed_paths()         // PathSet for base -> target
app.previous_failure(sel)   // symbolic historical failure for this PropertyId

// PathSet probes
paths.any_match(patterns)       // .success() = any path matches; .failure() = none matches
paths.all_match(patterns)   // .success() = every changed path matches

// Tree/File/overlay
text_or_tree.file(path)     // symbolic FileRef; missing file => runtime Error
replace_file(path, file)    // Overlay
text_or_tree.with_overlay(overlays)

// Command runs
let run = call(argv, tree)
run.exit_code_is(code)

// Run selectors and relational probes
stdout
stderr
post_file(path)
same_output(run_a, run_b, selectors)
```

`FileRef` is content-addressed by blob hash. The same bytes from different
source trees produce the same `FileRef`. `tree.file(path)` is a symbolic file
reference inside the static plan; if validation cannot find that file in the
runtime tree, the probe or run that needs it evaluates to `Error`.

`app.previous_failure(selector)` is a symbolic historical application reference.
`selector` is one of `History.First`, `History.Last`, or `History.Get(n)`. The
lookup key is the current `PropertyId`, so the reference cannot be a normal Roto
`Option` computed during source loading. If validation cannot find the selected
historical failure, the probe or run that needs it evaluates to `Error`.

The visible history is restricted to applications whose target is `app.base()` or
an ancestor of `app.base()`; the current `app.target()`, future states, and
sibling-branch future states are invisible. Returned historical applications are
read-only views; calling `.previous_failure(...)` on such a view evaluates to
`Error`.

Roto `Option` may still be used for values known while building the static
property template, but runtime-dependent sources such as historical failures and
files inside a tree are symbolic references, not ordinary `Option` values.

Example with symbolic historical and file references:

```roto
fn training_alignment(app: Application) -> Property {
    let target_run = call(["bash", "./check_diff.sh"], app.target());

    let prev = app.previous_failure(History.First);
    let checker = app.target().file("./check_diff.sh");
    let bad_tree = prev.target().with_overlay([
        replace_file("./check_diff.sh", checker),
    ]);
    let bad_run = call(["bash", "./check_diff.sh"], bad_tree);

    property(
        [
            target_run.exit_code_is(0).success(),
            any_of([
                app.changed_paths().any_match([
                    "check_diff.sh",
                    "compare.py",
                ]).failure(),
                bad_run.exit_code_is(0).failure(),
            ]),
        ],
        "validator is unchanged or still rejects a historical counterexample",
        Severity.Blocking,
        [],
    )
}
```

#### Command execution contract

`call(argv, tree)` constructs a deferred run node. Evaluation materializes the
input tree under:

```text
.graft/store/derived/worktrees/<tree-id>/
```

and runs `argv` with cwd forced to that materialized tree root.

Default execution contract:

- no timeout limit;
- network is allowed;
- filesystem access outside cwd is allowed by the host process model;
- identical run nodes are deduplicated within one evaluation pass;
- validation does not mutate the user's cwd or tracked workspace files.

These defaults are intentionally permissive. If a property needs stronger
reproducibility, it must express that as ordinary checks or use a future
explicit sandbox contract. Evidence records the observed run, not a claim that
arbitrary external state was hermetic.

#### `requires` dependency graph

`requires` is property-to-property dependency, not a Roto function call. This is
the only v2 composition mechanism for named policies.

Evaluation semantics:

1. Load all top-level property functions and build the dependency graph.
2. Reject unknown dependency names and cycles at load time.
3. Evaluate dependencies before the dependent property.
4. If every dependency is `Success`, evaluate the dependent property's own
  `checks`.
5. If any dependency is `Failure` or `NotApplicable`, the dependent property is
  `NotApplicable`; its own `checks` do not run.
6. If any dependency is `Error`, the dependent property is `Error`.
7. Dependency results are memoized during one evaluation pass.

This avoids user-function-to-user-function check composition while preserving a
clean policy graph. Graft does not add a preflight ban on helper calls; Roto
compilation is contained and reported as property-source compilation failure.
Roto compiler panics must not escape the daemon process.

#### PropertyId and names

Each loaded property produces a `PropertyDef`:

```rust
struct PropertyDef {
    name:        PropertyName,      // top-level function name, exact spelling
    plan:        PropertyPlan,      // canonical semantic plan
    description: String,            // display only
    severity:    Severity,          // display/admission policy only
    source_ref:  PropertySourceRef, // properties.roto:function_name
}

// PropertyId = blake3(canonical(name, plan.checks, plan.requires))
```

`description`, `severity`, comments, whitespace, and local variable names are not
hashed. Editing `checks`, `requires`, or the top-level function name changes the
`PropertyId`. Evidence references `property:<digest>`, not a mutable display
alias.

```
Invariant 2.5.1  (PropertyNameIsIdentityInput)
  properties.roto 中的顶层函数名是 property 的用户可见名字，且进入
  PropertyId。没有单独 registry alias 层。改名是语义变更；改
  description/severity 不是语义变更。
```

`graft.lock` caches the currently resolved mapping:

```text
properties.roto function cargo_tests_pass
  -> PropertyPlan
  -> property:<digest> + check_hash
  -> graft.lock [properties.cargo_tests_pass]
```

Operations:

- Rename `cargo_tests_pass` to `tests_pass`: new property name and new
`PropertyId`; old evidence remains queryable by old `property:<digest>` but no
longer satisfies config entries requiring `cargo_tests_pass`.
- Edit only `description` or `severity`: `PropertyId` unchanged.
- Edit `checks` or `requires`: `PropertyId` changes; old evidence remains in
the store but no longer satisfies current admission/promotion requirements.
- Delete a function: config entries naming it fail with `[E_UNKNOWN_PROPERTY]`.

#### Constraint lattice

`Constraint` is admission/promotion expression state, not property source.
A `Property` remains the user-authored body in `properties.roto`; a
`Constraint::Primitive` is a resolved reference to one such property body. The lattice
operators compose those primitives into the proof obligation that `admit` or
`promote` must satisfy for one concrete `Application`.

形式定义见 [`docs/graft-kernel.lean`](../graft-kernel.lean)：`satisfies` / `top` / `bottom`
/ `both`(`⊓`) / `either`(`⊔`) 在「核心模型」段，`satisfies_top`、`satisfies_bottom`、
`satisfies_both`、`satisfies_either` 四条 axiom 在「核心公理」段。`Top` 总被满足，
`Bottom` 从不被满足，`Both` 要两支都满足，`Either` 至少一支满足。`satisfies` 是 opaque
谓词、无 functoriality 公理。

**约束传递性（`Stable`，已落地）**：「核心定义」段引入谓词 `Stable c`，意为“C 在顺序组合
下封闭”：两端各自满足 C 且可组合，则复合体也满足 C。这是关于*单个共享约束*的命题，
不是“两个不同约束取 ⊓”（后者一般为假：反例 c=“target 含 /tmp/foo”，a 创建、b 删除）。
配套定理：`stable_top` / `stable_bottom`（上下界平凡稳定）、`stable_both`（`⊓` 封闭，故
多个 stable 约束可自由交；`⊔` **不**封闭，无 `stable_either`），以及 `certifyComposedStable`
（两端携带同一 stable 约束时，复合体认证无需新证据义务）。sound 来源：预算/量化类约束
（如“diff ≤ 10 行”）**不声明** `Stable`，传播对它自动失效。注意：`Stable` 是 kernel 层
soundness，**不**授权 runtime 跳过 evidence——复合体是新 application / 新 id，仍按 [§4.5](./lifecycle.md) 重跑。

状态三分（wellformed / consistent / resolved）以及其按 primitive 的稳定性断言仍为 kernel 末尾 TODO，
待后续讨论再落地。

Rust stores the same algebra explicitly:

```rust
enum Constraint {
    Top,
    Bottom,
    Primitive { property: PropertyRef },
    Both { left: Box<Constraint>, right: Box<Constraint> },
    Either { left: Box<Constraint>, right: Box<Constraint> },
}
```

`Constraint::Primitive` must carry the resolved `PropertyId`; storing only a name
would make a candidate's required policy drift when `properties.roto` changes.
The same `Constraint` AST appears in candidate and patch records:

- `candidate.constraint`
- `patch.constraint`

`Top` is always satisfied. `Bottom` is never satisfied. `Both(left, right)` is
satisfied only when both branches are satisfied. `Either(left, right)` is
satisfied when at least one branch is satisfied. Repeated CLI flags and flat
`required_properties = ["a", "b"]` syntax are sugar for a right-associated
`Both` tree.

Properties are evaluated over one whole application state. In a multi-repo
workspace, repos are directories inside that state, normally
`worktrees/<repo-id>/`; they are not separate property namespaces. A property
can compare repos by reading or running commands against paths in `.base` and
`.target`, for example `worktrees/A/...` and `worktrees/B/...`.

`graft.toml` binds admission or promotion constraints to property bodies by name:

```rust
PropertyRef {
    id:       PropertyId,
    name:     PropertyName,
}
```

The display and CLI format is the property name, for example
`graft_config_current`, `c_cargo_tests_pass`, or `ab_task_output_same`.
`properties.roto` defines the property body and can inspect the entire state.
`graft.toml` only chooses which constraints are required:

```toml
[admission]
required_properties = [
  "graft_config_current",
  "a_empty_change",
  "b_empty_change",
  "c_non_empty_change",
  "c_cargo_tests_pass",
  "ab_task_output_same",
]

[promotion]
required_properties = ["c_cargo_tests_pass"]
```

Evidence/admission lookups are keyed by `(subject, property_id)`. If a property
requires another property, `requires` refers to another top-level property body
over the same application; requirement expansion produces additional
`Constraint::Primitive` leaves under `Both`, not a separate property namespace.

#### Roto interop and host-binding

> 本小节并入自原独立文档 `docs/roto-property-language-poc.md`（已删除）。它记录 production `properties.roto` 加载（`graft-runtime`）与隔离 host-binding 回归（`graft-validate`）之间的边界，以及 `roto = 0.11.0` 下已知的 interop 约束。

Source → plan 边界：

```text
properties.roto source
  -> compile/typecheck with Graft-provided Roto runtime
  -> execute property functions against symbolic host Application values
  -> return Graft-owned PropertyPlan / CheckPlan templates
  -> Graft runtime evaluates plans and creates EvaluationRecord/EvidenceRecord
```

Roto 源只构造 plan，不能创建 `EvaluationRecord`、`EvidenceRecord`、id、admission 或 patch。

Dependency status：

- `graft-runtime`：production loader 依赖 `roto = "0.11.0"`。
- `graft-validate`：隔离 dev-dependency `roto = "0.11.0"`，承载 PoC/regression fixture。

Passing evidence：

```text
$ cargo test -p graft-runtime config --locked                        # 23 passed
$ cargo test -q -p graft-validate --test roto_property_language_poc  # 3 passed
```

集成测试证明：top-level `fn name(app: Application) -> Property` 可返回 host-owned `Property`；无 `property_registry()`，组合靠 `requires` 精确名引用；`property(checks, description, severity, requires)` 下沉为 `graft_core::PropertyPlan` 加展示元数据；metadata-only drift（`description` / `severity`）不改 `PropertyId`，语义 drift（`name` / `checks` / `requires`）会改；结构 / 命令 / 关系探针分别通过 `changed_paths().any_match([...])`、`call([...], app.target()).exit_code_is(0)`、`same_output(base, target, [...])`；历史与文件引用是符号化的（`previous_failure`、`app.target().file(...)`、`with_overlay([replace_file(...)])`）。

Representative fixture `crates/graft-validate/tests/fixtures/properties.roto`：`no_generated_artifacts`（path denylist `any_match(...).failure()`）、`cargo_tests_pass`（`app.target()` 上 exit-code 探针）、`safe_patch`（空 checks 加 `requires`）、`precision_invariance`（`same_output`）、`training_alignment`（历史失败 + overlay + `any_of`）。

Roto 0.11 interop 约束：

1. **Host `Property`，不是 Roto `record Property`**：Roto-native record 不跨 Rust host 边界（`get_function`）。production 用 host constructor `property(checks, description, Severity.Blocking, requires) -> Property`。
2. **Host 方法，不是 host 字段**：endpoint 是方法调用 `app.base()` / `app.target()` / `prev.target()`，保持 `Application` / `Tree` 为 host-owned 符号 plan 值。
3. **`match` 是保留字**：路径探针拼写为 `paths.any_match(patterns)` / `paths.all_match(patterns)`；`any_match(...).failure()` 是 canonical「无路径匹配」形状。
4. **Public namespace 可能不同于内部 host type 名**：为暴露 `Severity.Blocking` 同时向 `property(...)` 传 host severity，host binding 可把底层类型注册为内部名，并暴露公开 `Severity` 常量模块。
5. **避免 zero-sized custom host value**：把单变体 zero-sized `PathSetPlan` 直接 `Val<PathSetPlan>` 会触发 Roto 0.11 段错误；PoC 用非零 host wrapper 承载 `PathSet`，在方法内下沉到核心 `PathSetPlan`。

Known ICE（上游上下文）：Roto 0.11.0 在「Roto 函数调用另一个返回 custom host plan 类型的用户 Roto 函数」时仍可能 ICE；最小化旧 fixture `crates/graft-validate/tests/fixtures/properties_check_composition_ice.roto` 保留为上游上下文。v2 设计用 `requires` 表达组合，避免 property-to-property 函数调用；production loading 应依赖 Roto diagnostics 或把 compiler panic 报为 loader error，而不改 workspace state。

### 2.6 Evidence

#### 模型

Evidence 是 `(Application, property)` 在某次 verifier 运行下的 canonical observation：

```rust
struct EvidenceRecord {
    id:             EvidenceId,                 // blake3(canonical(seed))
    application:    ApplicationId,
    change:         ChangeId,                   // endpoint view, for query/display compatibility
    property:       PropertyId,
    verifier:       VerifierId,                 // 例如 "changed_paths_any_match" / "cargo-test"
    validation_key: ValidationRunKey,
    result:         EvidenceResult,             // passed | failed | unknown | skipped
    effects:        Vec<EffectRecord>,          // observed verifier effects, canonicalized
    outputs:        RelevantOutputRecord,
}
// EvidenceId seed = (application, property, verifier, validation_key, result, effects, outputs)
// 注意：seed 不含 hostname、timestamp、candidate/patch id、run-id 或绝对 sandbox path。
// created_at / observed_at 是 local metadata，不进 EvidenceId。
```

#### 不变量：Evidence reproducibility

```
Invariant 2.6.1  (EvidenceContentAddressing)
  EvidenceId 完全由 (Application, property, verifier, ValidationRunKey,
  result, canonical effects, relevant outputs) 决定，不绑定运行 host。

  推论：
    在 host A 运行得到 evidence:E1
    在 host B 运行同 ValidationRunKey，canonical record 一致 ⇒ 同 ID = E1
    canonical record 不一致 ⇒ 不同 ID（典型例子：host A passed, host B failed，
    或 relevant stdout digest 不同）

  这让"本地重建 evidence"成为内容寻址 hash 匹配，无需跨 host 信任。
```

```
Invariant 2.6.2  (ObservedEvidenceReproducibility)
  Evidence 的可重现性是对 canonical EvaluationRecord 的可重现性，
  不是对 verifier 原始 stdout/stderr、运行时间、绝对路径、host metadata 的
  bit-for-bit 复现承诺。

  给定 (Application, property, verifier, execution_contract, sandbox_profile,
  relevant_output_spec)，verifier 在隔离 sandbox 中运行后应产出同一个
  canonical EvaluationRecord。若 canonical record 不同，则得到不同
  EvidenceId，并被报告为本地复现差异。

  v1 不承诺屏蔽所有不可控来源（时钟、RNG、硬件性能、外部网络）。依赖这些
  来源的 verifier 必须把相关约束写进 execution_contract / relevant_output_spec，
  或把结果标记为 host-specific / observational。capabilities / stability 字段
  后续补充。
```

#### 沙箱化幂等运行

Verifier **永远在 sandboxed validation run 中运行**，且不写用户 cwd。
Graft 区分可复用的 clean target worktree 和一次性 writable run view：

```text
Application(base_state, action, applicability_proof, target_state)
  -> WorktreeCacheKey(application_id, target_tree_id, materializer_version,
                      file_mode_semantics, symlink_semantics, platform_family)
  -> .graft/store/derived/worktrees/<key>/root/      # clean, read-only by policy
  -> $GRAFT_HOME/run/validation/<run-id>/            # disposable writable view
  -> canonical EvaluationRecord
  -> evidence:<digest>.json in store/derived/evidence/
```

Reuse 规则：

```text
1. 若 WorktreeCacheKey 命中且 manifest/tree digest 校验通过，复用 clean target root。
2. 每次 verifier 执行都从 clean target root 派生新的 writable run view。
3. run view 可写；clean target root 不可写，运行后必须仍然 clean。
4. EvidenceRecord/EvaluationRecord 的 canonical seed 不含 run-id、绝对 sandbox 路径、hostname、timestamp。
5. 若 ValidationRunKey 完全相同且 property 声明 relevant output deterministic，
   可直接复用已有 canonical EvaluationRecord；否则复用 worktree 但重跑 verifier。
6. force rerun 必须重跑，并把新 canonical record 与旧 evidence_refs 中的 ID 比较。
```

`ValidationRunKey` 至少包含：

```text
application_id
property_id
check_plan_digest
verifier_id + verifier_version_or_digest
command argv / runtime primitive id
execution_contract_digest
sandbox_backend_id + sandbox_profile_digest
relevant_output_spec_digest
```

平台后端分级：

- Linux 首选 `bubblewrap`/user+mount+pid+ipc+network namespaces：read-only bind clean target root，tmpfs/overlay/fuse-overlayfs 提供 per-run writable layer，默认禁网；Landlock/seccomp 可作为后续收紧。
- macOS v1 首选 APFS clone/copy-on-write 派生 run tree，scrub env/TMPDIR；`sandbox-exec`/Seatbelt profile 可作为 best-effort 文件/网络限制，但不要把它承诺成与 Linux namespace 等价的稳定安全边界。
- POSIX fallback 只提供 process-wrapper/symlink-or-copy tree 隔离，不能声称是 security boundary。
- strict future backend 可用 VM/container image，把 toolchain、网络、时钟/RNG 策略纳入 execution contract。

verifier 跑过程**不读 cwd**。Evidence 的输入由 `Application`、property/check、execution contract 和 canonical result 决定；这条让 cwd dirty 状态不影响 evidence 计算，也让本地重建 evidence 成为内容寻址比较。

#### Partial reproducibility and effect indexing

Graft v1 的可重现性是 **observational reproducibility**：只要求 property 声明的
relevant observation 可复现，不要求整个 verifier 进程的所有副作用、日志、耗时和临时文件
bit-for-bit 相同。

```rust
struct ExecutionContract {
    env: BTreeMap<String, String>,          // allowlisted env only
    cwd: SandboxCwdPolicy,                 // always sandbox cwd, never user cwd
    network: NetworkPolicy,                // default Deny
    filesystem: FilesystemPolicy,          // writable paths default only run dir/tmp
    toolchain: Vec<ToolRequirement>,       // name/version/digest when known
    caches: Vec<CachePolicy>,              // none | read-only | declared writable cache
    time: TimePolicy,                      // wall clock unspecified in v1 unless declared
    randomness: RandomPolicy,              // unspecified in v1 unless declared
}

struct RelevantOutputSpec {
    exit_code: bool,
    stdout: OutputSelector,                // none | full | lines/globs/regex captures
    stderr: OutputSelector,
    declared_files: Vec<PathPattern>,      // outputs whose digest matters
    normalize_paths: bool,                 // strip sandbox absolute paths
    normalize_line_endings: bool,
}
```

Effect records are indexed observations, not permissions by themselves:

```rust
enum EffectRecord {
    FsRead { class: FsReadClass, digest: Option<BlobId> },
    FsWrite { path_class: PathClass, digest: Option<BlobId> },
    ProcessExec { argv_digest: Digest, exit_code: i32 },
    Network { policy: NetworkPolicy, observed: NetworkObservation },
    TimeRead { policy: TimePolicy },
    RandomRead { policy: RandomPolicy },
}
```

默认 effect policy：

- verifier 可读 clean target root、declared toolchain/system read-only paths；不得读 user cwd。
- verifier 可写 disposable run view 和 run tmp；不得写 clean target root、workspace store、cwd、external target。
- network 默认 deny；需要网络的 verifier 必须在 `ExecutionContract.network` 声明，且其 relevant output 必须足以解释不可复现性。
- time/RNG 在 v1 不强行虚拟化；读了它们的 verifier 只能声明 observational/host-specific，不能声称 strict deterministic。
- declared writable caches 可用于性能，但 cache policy digest 必须进入 `ValidationRunKey`；cache 内容本身不能成为隐式 evidence 输入。

`RelevantOutputRecord` 只保存 property 关心的规范化摘要：exit code、selected stdout/stderr digest、declared output file digests、diagnostics。raw logs、绝对 sandbox path、duration、timestamp 属于 local debug metadata，默认不进 EvidenceId。

Promotion 与 verifier effects 分离：validation evidence 只能证明 `Property(Application)`；`graft patch promote --yes` 是唯一外部 side-effect boundary，并产生单独的 `PromotionRecord`。Promotion 的 effect record 描述写了哪个 target/ref/file，但不回填或改写 EvidenceId。

#### 存储

evidence body 落在 `store/derived/evidence/`：

- 它是可重建数据。`rm -rf store/derived/evidence/` 安全，下次需要时按 `evidence_refs` 中的 ID 重跑得到。
- 不参与 sync。远端不传输 evidence body。

#### Evidence 引用：evidence_refs

Owner（candidate / patch）通过外挂 append-only 索引引用 evidence。schema 完全统一：

```json
// store/{public|private}/evidence_refs/<owner-digest>.json
{
  "owner":      "patch:91sx8q2h",          // 或 candidate:...
  "evidence":   ["evidence:abc", "evidence:def"],
  "updated_at": "2026-06-01T08:30:00Z"
}
```

落点由 owner 类型决定：

- owner = candidate → `store/private/evidence_refs/`（local-only）
- owner = patch     → `store/public/evidence_refs/`（synced）

`evidence_refs` 是 append-only：admit 复制、post-admit `graft patch validate patch:...` 追加。owner body 永久不可变。

#### Sync 模式

evidence sync 的核心设计：**body 不 sync，refs sync**。

```
sync over the wire:
  store/public/evidence_refs/         ✓
  store/derived/evidence/             ✗（不传输 evidence body）

local rebuild:
  fresh clone 拿到 patch + evidence_refs，但 evidence body 缺失
  graft patch show patch:X 看到 "cargo_tests_pass ✓ (not yet locally verified)"
  graft patch validate patch:X --expect cargo_tests_pass
    -> 在 derived worktree 中重跑 verifier
    -> 算出 evidence:E
    -> 检查 E ∈ evidence_refs[patch:X].evidence
       是 → "复现成功"，evidence body 写入 store/derived/evidence/E.json
       否 → "本地重建结果与远端 evidence_refs 中的 ID 不一致"，evidence:E' 是新增条目
            可被 append 到 evidence_refs[patch:X]，由用户决定是否信任本地结果
```

`graft verify-pending` 把所有 "evidence_refs 中存在但本地 store/derived/evidence/ 缺失"的 evidence 一次性重跑。

#### Admission 算法

```text
admit(candidate:C, required: Constraint):
  constraint = required ⊓ C.constraint

  satisfy(constraint) := match constraint:
    Top        -> ok
    Bottom     -> fail [E_CONSTRAINT_UNMET]
    Primitive{id}   -> ∃ evidence:E ∈ evidence_refs[C]:
                    E.application == C.application
                    AND E.property == id
                    AND E.result == passed
    Both{x,y}  -> satisfy(x) AND satisfy(y)
    Either{x,y}-> satisfy(x) OR  satisfy(y)

  if satisfy(constraint):
    move candidate body to patch body (re-hashed; new PatchId)
    move evidence_refs[C] to evidence_refs[P]  (rename owner field, recompute filename)
    delete candidate (no leftover)
```

admit 不复制 evidence body——只复制 evidence ID 列表。一份 evidence 同时被 candidate 和 patch 引用是常态。

注意 admission 查询 `E.result == passed` 是对 evidence body 的查询；本地需要拿到 evidence body 才能算。如果 evidence body 不在 `store/derived/evidence/`（refs 中有 ID 但本地未 rebuild），admit fail loud，提示 `graft patch validate <C> --expect <property>`。
