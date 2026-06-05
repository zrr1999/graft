# 可验证补丁系统设计文档

本文档是 Graft 的唯一核心设计文档。README 负责快速上手；本文档负责说明模型、边界和权衡。

> 本版是模型重写版（移除 main 视图、移除 conflict / artifact、引入 store 三层结构、
> sync 协议化、graft.lock 双锚等）。和当前实现存在差异；实现迁移按本文为准。

---

## 1. Why Graft

### 1.1 问题

Graft 解决的不是「怎么提交 commit」，而是更早一层的问题：

```text
一组人类/智能体产生的文件修改，什么时候可以被认为是可信变更？
```

Git 假设变更的可信度由协作流程（review、CI、PR）在外部保证；Graft 把这件事内嵌成一等概念。

### 1.2 形式

```text
State + Action + ApplicabilityProof
  -> Application
  -> property obligations
  -> runtime-generated Evidence
  -> admitted Patch
  -> target Promotion
```

核心判断：

```text
Evidence ⊢ Property(Application)
Patch = admitted, proof-carrying Application
```

一个 patch 不因为「agent 说完成了」而可信，而因为它是带有 runtime-generated evidence 的 admitted application。

### 1.3 与 Git 的关系

Graft 不替代 Git，也不把 cwd 里的 Git 仓库当成 workspace 本体。

- workspace 是 `$GRAFT_HOME` 或显式 `graft workspace init` 管理的用户级对象；cwd 只是命令路由的 attach key。
- cwd 是否是 Git 仓库与 Graft workspace 概念正交；Graft 默认不写 cwd。
- 远端 Git 仓库是 Graft 的存储分区，没有"main 视图"，托管平台浏览体验由 Graft 自己另外提供（不在本版 scope）。
- 显式 `graft patch promote` 可把某个 patch 投影到任意 target：远端 Git ref、本地 Git ref，或本地非提交文件。这是唯一会把可信 patch 输出到外部世界的路径。

一句话：

```text
Graft 管「变更为什么可信」，并把可信变更保存在 Graft workspace 中。
Git 在外部世界依然是发布渠道和可选 target，但 Graft 自身不依赖 Git 表达 patch graph。
```

### 1.4 非目标

本版不做：

- 替代 Git 的协作流程或托管平台。
- 完整 patch theory（pijul / darcs 风格的 conflict-as-first-class、commute、reorder）。
- 一等的 conflict 对象（compose / migrate / revert 不可解时直接 fail）。
- 一等的 artifact 对象（v1 只保存 runtime 生成的 evidence 与 declared relevant output 摘要；完整 artifact 归后续设计）。
- 持久化或跨 host 的 verifier 输出归档。
- 中央 review gate（admission = 本地认可，review 在 patch 层分布式发生）。
- main / HEAD 等"默认视图"概念。
- 任何形式的 host-bound state（PatchId / EvidenceId 不携带 hostname / timestamp）。

---

## 2. Object model

### 2.1 Three storage tiers

Graft 把所有持久数据按"内容寻址性 + sync 性 + 可重建性"划分为三层：

```text
store/public/    immutable, content-addressed, syncable when workspace [sync] enabled
store/private/   immutable, content-addressed, never synced
store/derived/   rebuildable, content-addressed, locally cached
```


| 层       | 例子                                                                                                             | sync？                     | 丢了能恢复？              |
| ------- | -------------------------------------------------------------------------------------------------------------- | ------------------------- | ------------------- |
| public  | tree, action, application, change, property, patch, evidence_refs(patch), relation, promotion, manifest, blob | 可选（由 workspace [sync] 决定） | 若已 sync 可从 remote 拉 |
| private | candidate, evidence_refs(candidate)                                                                            | 否                         | 不能（local 决定）        |
| derived | evidence, verifier clean worktree cache                                                                        | 否                         | 重跑 verifier / 重物化   |


这一划分是 layout 原则，也是 sync / gc / clone 各自规则的依据。`store/derived/` 可整目录 `rm -rf`，下次需要时按 `evidence_refs` 中记录的 ID 重建。

mutable durable state（alias、remote 同步进度）不在 `store/`，在 workspace `state/`；cwd route / repo local paths 在 `$GRAFT_HOME/registry.toml`，详见 §3.2 和 §12。

### 2.2 Identity scheme

所有 typed object 用统一形式：

```text
<kind>:<digest>
```

`digest` 默认 12 字符 blake3 hex。冲突时按需增长到 16 / 20 / full。CLI 接受 ≥ 6 字符前缀；歧义时报错并列出全部匹配。


| Object                      | ID 形式                  | 文件位置                                             |
| --------------------------- | ---------------------- | ------------------------------------------------ |
| blob                        | `<blake3-hex>`         | `store/public/blob/<blake3-hex>`                 |
| tree                        | `tree:<digest>`        | `store/public/tree/<digest>.json`                |
| action                      | `action:<digest>`      | `store/public/action/<digest>.json`              |
| application                 | `application:<digest>` | `store/public/application/<digest>.json`         |
| change                      | `change:<digest>`      | `store/public/change/<digest>.json`              |
| property                    | `property:<digest>`    | `store/public/property/<digest>.json`            |
| patch                       | `patch:<digest>`       | `store/public/patch/<digest>.json`               |
| evidence_refs (patch owner) | by `<patch-digest>`    | `store/public/evidence_refs/<patch-digest>.json` |
| relation                    | `relation:<digest>`    | `store/public/relation/<digest>.json`            |
| promotion                   | `promotion:<digest>`   | `store/public/promotion/<digest>.json`           |
| manifest                    | `manifest:<digest>`    | `store/public/manifest/<digest>.json`            |
| candidate                   | `candidate:<digest>`   | `store/private/candidate/<digest>.json`          |
| evidence_refs (cand. owner) | by `<cand-digest>`     | `store/private/evidence_refs/<cand-digest>.json` |
| evidence                    | `evidence:<digest>`    | `store/derived/evidence/<digest>.json`           |
| scratch                     | `scratch:<digest>`     | daemon memory only                               |
| view                        | `view:<digest>`        | response only                                    |


注：

- `blob` 是裸 blake3，不带 typed 前缀——它的 hash 就是内容。
- `action` 是 base-polymorphic 的纯语法 AST；`application` 是 (action, base, applicability_proof, target, change) 的具体实化。两者都是内容寻址 immutable public object，body schema 在 §2.4。candidate / patch 的 body 引用 `application:<digest>` 而不是内嵌 base/target/change（详见 §2.7）。
- `evidence_refs` 文件名是 owner 的 digest，没有自己的 ID。它是 owner 上的外挂 append-only 索引，不参与 ID 体系。
- 旧前缀（`gr`_, `grc_`, `ev_`, `ch_`, `gt_`, `cf_`, `rel_`, `prm_`, `scr_`, `fv_`）已废止。遇到这类输入立即以 `[E_LEGACY_ID]` 失败并提示新形式。

### 2.3 Type-theoretic model

从类型论看，Graft 不应把 patch 建模成一个只属于单一 base 的裸箭头
`Patch<A, B>`。更合适的做法是先区分核心状态迁移层和 admission 层：

```text
State                : Type
Action               : Type     # base-polymorphic change intent / pure program AST
Applies(action, s)   : Type     # why an Action applies to one concrete State
Application          : Type     # base-specific realized Action
Property(app)        : Type     # proof obligation over one concrete Application
Evidence             : Type     # runtime-generated witness of Property(app)
Admission(app)       : Type     # local decision that required properties are satisfied
Candidate            : Type     # local private proposed Application
Patch                : Type     # admitted proof-carrying Application
Promotion            : Type     # external side-effect consuming a Patch
```

核心构造是：

```text
apply(action, state, proof : Applies(action, state)) : State

Application(action) =
  Σ (base : State).
  Σ (proof : Applies(action, base)).
  Change(base, apply(action, base, proof))
```

同一件事也可以写成 judgement：

```text
Γ ⊢ s : State
Γ ⊢ a : Action
Γ ⊢ p : Applies(a, s)
────────────────────────────
Γ ⊢ apply(a, s, p) : State
Γ ⊢ app(a, s, p)   : Application
```

这里最关键的是：`Applies(a, s)` 不是 `bool`，而是一个可能有 inhabitant 的类型。
如果能构造 `p : Applies(a, s)`，说明 action 可以应用到这个 state；如果构造不出来，
系统必须返回结构化失败原因。这样“一个 action 能应用的 base 是一个集合”可以写成：

```text
ApplicableBases(action) = Σ (state : State). Applies(action, state)
```

这不是在说一个 admitted patch 有多个 base，而是在说一个可迁移的 `Action` 可能有
多个 `(base, proof)` inhabitant。每个 inhabitant 都会产生一次新的 concrete
`Application`。

也就是说，`Action` 是 base-polymorphic：它表达可迁移的变更意图，但不单独产生
可信对象。`Application` 是 base-specific：它把某个 `action`、某个具体
`base`、以及为什么能应用的 `proof` 一起端点化为 `base -> target`。

当 `Applies(A, S1)` 有 inhabitant `p1` 时，才能构造：

```text
T1 = apply(A, S1, p1)
Application(A, S1, p1) = Change(S1, T1)
```

当同一个 action 也能迁移到 `S2` 时，需要新的 proof：

```text
T2 = apply(A, S2, p2)
Application(A, S2, p2) = Change(S2, T2)
```

`p1` 和 `p2` 不必相同。它们可以来自路径存在性、base blob 匹配、hashline anchor
未漂移、语义迁移成功、冲突已解决等不同证据。迁移失败不是“`Action` 不存在”，
而是当前 `state` 上构造不出 `Applies(action, state)`。

`Patch` 在类型论里不是 `ActionId` 的别名，而是 admission 后的封装：

```text
Admission(app) : Type
Patch =
  Σ (app : Application).
  Σ (admission : Admission(app)).
  EvidenceRefs(app, admission.required_properties)
```

物理存储上，v1 把 `Action` 与 `Application` 都作为一等 immutable public object 持久化；
`Candidate` / `Patch` 只是围绕 `ApplicationId` 的 lifecycle wrapper。patch body 和
`evidence_refs` 可以分开存放；语义上它们共同表达“这个 concrete application 已经被本地
runtime 接纳”。因此 `patch:<id>` 的 subject 必须固定到一个 `application:<id>`，否则
content-addressed ID、evidence 重建和 sync 都会失去稳定含义。

落到对象边界：

- `ActionId` 是共享变更意图，可以跨 base migrate/reapply。
- `ApplicationId` 是一次具体应用，绑定 action、base、applicability proof、target 与 endpoint change。
- `ScratchId` 是可变草稿状态图中的节点，用来构造或试探 `Action` / `Application`。
- `CandidateId` 是本地私有的 `Application` 提议。
- `PatchId` 是已接纳、可同步、proof-carrying 的 concrete `Application`。
- `PromotionId` 记录把某个 `Patch` 投影到外部 target 的副作用结果。

因此迁移语义应是：

```text
migrate(action, new_base)
  -> proof : Applies(action, new_base)
  -> new Application
  -> new candidate/patch instance
  -> rerun evidence for the new Application
```

旧 evidence 不能默认复用到新 Application，因为 `Property(Application)` 的 subject
绑定具体 base/action/proof/target。未来如果出现 action-level property，也应建模成
另一类 subject，而不是把 application-level evidence 混用。

Property / Evidence 的边界：

```text
Property(Application) : Type
Evidence              : runtime-generated inhabitant of Property(Application)
```

`EvidenceRecord` / `EvaluationRecord` 不是 property source 能自行构造的数学证明，
而是 Graft runtime 生成的可重建 proof-carrying data：给定
`(Application, property, verifier, execution_contract)`，本地能在 sandboxed validation
中重跑并得到同一 `EvidenceId`，就等价于重新构造了该 property 的 witness。

#### Why not start from category theory

范畴论里可以把类似结构画成 `span` 或 profunctor，但在实现设计上类型论的
`Applies(action, state)` 更直接：它把“是否适用”从隐含判断变成显式数据/证据，
也更容易映射到 Rust enum、validator、diagnostic 和 smoke test。

`span` 在这里不是文本里的 character span，而是范畴论里表达关系的形状：

```text
      ApplicableBases(action)
        /                    \
       v                      v
  base State              target State
```

更标准地说，span 是一个中间对象 `R` 和两条映射 `R -> A`、`R -> B`，
记作 `A <- R -> B`。在这里，`R = ApplicableBases(action)`；`R` 的一个元素可以理解成
`(base, proof)`。左边投影拿到 base，右边投影拿到
`apply(action, base, proof)` 的 target。

span 能说明“同一个 action 关联多组 base/target”，但它没有直接告诉我们
proof 如何存、失败如何分类、diagnostic 如何返回。因此它适合作为解释图，
不适合作为 v1 的 object schema 主表达。

### 2.4 State, action, application, change

本节给出 §2.3 模型在 v1 中的一等持久对象 schema。

#### State

Graft 中 patch 的“状态”指 `StateId`，已收窄为：

```rust
enum StateId {
    GraftTree(TreeId),          // tree:<digest>，Graft 内部 tree
    RepoTree(RepoBaseState),    // repo:<repo_id>@<treeish>#<resolved_tree_oid>，外部 git 锚点
    GitTree(String),            // local git treeish, resolved only at materialization boundary
}
```

`Conflict` 不再是 StateId 的变体（v1 不建模 conflict）。

`TreeEntry = { path, hash, size, mode: FileMode }`，`FileMode = Regular | Executable | Symlink`。

#### Action object

`Action` 是 base-polymorphic program，不是 endpoint diff。它可以从不同 base 构造不同
`Application`，但每一次 application 都必须携带具体 `ApplicabilityProof`。

v1 canonical Action DSL 是内部 AST；CLI、pi-graft、hashline 编辑器或未来文本语法都只是
frontend。不要把某个 UI 输入格式当成 `ActionId` 的来源。

```rust
struct ActionObject {
    id:     ActionId,     // action:<digest>
    action: Action,
}

enum Action {
    CreateFile {
        path: Path,
        content: BlobExpr,
        mode: FileMode,
        if_absent: bool,
    },
    DeleteFile {
        path: Path,
        expect: FilePredicate,
    },
    ReplaceFile {
        path: Path,
        expect: FilePredicate,
        content: BlobExpr,
        mode: Option<FileMode>,
    },
    EditText {
        path: Path,
        expect: TextFilePredicate,
        hunks: Vec<TextHunk>,
    },
    Rename {
        from: Path,
        to: Path,
        expect: FilePredicate,
        if_to_absent: bool,
    },
    Chmod {
        path: Path,
        expect: FilePredicate,
        mode: FileMode,
    },
    Sequence(Vec<Action>),
}
```

`BlobExpr` 当前只需要 literal blob reference / inline bytes lowering；future 可加 generated
content，但必须把 generator digest 与 declared inputs 进 canonical body。`FilePredicate`
用于表达 applicability 条件，例如：

```rust
enum FilePredicate {
    AnyFile,
    Missing,
    BlobEquals(BlobId),
    ModeEquals(FileMode),
    BlobAndMode { blob: BlobId, mode: FileMode },
}
```

`EditText` 不直接存 UI line numbers。它存规范化文本 hunks 和 anchor predicate：

```rust
struct TextHunk {
    anchor: AnchorPredicate,
    delete_text_digest: Option<BlobId>,
    insert_text: BlobExpr,
}

enum AnchorPredicate {
    UniqueContext {
        before_context_digest: Option<BlobId>,
        selected_text_digest: BlobId,
        after_context_digest: Option<BlobId>,
    },
    WholeFile { old_blob: BlobId },
}
```

`ActionId = hash(canonical(Action))`。canonicalization 只做语法级规范化，不试图证明
semantic equivalence：

- path 是 UTF-8 relative path，`/` 分隔，禁止 absolute、空 segment、`.`、`..`。
- object key order、enum tags、整数、bytes/base64 表达固定；不保留注释或用户 alias。
- blob 内容先内容寻址，Action 里引用 `BlobId`。
- `Sequence` flatten nested sequence；空 sequence 保留为 `Sequence([])`，是否拒绝 empty change 是 property/admission policy，不是 Action parser 的默认 gate。
- 不跨路径排序 sequence。两个 action 是否 commute 是更高层 relation/migration 问题，不参与 `ActionId` equality。
- 对同一 path 的相邻 edit 可由 frontend 预合并，但 canonicalizer 不做依赖 base 的 rewrite。

#### Applicability proof and Application object

`ApplicabilityProof` 是 `Applies(action, base)` 的可序列化 witness。它不单独获得 typed object id，
但 canonical proof digest 进入 `ApplicationId`。proof 必须足以让 daemon 对同一
`(action, base_state)` 重放 applicability 检查并得到同一 target；不能只存 “ok: true”。

```rust
struct ApplicabilityProof {
    action:     ActionId,
    base_state: StateId,
    steps:      Vec<ApplicabilityStep>,
}

enum ApplicabilityStep {
    CreateFile { path, observed_missing: bool },
    DeleteFile { path, matched: FileSnapshot },
    ReplaceFile { path, matched: FileSnapshot },
    EditText { path, matched: TextFileSnapshot, anchors: Vec<AnchorMatch> },
    Rename { from, to, matched: FileSnapshot, observed_to_missing: bool },
    Chmod { path, matched: FileSnapshot },
    Sequence { children: Vec<ApplicabilityProof> },
}
```

以上 enum 是语义 shape；实现可以拆成更细的 canonical record，但必须保持：成功 proof 是一等数据，
失败是结构化 diagnostic。Applicability 失败必须返回结构化原因：

```text
AppliesFailure =
  | MissingPath(path)
  | PathAlreadyExists(path)
  | BlobMismatch(path, expected, actual)
  | ModeMismatch(path, expected, actual)
  | NonUtf8Text(path)
  | AnchorNotFound(path, anchor)
  | AnchorAmbiguous(path, anchor)
  | AnchorDrift(path, anchor)
  | SequenceConflict(index, reason)
  | SemanticConflict(reason)
```

`Application` 绑定一次具体应用，而不是共享变更意图：

```rust
struct Application {
    id:                  ApplicationId, // application:<digest>
    action:              ActionId,
    base_state:          StateId,
    applicability_proof: ApplicabilityProof,
    target_state:        StateId,
    change:              ChangeId,      // endpoint view of this application
    lowering_version:    u32,
}
```

```text
ApplicationId = hash(canonical({
  action_id,
  base_state,
  applicability_proof_digest,
  target_state,
  change_id,
  lowering_version,
}))
```

`Application` 的 core integrity 要求：

```text
apply(action, base_state, applicability_proof) == target_state
replay(base_state, change.ops) == target_state
change.base_state == base_state
change.target_state == target_state
```

任何一条失败都是 `[E_CHANGE_INTEGRITY]`；这种对象在语义上是 ill-typed，不能被 admit、
validate 或 promote。

#### Change object

`Change` 是 `Application.endpoint_diff()` 的 canonical endpoint view。它可以用于
materialize / promote / validate worktree materialization、diff 展示和 v1 relation transform，
但不是 Action 的完整语义。

```rust
struct Change {
    base_state:   StateId,
    target_state: StateId,
    ops:          Vec<ChangeOp>,    // canonical ordered
}

enum ChangeOp {
    CreateFile  { path, blob, mode },
    DeleteFile  { path, blob, mode },
    ReplaceFile { path, before, after, mode_before, mode_after },
    Rename      { from, to, blob, mode },
    Chmod       { path, blob, mode_before, mode_after },
}
```

ops 的规范顺序：按 `(op_kind_tag, primary_path)` 字典序。`Rename` 的 primary_path 是 `from`。

`ChangeId = blake3(canonical(Change))`。

关系：

```text
ActionId        stable across bases; may migrate/reapply
ApplicationId   one action + one base + one proof + one target
ChangeId        endpoint view of Application; replay(base, ops) == target
CandidateId     local/private wrapper around Application + expected properties/provenance
PatchId         admitted/public wrapper around Application + admission metadata
```

`ChangeSet { files: Vec<FileChange> }`（endpoint diff）退化为 `Change.endpoint_diff()` 的 derived view，
仅用于展示。relation commands 可用 Change endpoint algebra 作为 v1 实现手段，但输出仍必须是新的
`action:<id>` + `application:<id>` + `candidate:<id>`，不能只产出裸 `change:<id>`。

#### Hashline lowering

`LINE#HASH` / hashline 是 UI anchor protocol，不是 Action representation。

Lowering pipeline：

```text
scratch read --mode hashlines
  -> user/tool submits edit anchors: (path, LINE#HASH range, replacement)
  -> daemon verifies anchors against current scratch/base file view
  -> construct normalized AnchorPredicate + TextHunk
  -> write action:<digest>
  -> apply(action, base, proof) yields application:<digest>
  -> endpoint change:<digest> is derived from base/target for storage and display
```

规则：

- `LINE#HASH` 的 line number 只用于定位当前 file view；不进入 `ActionId`。
- line hash / selected text / context lowering 成 `AnchorPredicate`；如果 anchor 不唯一或已漂移，返回 `AnchorNotFound` / `AnchorAmbiguous` / `AnchorDrift`。
- `scratch:<digest>` 是 daemon scratch graph 节点，不是 `ActionId`。scratch op chain 可以用于构造 Action，但最终 candidate 必须绑定 concrete `Application`。
- rename 在 hashline/scratch UI 中仍可用 delete+write 表达；若 frontend 能证明同 blob move，可 lower 成 `Action::Rename`，否则保留为 delete/create sequence。

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

#### Admission expression

`PropertyExpr` is admission/promotion expression state, not property source:

```rust
enum PropertyExpr {
    True,
    Atom { id: PropertyId, name: PropertyName },
    And  { terms: Vec<PropertyExpr> },
}
```

`PropertyExpr::Atom` must carry the resolved `PropertyId`; storing only a name
would make a candidate's required policy drift when `properties.roto` changes.

`PropertyExpr` appears in candidate and patch records:

- `candidate.expected`
- `patch.properties`

Properties are evaluated over one whole application state. In a multi-repo
workspace, repos are directories inside that state, normally
`worktrees/<repo-id>/`; they are not separate property namespaces. A property
can compare repos by reading or running commands against paths in `.base` and
`.target`, for example `worktrees/A/...` and `worktrees/B/...`.

`graft.toml` binds admission or promotion policy to property bodies by name:

```rust
PropertyRef {
    id:       PropertyId,
    name:     PropertyName,
}
```

The display and CLI format is the property name, for example
`graft_config_current`, `c_cargo_tests_pass`, or `ab_task_output_same`.
`properties.roto` defines the property body and can inspect the entire state.
`graft.toml` only chooses which properties are required:

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
over the same application.

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
admit(candidate:C, required: Vec<PropertyExpr>):
  for expr in required:
    satisfy(expr) := match expr:
      True     -> ok
      Atom{id} -> ∃ evidence:E ∈ evidence_refs[C]:
                    E.application == C.application
                    AND E.property == id
                    AND E.result == passed
      And{ts}  -> ∀ t ∈ ts: satisfy(t)

  if all satisfy:
    move candidate body to patch body (re-hashed; new PatchId)
    move evidence_refs[C] to evidence_refs[P]  (rename owner field, recompute filename)
    delete candidate (no leftover)
```

admit 不复制 evidence body——只复制 evidence ID 列表。一份 evidence 同时被 candidate 和 patch 引用是常态。

注意 admission 查询 `E.result == passed` 是对 evidence body 的查询；本地需要拿到 evidence body 才能算。如果 evidence body 不在 `store/derived/evidence/`（refs 中有 ID 但本地未 rebuild），admit fail loud，提示 `graft patch validate <C> --expect <property>`。

### 2.7 Candidate, patch, admit

```rust
struct Candidate {
    id:          CandidateId,       // candidate:<digest>
    application: ApplicationId,     // concrete application:<digest>
    expected:    Vec<PropertyExpr>, // declared obligations for this application
    provenance:  Provenance,
}
// CandidateId seed = body fields；evidence_refs / local created_at 不在 body

struct Patch {
    id:          PatchId,           // patch:<digest>
    application: ApplicationId,     // concrete application:<digest>
    properties:  Vec<PropertyExpr>, // satisfied declared obligations
    provenance:  Provenance,
    admission:   AdmissionSummary,  // no host/time fields
}
// PatchId seed = body fields；evidence_refs / local admitted_at 不在 body
```

`base_state`、`target_state` 与 `change` 不再重复内嵌在 candidate / patch body；它们从
`store/public/application/<digest>.json` 读取。这样 `candidate:<id>` / `patch:<id>`
只是 lifecycle wrapper，subject 始终是同一个 immutable `application:<id>`。

#### Candidate = local-only

Candidate 是私有提议，**不 sync**。`store/private/candidate/` 是它的家。其他 clone 看不到、不能 review、不能继承。

review 在 patch 层分布式发生：admit 之后 patch sync 出去，每个 clone 自己决定是否在自己的工作流中接受这个 patch。本地"不接受"的表达是不让 alias 指它、不 materialize、不在 incoming 默认列表展示（v1 全列；按需后续加 hide）。

#### admit = mv

admit 的物理含义是把对象**从 private 跨子目录搬到 public**：

```
store/private/candidate/<C-digest>.json
   → store/public/patch/<P-digest>.json     # body 变化（schema 不同），ID 重算

store/private/evidence_refs/<C-digest>.json
   → store/public/evidence_refs/<P-digest>.json  # 文件名 = 新 PatchId
                                                 # body 中 owner 字段更新
```

操作完成后 candidate **在文件系统上消失**——它不是历史记录。要追溯 patch 来自哪个 candidate，靠 `patch.provenance` 字段（包含原 candidate ID 等元数据）。

#### admit failure modes

```text
[E_ADMISSION_UNMET]
  required PropertyExpr 的某 atom 找不到 passed evidence。
  原因可能是：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
            或者本地从未跑过该 verifier。
  提示：graft patch validate candidate:C --expect <property>

[E_PROPERTY_DRIFT]
  required atom 的 PropertyId 与 candidate.expected 中对应 atom 不一致。
  原因是 properties.roto 中对应顶层 property 函数在 candidate 创建后被修改或改名。
  解决：要么用现行 PropertyId 跑新 evidence，要么 revert properties.roto 改动。
```

### 2.8 Relation, promotion

#### Relation

`relation` 表达对象之间的已知关系：

```rust
struct Relation {
    id:   RelationId,                 // relation:<digest>
    kind: RelationKind,               // Compose | Migrate | Revert | Materialize
    inputs:  Vec<ObjectRef>,
    outputs: Vec<ObjectRef>,
    provenance: Provenance,
}
```

例：`compose(patch:a, patch:b) -> candidate:c?` 先把 relation intent 写进 candidate provenance；当该 candidate admit 成 `patch:c` 时，daemon 写一条 public `Relation { kind: Compose, inputs: [a, b], outputs: [c] }`。

Relation 是 admitted object 的 derivability 历史事实记录，不参与 admission 本身。`graft patch show patch:c` 会展开 relation：`patch:c is compose(patch:a, patch:b)`。Relation 不指向 private candidate，以免 sync 出本地私有对象引用。

#### Promotion

Promotion 是把 patch 投影到显式 target 的事件记录。当前实现的 target 是配置的外部 Git repo/ref（`[promote_targets.<name>]`）或命令行 `--to <branch>` 指定的本地 Git ref：

```rust
struct PromotionRecord {
    id:           PromotionId,        // promotion:<digest>
    patch_id:     PatchId,
    target:       String,             // target:<name>:<ref> 或 branch/release/pr label
    dry_run:      bool,
    status:       String,
    effects:      Vec<PromotionEffect>, // external writes actually attempted/observed
    promoted_at:  String,             // local metadata; not evidence
}
```

`graft patch promote patch:X --to <target-or-branch>` 触发：

1. 若 `<target-or-branch>` 命中 `graft.toml [promote_targets.<target>]`，解析其 `path`、`branch` 与 `required_properties`；否则按显式分支/PR/release 目标处理。
2. 对 `[promotion].required_properties`、配置 target 的 `required_properties` 与 CLI `--require <property>` 重新跑 admission 查询。
3. 从 `patch.application.target_state` 构造目标 commit/ref；只有 `--yes` 会真正写外部 Git repo/ref，默认是 dry-run。
4. apply 时在 `store/public/promotion/<digest>.json` 落一条记录。
5. 当前 workspace 若启用 sync，下次 sync 把这条 promotion record 推到 Graft remote。

Promotion 是显式 side-effect 边界：除了 `graft patch promote --yes`，Graft 命令不把 patch 输出到外部 target。Promotion effects 可记录 `GitRefUpdate`、`LocalFileWrite`、`RemotePush` 等外部写入事实；它们证明「patch 被投影到哪里」，不证明 `Property(Application)`，也不改变任何 EvidenceId。

`[promote_targets.<name>]` 在 `graft.toml` 配置，`required_properties` 在该处声明（详见 §11 和 §12）。

---

## 3. Workspace layout

### 3.1 cwd

```text
cwd/
  graft.toml              # 项目定义，进 snapshot
  graft.lock              # Git-managed 派生锚（properties + repos），不进 Graft snapshot
  properties.roto          # property source，进 snapshot
  src/                    # workspace files，进 snapshot
  worktrees/              # state content: managed repo directories, e.g. worktrees/A/
  README.md
  ...
```

约束：

- 这里的 `cwd/` 只描述 `graft workspace init` 创建的 **local workspace root**。一般命令的 cwd 可以是任意目录；它通过 §12 的 lookup / routes 解析到 workspace。
- cwd 根目录允许存在 `.git/`。cwd 是否是 Git 仓库只影响 attach 时能否自动登记 repo，不影响 workspace 是否存在。
- snapshot 包含什么：
  - 包含：`graft.toml`、`properties.roto`、所有普通工作区文件，以及 state 内的 `worktrees/<repo-id>/` repo 内容。
  - 不包含：`graft.lock`（派生锚，不作为 candidate 内容；但在 Git workspace 中必须由 Git 跟踪）、`.graft/`（本地状态）、`.worktrees/`（临时 materialize 输出）、`.git/`（外部 VCS）、`.gitignore` 类工具忽略的常见生成物。
  - 排除规则的具体语法在 §3.4。

local workspace root 是工作区，不是默认视图。Graft 不维护"当前 cwd 是 selected snapshot 的物化"这种隐式不变量；显式 materialize 总是写隔离 worktree，详见 §5 与 §12。

### 3.2 .graft/

```text
.graft/
  config.toml                       # local: [remotes.*], daemon options

  store/
    public/                         # immutable, sync
      blob/        <blake3>
      tree/        <digest>.json
      action/      <digest>.json
      application/ <digest>.json
      change/      <digest>.json
      property/    <digest>.json
      patch/       <digest>.json
      evidence_refs/  <patch-digest>.json     # append-only index, sync
      relation/    <digest>.json
      promotion/   <digest>.json
      manifest/    <digest>.json

    private/                        # immutable, never sync
      candidate/      <digest>.json
      evidence_refs/  <candidate-digest>.json # append-only index, local

    derived/                        # rebuildable, local
      evidence/    <digest>.json    # rebuilt locally
      worktrees/   <key>/root/      # clean target cache for verifier/run materialization

  state/                            # mutable durable, atomic write
    aliases/
      candidates/<name>             # 单文件: candidate:<digest>
      patches/<name>                # 单文件: patch:<digest>
      promotions/<name>             # 单文件: promotion:<digest>
    remotes/<remote>/
      last_synced                   # manifest:<digest>
      transport.cache/              # bare git odb (transport-only)

.worktrees/
  <state-slug>/                      # explicit materialize inspection output
```

全局 daemon 的 process state 不在 workspace 内，而在 `$GRAFT_HOME/run/`（§12）。

四个 `.graft/` 顶级目录 + workspace-level `.worktrees/`，每个角色单一：


| 顶级               | 内容性质                                      | sync | 启动清理 |
| ---------------- | ----------------------------------------- | ---- | ---- |
| `config.toml`    | 用户可改本地配置                                  | 否    | 否    |
| `store/public/`  | 内容寻址不可变，按 workspace sync policy 决定是否 sync | 可选   | 否    |
| `store/private/` | 内容寻址不可变，local-only                        | 否    | 否    |
| `store/derived/` | 可重建本地数据                                   | 否    | 可选   |
| `state/`         | 可变指针，atomic write                         | 否    | 否    |
| `.worktrees/`    | explicit materialize 输出，临时检查目录；不建议编辑 | 否    | 可清理  |


#### 写规则

- `store/public/` `store/private/`：daemon 写一次后内容永不修改（content-addressed）。JSON body / index 通过 temp file + atomic rename 发布；删除只通过 gc。
- `store/derived/`：daemon 重建时写入；用户可整目录 `rm -rf` 而不破坏正确性。
- `state/`：atomic rename 写入。每个文件短小，单次 read/write 即一致快照。
- `evidence_refs/` 是 append-only：daemon 通过 read → append → atomic rename 实现。同一 owner 的 refs 文件不并发写（daemon 串行化）。
- `.worktrees/`：user-facing `graft patch materialize` 输出目录；daemon 可按 gc/doctor 策略清理过期目录。这里是检查输出，不是 patch/state 的源，不建议用户编辑。
- `.graft/store/derived/worktrees/`：verifier / `graft run` 的 clean target cache，属于内部可重建缓存；它和 workspace-level `.worktrees/` 名字相近但语义不同。
- `$GRAFT_HOME/run/`：全局 daemon 启动时清理 `validation/` `tmp/` 等 ephemeral，重建 `daemon.sock` `daemon.pid`。

#### sync 范围

```
sync = if workspace [sync] is enabled:
         mirror store/public/ to remote refs/graft/{facts, blobs, manifests}
       else:
         no-op for distribution; admission still writes store/public/ locally
       (详细映射见 §6 和 §12)
```

`store/private/` `store/derived/` `state/` `.worktrees/` `$GRAFT_HOME/run/` 永不 sync。`ws:default` 强制永不 sync。

#### Alias locality

`state/aliases/*` 是 workspace-local mutable bindings：

```text
state/aliases/patches/release-candidate -> patch:abc123
```

它们不进入 manifest，不写 remote refs，不与其他 clone merge。两个 clone 可以把同一个 alias 名指向不同 patch，这不是 sync conflict。远端 patch fetch 到本地后，用户可以显式设置本地 alias 指向它；不设置 alias 就只是 store 中多了一个可查对象。

Property names 是另一回事：`properties.roto` 顶层 property 函数是 patchable workspace source，改变函数名、`checks` 或 `requires` 会产生新的 `PropertyId`；evidence/admission 仍按 `PropertyId` 比较。

#### gc 范围

reachability roots：

```
state/aliases/{candidates,patches,promotions}/*  解析得到的对象 ID
当前 properties.roto 顶层 property 函数解析得到的 PropertyId 集合
当前 graft.toml [admission].required_properties / [promotion].required_properties / [promote_targets.*].required_properties 解析到的 PropertyId
daemon 内存中持有 lease 的 active scratch
```

从 roots walk，标记可达对象。`store/{public,private}/` 中不可达的对象在 gc 时删除（`store/derived/` 整目录可清，按需重建）。详见 §9。

### 3.3 graft.toml / graft.lock 双锚

`graft.toml` 是 workspace 元配置；`graft.lock` 是派生缓存兼解析锚。二者都属于 workspace 的受管文件：任何变更都通过 patch admit，并且必须在同一个 patch 中原子同步。

`graft.lock` 在 Git workspace 中必须入 git、可跨 clone，**不包含本地路径**。本地路径归 `$GRAFT_HOME/registry.toml [[repo_paths]]`（§12）。

#### 形态

```toml
# graft.toml
schema = 1

[admission]
required_properties = []

[promotion]
required_properties = []

[repos.linux-stable]
url = "https://git.kernel.org/.../linux-stable.git"
default_branch = "linux-6.6.y"  # 可选；graft repo add 默认写 remote HEAD

[repos.cpython]
url = "https://github.com/python/cpython"

[promote_targets.gh-main]
path = "../external-git-repo"
branch = "main"
required_properties = ["cargo_tests_pass"]
```

```toml
# graft.lock, @generated by graft; do not edit by hand
version = 1
locked_at = "2026-06-01T08:30:00Z"

# Properties: property function name -> PropertyId
[properties.empty_change]
id         = "property:374d33205102"
check_hash = "..."

[properties.cargo_tests_pass]
id         = "property:044b52a36644"
check_hash = "..."

# Repos: repo id -> resolved tree
[repos.linux-stable]
url          = "https://git.kernel.org/.../linux-stable.git"
treeish      = "linux-6.6.y"
resolved_oid = "tree-oid:..."
resolved_at  = "2026-06-01T08:30:00Z"

[repos.cpython]
url          = "https://github.com/python/cpython"
treeish      = "HEAD"
resolved_oid = "tree-oid:..."
resolved_at  = "2026-06-01T08:30:00Z"
```

#### Properties 节

来源：`properties.roto`。每次 graft 命令启动时 daemon 解析并 typecheck：

```text
properties.roto top-level function foo(app: Application) -> Property
  -> canonical PropertyPlan
  -> PropertyId + check_hash
  -> graft.lock [properties.foo]
```

如果 `graft.lock` 中 `[properties.foo].check_hash` 与当前算出不一致 → drift detected。修复方式不是旁路写 lock，而是构造一个元配置 patch，把 `properties.roto` 与 `graft.lock` 的新解析结果一起 admit。

命名与边界：

- Property name 是顶层函数名的原样拼写，例如 `empty_change`、`cargo_tests_pass`；不做 PascalCase alias 转换。
- Runtime primitive id 使用 `snake_case`，例如 `changed_paths`, `match`, `all_match`, `call`, `exit_code_is`, `same_output`。
- Runtime primitive 是内部 observe/compute/decide 的 building block，不承载 workspace policy name，也不把多个可独立命名的 policy requirement 捆成一个 primitive。
- `apply(action, base, proof) == target` 与 `replay(base, change.ops) == target` 是 Graft core application integrity，不是 property，也不是 runtime primitive；admit/materialize/promote 默认都会检查它。
- 空 change 是普通 property，可由 workspace 在 `properties.roto` 中声明，例如 `fn empty_change(app: Application) -> Property { ... }`；非空要求也应作为 workspace policy 显式声明，而不是默认 gate。

#### Repos 节

来源：`graft.toml [repos.<repo_id>]`。`repo_id` 是 workspace-local 的稳定名字，也是 `.graft/repos/<repo_id>` 的受管 clone 目录名；`graft.lock` 记录同一个 `url` 用于检测 repo 配置漂移，但不另存 canonical URL hash。

```toml
[repos.<repo_id>]
url = "..."                  # 必填
default_branch = "main"      # 可选；repo add 未显式指定时写 remote HEAD
```

解析规则：

- `graft repo add` clone/fetch 到 `.graft/repos/<repo_id>`，写入 `url` 和 `default_branch`，随后立即 lock。
- 已存在的 `.graft/repos/<repo_id>` 必须有精确匹配的 `origin` URL；比较只剥离 Git 输出行尾，不做 whitespace trim 或 lossy Unicode 归一化。如果 config URL 指向另一个 repo，`repo sync/lock/update` 必须失败而不是复用旧 cache。
- `default_branch` 存在：fetch/lookup 当时分支 tip，写入 `url`、`treeish = default_branch` 与 `resolved_oid`。
- 手写配置缺少 `default_branch`：按 `HEAD` lock，写入 `url`、`treeish = "HEAD"` 与 `resolved_oid`。

Application 引用外部 repo 时，`base_state = RepoTree { repo_id: <repo_id>, treeish: <treeish>, resolved_tree_oid: <resolved_oid> }`。**信任语义来自 lock 里的 resolved tree oid，不来自浮动分支名**。
后续按 `RepoTree` materialize snapshot 时仍必须通过当前 repo config 的 `url` ensure 受管 clone：cache 缺失可以重建，cache `origin` 与 config URL 不一致必须失败，不能直接按 `.graft/repos/<repo_id>` 读旧 clone。

##### graft repo add / lock / update

```bash
graft repo add <name> <url>         # clone/fetch 到 .graft/repos/<name>，记录 remote 默认分支，再写 graft.toml 并 lock
graft repo lock                     # 解析所有 repos 到 lock
graft repo lock <name>              # 单独解析一个

graft repo update <name>            # fetch 后强制重新解析 default_branch/HEAD；允许 resolved_oid 漂移
graft repo update --all
```

#### 漂移检测

```text
[E_REPO_LOCK_DRIFT]
  graft.toml [repos.X].url/default_branch 与 graft.lock [repos.X] 不一致，或
  lock 缺少 treeish/resolved_oid。

  解决：
    graft repo update X     # fetch 并刷新 lock
    或 revert graft.toml 改动
```

```text
[E_PROPERTY_LOCK_DRIFT]
  properties.roto 中 X 的 check_hash 与 graft.lock [properties.X].check_hash 不一致。

  解决：构造元配置 patch，刷新 lock。
  注意：refresh 后 PropertyId 可能漂移，旧 evidence 不再满足新 admission。
```

#### 不变量

```
Invariant 3.3.1  (NoDriftingExternalReferences)
  patch.application.base_state 中的 repo treeish/resolved_tree_oid 必须来自 graft.lock
  对应 repo id 的 treeish/resolved_oid（创建 application 时刻的快照）。

  branch / default HEAD 只用于解析 lock，不进入 patch 的信任语义。
```

```
Invariant 3.3.2  (LockSchemaUniformity)
  graft.lock 的所有顶级 entry 都是 (workspace-local name, resolved-id) 映射，
  无运行状态、无 sync 进度、无 cwd 信息、无本地路径。

  cwd 路由：$GRAFT_HOME/registry.toml [[routes]]
  受管 repo clone 路径：workspace .graft/repos/<repo_id>
  attach 发现的外部 checkout 路径：$GRAFT_HOME/registry.toml [[repo_paths]]
  sync 进度：workspace .graft/state/remotes/<name>/last_synced
```

### 3.4 Snapshot 与 ignore 规则

当前主路径不会把 cwd 隐式捕获成 candidate。snapshot ignore 规则只用于显式 snapshot/materialize/verifier 内部路径；scratch 主路径通过 `scratch write/edit/delete [--repo <repo>] --base/--from` 明确指定文件和来源：

```
内置排除（不可关闭）：
  .graft/                         # graft 自身
  .git/                           # 外部 VCS；可存在但不进入 snapshot
  graft.lock                      # 派生锚；不进 Graft snapshot，但仍是 Git-managed meta file
  worktrees/                      # local managed-repo checkout/output area; not implicit source

用户可配（graft.toml [snapshot.ignore]）：
  patterns = ["target/**", "node_modules/**", "*.log"]
```

`[snapshot.ignore]` 模式语法是 gitignore-compatible 子集。

具体匹配规则、symlink 处理、大文件阈值等实现细节在 §10 invariants 列出。

---

## 4. Lifecycle

Graft 把变更生命周期拆成三道关，每一关都有显式动词和门槛：

```text
scratch operation (CLI or pi-graft client)
  -> graft scratch write/edit/delete/read [--repo <repo>] --base <base> | --from <scratch>
scratch                         daemon-backed draft, not synced, not a candidate
  -> graft patch from-scratch <scratch>
candidate                       store/private, local-only
  -> graft patch validate       produces evidence (store/derived)
  -> graft patch admit          gates: core integrity + [admission].required_properties
patch                           store/public, synced only when workspace [sync] enabled
  -> graft patch promote        gates: core integrity + promotion/target/CLI required_properties
target output                   outside Graft's patch graph (git ref / local file)
```

每道关的语义：

- **scratch**：临时草稿读写/编辑/删除，无 candidate/patch 写入、无 cwd 捕获。第一次操作显式给 `--base`，后续操作显式给 `--from`；rename 用 delete+write 表达。CLI 与 pi-graft 是两个 client，但共享同一 daemon wire protocol。
- **candidate-from-scratch**：把 scratch 中的变更落成可寻址 candidate，无外部副作用。空 change 允许存在；若 workspace 要显式标记或拒绝它，应声明 `empty_change` / `non_empty_change` property。
- **admit**：candidate 升 patch，等于「我（本地）愿意把这件事公开给团队」。门槛是 application core integrity（`apply(action, base, proof) == target` 且 `replay(base, change.ops) == target`）加 `[admission].required_properties`。**这不是 review gate**——review 在 sync 之后由每个 clone 自己决定。
- **promote**：patch 投影到下游 target（当前实现为配置的外部 Git repo/ref 或显式 `--to <branch>`），等于「它能 ship 给非 Graft 用户或工具」。门槛 `[promotion].required_properties`、`[promote_targets.<target>].required_properties` 与 `--require`。

admit ≠ review。这点在分布式协作中至关重要：远端 patch 进入本地 store 不代表本地必须采用，alias 是否指向它由本地决定。

### 4.1 scratch + candidate-from-scratch: draft → candidate

```bash
graft scratch write  [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --content <bytes>
graft scratch edit   [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --edits <json>
graft scratch delete [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path>
graft patch from-scratch <scratch-id> [--expect <property>...] [--producer <label>] [--message <msg>]
```

行为：

1. 解析 scratch source：
  - `--base <id>` 开始第一版 scratch。不写 `--repo` 时，bare treeish 在 workspace Git 上下文解析；写 `--repo C` 时，bare treeish 在 `[repos.C]` 上下文解析。`graft:empty`、`tree:...`、`candidate:...`、`patch:...` 是显式 base ref，不需要 repo base context。
  - `--from <scratch-id>` 续写前一版 scratch。scratch 是 daemon-instance-scoped，daemon 重启后可能失效。
  - `base` 与 `from` 互斥且必须提供其一。
2. scratch 命令只改 daemon scratch graph，返回新的 `scratch:<digest>` 与 changed paths；不写 candidate、patch、git ref 或 cwd 文件。
3. `graft patch from-scratch` 读取指定 scratch，把 scratch op chain lower 成 canonical `Action`，构造 `ApplicabilityProof`，应用到 base 得到 target，并派生 endpoint `Change`。
4. 空 change 允许存在；若 workspace 要显式标记或拒绝它，应声明 `empty_change` / `non_empty_change` property。`Action::Sequence([])` 仍按 §2.4 的 canonicalization 规则保存。
5. 写入：
  - `store/public/blob/`（新增内容）
  - `store/public/tree/`（target tree）
  - `store/public/action/`
  - `store/public/application/`
  - `store/public/change/`
  - `store/private/candidate/<C-digest>.json`
  - `store/private/evidence_refs/<C-digest>.json`（空 evidence 列表）
6. 不写 alias 或命名视图；当前 CLI 不提供 `graft alias` surface。

注意 blob/tree/action/application/change 都进 public——它们是内容寻址不可变事实，将来 admit 后这些对象就是 patch 的一部分，提前进 public 不浪费。candidate 自己进 private。admit 不负责捕获 cwd。

### 4.2 graft patch validate: 跑 verifier 产 evidence

```bash
graft patch validate <id> [--expect <property>...]
```

`<id>` 可以是 `candidate:...`、`patch:...`，或 `application:...`。当前主路径要求显式 id；先用 scratch 命令得到 `scratch:<digest>`，再用 `graft patch from-scratch <scratch>` 得到 candidate。`change:...` 只是 endpoint view，不是 property subject；若需要验证，必须通过拥有该 change 的 application / candidate / patch。旧的 `graft validate` 顶层入口是隐藏兼容 alias。

#### 流程

```text
1. 解析 <id> 拿到目标 application，并从 application 读取 action/base/proof/target/change。
2. 解析 candidate/patch 已声明的 expected properties，并把重复给出的 `--expect <property>` 列表追加为当前 PropertyId（通过 `properties.roto` 当前顶层函数名映射）。
3. 对每个 (Application, property)：
   a. 解析 property 的 `requires` 依赖闭包并按拓扑序评估，结果 memoize。
   b. 对需要执行的 property/check，物化输入 tree 到 `.graft/store/derived/worktrees/<tree-id>/`。
   c. 计算 ValidationRunKey（property/check、runtime primitive、execution contract、relevant output spec）。
   d. 若已有 canonical EvaluationRecord 且 reuse policy 允许，直接复用；否则在物化 tree 根目录作为 cwd 执行对应 run。
   e. 默认执行契约：无 timeout、允许网络、允许 host 文件系统访问；validation 不读写用户 cwd，除非 property 命令本身通过 host 文件系统访问它。
   f. 收集并规范化 result / relevant output / declared output digests，构造 canonical EvaluationRecord。
   g. hash 得 evidence:E；检查 store/derived/evidence/E.json：
      存在 -> noop（content-addressed，重复跑得同 ID）。
      不存在 -> 写入 store/derived/evidence/E.json。
   h. append E 到 evidence_refs[<id>].evidence（如果不在）。
4. 保留 derived worktree cache；store/derived 可由 gc 安全清理。
5. 渲染结果；若 force rerun 与已有 evidence_refs 不匹配，报告本地复现差异。
```

#### 显式 id 版本

`graft patch validate <id>` 始终隔离运行，与 cwd 状态无关。cwd 只用于 workspace 路由，不用于推断要验证的 change。详见 §5.2。

#### 后续追加 evidence

```bash
graft patch validate patch:91sx8q2h --expect cargo_fmt_clean
```

patch body 永远不变；evidence_refs 是 append-only。post-admit 追加 evidence 是常态——本地 verify 远端 patch 的复现性就是这个路径（§6.3）。

### 4.3 graft patch admit: candidate → patch

```bash
graft patch admit <candidate-id> [--require <property>...]
```

```text
1. 解析 candidate:C。
2. 检查 application core integrity：`apply(action, base, proof) == target` 且 `replay(base, change.ops) == target`，失败 -> `[E_CHANGE_INTEGRITY]`。
3. required = `[admission].required_properties` ∪ candidate.expected ∪ repeatable `--require <property>` additions。
4. 对 required 中每个 atom，admission 算法（§2.6）查 evidence_refs[C]:
   ∃ E ∈ refs[C].evidence:
     E.application == C.application
     AND E.property == atom.id
     AND E.result == passed
   失败任何一条 -> [E_ADMISSION_UNMET]。
5. 通过：
   构造 Patch body（C 的 application/base/target/change/provenance + properties + admission summary）。
   PatchId = hash(Patch body)，不包含 local admitted_at。
6. mv:
   store/private/candidate/<C-digest>.json -> store/public/patch/<P-digest>.json
   store/private/evidence_refs/<C-digest>.json -> store/public/evidence_refs/<P-digest>.json
     （body 中 owner 字段从 candidate:C 改为 patch:P）
7. 删除 candidate alias（如果有指向 C 的 state/aliases/candidates/<name>）。
8. 不修改 cwd route 或 worktrees（admit 不切视图）。
```

admit 完成后 candidate 在文件系统上消失。要追溯 patch 来自哪个 candidate，看 `patch.provenance` 字段。

admit 保持纯粹：只接受已有 candidate，不捕获 cwd，不 materialize，也不写 promote target。用户需要先通过 scratch 具体命令生成 scratch，再用 `graft patch from-scratch` 进入 candidate lifecycle。

#### Failure modes

```text
[E_ADMISSION_UNMET]
  required PropertyExpr 的某 atom 找不到 passed evidence。
  原因：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
        或本地从未跑过该 verifier。
  提示：graft patch validate <candidate> --expect <property>

[E_PROPERTY_DRIFT]
  required atom 的 PropertyId 与 candidate.expected 中对应 atom 不一致。
  原因：properties.roto 中对应顶层 property 函数在 candidate 创建后被修改或改名。
  解决：要么用现行 PropertyId 跑新 evidence，
        要么 revert properties.roto 改动并刷新 graft.lock。
```

### 4.4 graft patch promote: patch → target projection

```bash
graft patch promote <patch-id> --to <target-or-branch> [--require <property>...]
```

#### 流程

```text
1. 解析 patch:P；检查 application core integrity。
2. 若 --to 命中 [promote_targets.<target>]，再解析该 target 的 path/branch/required_properties。
3. required = `--require <property>` 给出 ∪ `[promotion].required_properties` ∪ `[promote_targets.<target>].required_properties`。
4. 对 required 跑 admission 算法（与 admit 同；查 evidence_refs[P]）。
   失败 -> [E_PROMOTION_UNMET]
5. 构造目标 Git commit/ref；默认 dry-run，仅 --yes 写外部 Git repo/ref。
6. apply 时落 promotion record:
   store/public/promotion/<digest>.json
   { id, patch_id, target, dry_run, status, promoted_at }
7. 不更新 cwd 路由，不 materialize 视图。
```

当前 workspace 若启用 sync，下次 sync 自动把 promotion record 推到 graft origin；`ws:default` 永不 sync。

#### 不变量

```
Invariant 4.4.1  (PromotionIsTheOnlyTargetProjection)
  Graft 命令中只有 graft patch promote 把可信 patch 写到外部 target。
  graft sync 只写 Graft remote（refs/graft/*），不写 refs/heads/* 或 PR head。
  scratch / patch from-scratch / patch admit / patch materialize / patch validate 永远不写任何 git ref 或 local-file target。
```

#### Failure modes

```text
[E_PROMOTION_UNMET]            required_properties 不满足。
[E_PROMOTION_NOT_FF]           remote-push target 不能 fast-forward；用户决定 force / abort。
[E_PROMOTION_DIRTY_TARGET]     local-git-commit target dirty；先清理或提交本地改动。
[E_PROMOTION_TARGET_UNKNOWN]   --to <target> 在 graft.toml 的 [promote_targets] 中找不到，且不能作为普通 branch 处理。
```

### 4.5 关系操作: compose / migrate / revert

```bash
graft patch compose <patch:a> <patch:b>          # 输出新 candidate
graft patch migrate <patch:a> --onto <state>     # 输出新 candidate
graft patch revert <patch:a>                     # 输出新 candidate
```

语义在 op list 上工作（§2.4）。每条命令产出新 candidate，并在 candidate provenance 中记录 pending relation；candidate admit 成 patch 后再写 public relation（§2.8）。

**v1 不建模 conflict**：上述命令在不可解时**直接 fail**，不产出 conflict 对象。错误信息提供具体冲突位置：

```text
[E_COMPOSE_CONFLICT]
  cannot compose patch:a and patch:b:
    src/foo.rs line 42: a writes "X", b writes "Y"
    src/bar.rs: a deletes, b modifies
  to resolve manually:
    1. graft patch materialize <some clean state>
    2. encode the resolved files through scratch write/edit/delete --base/--from
    3. graft patch from-scratch + graft patch validate + graft patch admit
```

user 自己起 candidate 编码 resolution，admit 后产出 patch:c'，并写一条 `Relation { kind: Compose, inputs: [a, b, ...], outputs: [c'] }` 关联三者作为 derivability 历史。

---

## 5. Materialized states and run

cwd 不是 view。cwd 只用于命令路由（§12）。`StateId` 表示完整 workspace snapshot；`application`、`patch`、`candidate`、`repo:<id>@<treeish>` 和 Git treeish 都只是解析到 state 的入口。

### 5.1 Materialize 输出目录

```text
<workspace-root>/.worktrees/<state-slug>/
```

`<workspace-root>` 是 local workspace root 或 system workspace root（例如 `$GRAFT_HOME/workspaces/default`）。`graft patch materialize <state-ref>` 默认永不写当前 cwd。输出目录按 resolved state identity 命名，不按输入 ref 或 patch id 命名。

多 repo 内容是 materialized state root 内的普通目录，例如：

```text
<workspace-root>/.worktrees/<state-slug>/
  graft.toml
  properties.roto
  worktrees/A/
  worktrees/B/
```

`.worktrees/` 是 user-facing 临时检查输出，不是 state source，不建议编辑；Graft store 中真实状态事实仍在 `.graft/`。
不要把这里和 `.graft/store/derived/worktrees/` 混淆：后者是 verifier / `graft run` 的内部 clean target cache，可随 `store/derived/` 一起重建。

### 5.2 Dirty 状态

cwd dirty 不再是 `graft patch materialize` / `graft sync` 的全局门禁，因为这些命令不覆盖 cwd。dirty 只在会直接写用户显式目标时检查：

```text
blocked when target dirty:
  graft patch promote <patch> --to <configured-target> --yes     [E_PROMOTION_DIRTY_TARGET]

allowed regardless of cwd dirty:
  graft scratch read/write/edit/delete          只读/写 daemon scratch graph
  graft patch from-scratch                      只写 Graft store action/application/change/candidate
  graft patch admit <candidate>                 只写 Graft store patch/evidence_refs
  graft patch materialize <state-ref>           写 .worktrees/<state-slug>/
  graft run <state-ref> -- <cmd>                写临时 state root，命令结束后丢弃
  graft sync                                    只写 Graft remote refs/graft/*
  graft patch show / incoming / search          只读
  graft patch validate <id>                     隔离运行，与 cwd 无关
```

#### 不变量

```
Invariant 5.2.1  (CwdIsNotAView)
  cwd dirty 是用户显式目标的门禁，不是 workspace 或 patch graph 的底层状态。
  daemon 内部 verifier / gc / sync diff 等均不考虑 cwd 状态。
```

```
Invariant 5.2.2  (NoImplicitCwdWrites)
  graft patch materialize / graft run 永远不覆盖 cwd。
  任何写 cwd 或 cwd 内 Git repo 的动作只能来自显式 promote target。
```

### 5.3 graft patch materialize

```bash
graft patch materialize <state-ref> [--dry-run]
```

`<state-ref>` 可以是 `graft:empty`、`tree:...`、`application:...`、`candidate:...`、`patch:...`、`repo:<id>@<treeish>` 或 workspace Git treeish。内部先解析成确定 `StateId`，再物化完整 workspace state。显式写外部目标只能通过 `graft patch promote`。

#### 流程

```text
1. 解析 <state-ref> 到 resolved StateId S。
   patch -> patch.application.target_state；candidate -> candidate.application.target_state；
   application -> application.target_state；
   repo:<id>@<treeish> -> 当前 graft.lock + [repos] 确认后的 RepoTree。
2. 从 S/store/repo/Git 构造完整 TreeSnapshot。
3. 选择 output dir：workspace `.worktrees/<state-slug>/`。
4. 在临时 stage 中构造 S 的完整实例。
5. 若 output dir 不存在，atomic rename stage 到 output dir；若 output dir 已存在，先完整 stage，再把旧 output 移到 backup 并发布新 output。
6. 不更新 cwd 路由，不写 evidence，不写 admission/promotion record，不写 Git ref。
```

step 3 的 staging 是为了避免半成品进入 output dir。写 stage 失败时必须保留旧 output；替换已有 output 时不承诺跨平台 atomic swap，但不得先删除 output 再逐文件写入。

### 5.4 graft run

```bash
graft run <state-ref> [--cwd <path>] -- <cmd> [args...]
```

`<state-ref>` 和 materialize 使用同一套解析逻辑。run 在 `$GRAFT_HOME/run/tmp/<run-id>/` 下物化完整 state root；默认 cwd 是 state root，`--cwd` 必须是 state root 内的相对路径。命令允许写临时目录，但写入在命令结束后丢弃；run 不形成 scratch/candidate/evidence/promotion。`--json` 返回 resolved state、cwd、argv、exit code、stdout、stderr。

`call([...], app.target())` 与 `graft run <state-ref> -- ...` 使用同一个 state materialization + command execution model；区别是 validate 会为 property 生成 evidence，run 只返回一次性命令结果。

#### 辅助命令

```bash
graft workspace status             # 展示 cwd lookup 命中层、workspace、daemon 状态
graft patch diff <id-a> <id-b>     # object-to-object diff；不默认绑定 cwd
graft workspace attach --status    # 展示 cwd -> workspace route
```

---

## 6. Sync protocol

Sync 是 Graft 唯一识别的同步动词。没有 push / pull 心智。

```bash
graft sync [<remote>] [--fetch-only] [--push-only]
graft patch incoming
```

### 6.1 远端约定

Graft 远端是一个 Git 仓库，负责存储以下三个固定 refs：

```text
refs/graft/facts          镜像 store/public/{tree,action,application,change,
                                                  property,patch,evidence_refs,
                                                  relation,promotion}/
refs/graft/blobs          镜像 store/public/blob/
refs/graft/manifests      sync checkpoint chain
```

三个都是**非分支**命名空间 (`refs/graft/`*而非 `refs/heads/graft/*`)，不污染托管平台 branch UI。

#### 不同 ref 的原因

- `facts`：多数是小 JSON（< 1 KB），合在一个 tree 结构中 push 友好。
- `blobs`：可能很大（资源文件、二进制依赖）。单独 ref 让 lazy fetch / partial clone 可能。
- `manifests`：每次 sync 附加一个 manifest commit，chain 作为同步历史。

**注意**：evidence body 不进任何 ref。evidence_refs 进 `facts`。

### 6.2 Manifest schema

每次 sync 产出一个 manifest object，写进 `store/public/manifest/`，同时 push 作为 `refs/graft/manifests` 的 commit：

```json
{
  "id": "manifest:abc123def456",
  "version": 1,
  "facts_tip": "<refs/graft/facts 上本次 commit 的 oid>",
  "blobs_tip": "<refs/graft/blobs 上本次 commit 的 oid>",
  "prev_manifest": "manifest:<prev>",
  "summary": {
    "new_patches":   ["patch:91sx8q2h", "patch:bc12ef34"],
    "new_promotions": ["promotion:778899"],
    "new_blobs_count": 12,
    "removed_objects": []
  }
}
```

Manifest 是同步一致点：fetch 后验证 manifest 调用的所有 oid 存在，才接受这次 sync。Manifest body 不携带 host/timestamp；这些只属于本地 transport log metadata，不参与 manifest id。

### 6.3 Sync 状态机

```text
1. 检查当前 workspace 的 [sync] 配置；ws:default 强制不 sync。
2. 若命令提供 `<remote>`，同步成功后写为 `.graft/state/remotes/default`；若未提供 `<remote>`，读取该默认 remote，缺失时报 `[E_SYNC_REMOTE_REQUIRED]`。
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 验证远端格式:
   - 所有 manifest 的 prev_manifest 能连成 chain 或允许的 merge-DAG。
   - facts_tip / blobs_tip 在 fetch 下来的 history 中存在。
   - 每个 typed object 的 filename/id 与 canonical body hash 匹配；同 ID 不同 bytes 是 hard error。
5. 比较 local 与 remote 的 latest manifest:
   case A: local.last_synced == remote.latest_manifest
     全部一致 -> noop 或补齐 store/public/ 缺失对象。
   case B: local.last_synced 在 remote.history 中 (remote ahead)
     远端领先，本地只拉 -> 写 remote object 到 store/public/；evidence_refs 用 set union。
   case C: remote.latest_manifest 在 local.history 中 (local ahead)
     本地领先，远端需推 -> push 合并 + 新 manifest。
   case D: local 和 remote 都有对方没有的 manifest
     divergence -> fail loud；用户先 fetch/review 后再人工处理。
6. 实际写入 / push:
   - immutable public object：按 ID union；缺失则复制；同 ID 不同 bytes 失败。
   - evidence_refs：按 evidence ID set union；owner 不变；updated_at 仅 local display metadata。
   - state/aliases、state/remotes/last_synced：仅本地写，不参与 remote merge。
   - case B: store/public/ 写入缺失对象；state/remotes/<>/last_synced 更新。
   - case C: 构造本次 sync 的 manifest，推 facts/blobs commit + manifest commit。
   - case D: 默认直接报 divergence；用户可显式选择 `--on-divergence keep-remote`
     接受远端 manifest frontier 并跳过本轮 push。
7. 列出 incoming patch tree (§6.5)。
```

### 6.4 Divergence 策略

当前实现暴露两个策略：

- `--on-divergence abort`：默认策略。如果远端 manifest history 与本地 `last_synced`
  不兼容，`graft sync` 拒绝继续并提示先 fetch/review 或人工处理。
- `--on-divergence keep-remote`：仅在本轮允许 fetch 时可用。Graft 接受远端 latest
  manifest 作为新的 `state/remotes/<remote>/last_synced`，fetch 远端 public objects，
  并跳过本轮 push。这个策略不删除本地 immutable public objects；它只让远端 sync
  frontier 在本轮获胜，避免静默丢数据。

`keep-local` 和 `save-both` 还不是 v1 行为：前者需要显式的远端覆盖/删除语义，后者需要
manifest 从单 `prev_manifest` 演进到 merge-DAG。当前 CLI 不接受未实现策略，避免用 flag
伪装成已解决的数据模型。

### 6.5 Incoming tree 渲染

sync 完后、或手动 `graft patch incoming`：

```text
$ graft patch incoming

incoming patches reachable from origin (since last sync):

base: tree:551a2bf3 (local route context)
├── patch:bc12ef34  "fix: gc reachability"
│   target: tree:7a2f1c9d
│   properties: cargo_tests_pass ✓
│   ev refs:    2 referenced by remote, 0 locally rebuilt
│   └── patch:dd991122  "tweak gc traversal"
│       target: tree:8b3c4d77
│       properties: docs_only ✓
│       ev refs:    1 referenced by remote, 0 locally rebuilt
└── patch:91sx8q2h  "feat: scratch read mode"
    target: tree:9d8e3f01
    properties: cargo_tests_pass ✓  cargo_fmt_clean ✓
    ev refs:    3 referenced by remote, 0 locally rebuilt

base: tree:ffeedd00 (not in your store)
└── patch:778899ab  "experimental: fact compactor"
    base unknown locally; fetch source repo or migrate

local route context resolves to tree:551a2bf3 (clean).

suggested:
  graft patch materialize patch:bc12ef34   # smallest delta
  graft patch materialize patch:dd991122   # follow chain
  graft patch materialize patch:91sx8q2h   # alternative branch
  graft verify-pending                     # rebuild evidence locally
```

#### 渲染原则

1. **按 base_state 分组**。同一 base 下的 patch 可能是串联或并列，由 base->target 的本地拓扑决定取 sibling 还是 nested。
2. **本地 cwd 的 base 置顶**，其他 base 被始终按 "不在本地 store"/"远古老" 等标签分组。
3. **patch 核心信息以 property 为主**，evidence 给 drill-down。`graft patch show patch:X --evidence` 展开。
4. **property drift 标注 stale**：如果 patch 的 PropertyId 与当前同名 property 函数解析结果对不上，渲染为 `cargo_tests_pass ✓ (property drift; was X, now X')`。
5. **未本地重建的 evidence** 标 "referenced by remote, 0 locally rebuilt"。

### 6.6 Evidence sync 细则

```text
sync over the wire:
  store/public/evidence_refs/<owner>.json     → refs/graft/facts
  store/derived/evidence/<id>.json            ✖ (不传输)

push 时:
  evidence_refs 中包含远端还没有的 evidence ID 不是问题——ID 是
  内容寻址的，远端看到后可以选择本地 verify-pending 补上 body。

fetch 时:
  拉到远端 evidence_refs 后，本地 store/derived/evidence/ 中应该查
  evidence_refs 中出现但本地缺失的 ID，标为 "pending local rebuild" / "referenced by remote"。

append-only union (重要):
  同一个 evidence_refs[<owner>].json 可能 local 和 remote 都 append 了
  不同 entry（A 本地 verify 了 cargo_fmt_clean，B 本地 verify 了 cargo_tests_pass
  都 push 了）。这 NOT 是 conflict——是 union。
  sync 算法读 local 和 remote 两份后按 evidence ID set union 写一份。
  updated_at 取较新者仅用于展示；不参与 EvidenceId 或 owner identity。

reuse / invalidation:
  evidence body 只有在本地 store/derived/evidence/<id>.json 存在，且 body 中
  (ApplicationId, PropertyId, ValidationRunKey, canonical result/effects/outputs)
  与当前查询完全匹配时，才能满足 admission/promote/search。
  description/severity 变化不影响既有 evidence；PropertyId 变化会让旧 evidence 不再
  满足当前 property name 要求。ApplicationId 或 ValidationRunKey 变化也必须重跑。
```

#### 不变量

```
Invariant 6.6.1  (EvidenceRefsAreSetUnionAcrossSync)
  evidence_refs 是 Graft 中唯一允许 sync 两边都修改的对象。
  其重复动作按 evidence ID 集合 union；其他 typed object 都是 content-addressed
  不可变，不可能出现 两边都改 的场景。
```

### 6.7 Repo base 外部依赖处理

patch.application 的 `base_state = RepoTree {repo_id, treeish, resolved_tree_oid}` 时：

```text
sync push 时:
  不试图 顺象 这个 oid (外部 git repo 不受 graft 控制)。
  manifest.summary 中记 repo:<id>@<oid> 依赖。

fetch 后表现:
  graft patch show patch:X 显示 base = repo:linux-stable@<oid>。
  graft patch materialize patch:X:
    检查隔离 worktree 能不能拿到 oid。
    能  -> 隐式 fetch oid 后 materialize。
    不能 -> [E_REPO_OID_UNAVAILABLE]，提示检查 graft.toml + git fetch。
```

---

## 7. Clone

```bash
graft get <remote> <dir>
```

### 7.1 行为

```text
1. mkdir <dir> + cd <dir>。检查并且拒绝已存在 .graft/；.git/ 不属于 Graft workspace，可存在但不被写入。
2. 创建 workspace 骨架并登记 registry:
   .graft/config.toml          { remotes.origin.url = <remote> }
   .graft/store/{public,private,derived}/
   .graft/state/
   worktrees/
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 走 sync 状态机 (§6.3)；case B （remote ahead） 在这里是唯一可能。
5. 写入 store/public/。state/remotes/origin/last_synced 设为远端 latest。
6. cwd 留空（不创建 graft.toml，不创建 properties.roto）。
7. 提示用户选择下一步:
   - graft patch incoming
   - graft patch materialize <application:|patch:|tree:|candidate:|repo:|git-treeish>
   - graft workspace init  (如果想从 graft:empty 起新工作流)
```

### 7.2 初始提示输出

```text
$ graft get https://example.com/foo.git ./foo
fetched .graft from origin:
  refs/graft/facts:     312 objects
  refs/graft/blobs:     48 blobs
  refs/graft/manifests: 17 manifests
last_synced = manifest:abc123de

cwd is empty.

3 admitted patches reachable。选择下一步:

  graft patch incoming                     查看全部可物化对象
  graft patch materialize <application:|patch:|tree:>  输出到 .worktrees/<state-slug>/
  (or do nothing; .graft/ is fully populated for read-only inspection)
```

### 7.3 不变量

```
Invariant 7.3.1  (CloneDoesNotMaterializeByDefault)
  graft get 后 cwd 不被写入，需要显式 graft patch materialize 才输出到 .worktrees/<state-slug>/。
  原因：任何 "默认视图" 都会退化成 main 心智，与 Graft 所有权 明确同意 原则不符。
```

```
Invariant 7.3.2  (CloneStateIsAuthoritative)
  graft get 拉下的 store/public/ 与 remote 完全对齐 (manifest 验证后)。
  本地不产生额外修改。state/remotes/origin/last_synced 准确反映 fetch tip。
```

---

## 8. Daemon

### 8.1 唯一 writer

Graftd 是 `$GRAFT_HOME` 与所有 workspace `.graft/` 的唯一 writer。任何写命令 (CLI 进程、skill、SDK) 都走 wire op 到全局 daemon。

```text
CLI 进程:
  parse argv
  resolve workspace_id via §12 lookup
  resolve $GRAFT_HOME/run/daemon.sock
  if not exists or stale -> spawn global graftd
  send wire frame { workspace_id, argv, ... }
  await response
  render output

daemon 进程:
  bind global socket
  route each op by workspace_id
  lazily load WorkspaceState into HashMap<WorkspaceId, WorkspaceState>
  serialize writes per workspace (independent write locks)
  compute, write store/state/registry, respond
```

#### 为什么需要唯一 writer

- store/ 中 evidence_refs 是 append-only，需要 read-modify-write 原子性。
- state/aliases/* 可能跨 alias 互相引用（admit 删 candidate alias 中间态），需要事务。
- registry.toml 是全局 routing/index，需要 flock + daemon 串行化。
- sync 是多步骤操作 (fetch / write / push)，需要串行。

CI / sandbox 场景靠 daemon 的 idle timeout 自动退出（§8.4）。

### 8.2 IPC 协议

当前实现使用 `cli_exec` wire op 承载多数 daemon-owned workspace 写命令——daemon 接收 argv、在 daemon 进程内解析并执行同一套 command logic。P8 起每个 routed 请求必须携带 `workspace_id`；daemon 通过 registry 解析 workspace，并把可选 `workspace_root` 只当作一致性校验。`cli_exec` 不是泛用 argv 后门：scratch/candidate 走 typed RPC，`attach`/`detach` 走 workspace registry typed op，status/show/property/init 等本地或只读命令不允许通过 `cli_exec`。

```json
{
  "op": "cli_exec",
  "workspace_id": "ws:default",
  "workspace_root": "/Users/me/project",
  "argv": ["graft", "--cwd", "/Users/me/project", "patch", "validate", "candidate:abc"]
}
```

将来可能迁移到粒度更细的 typed RPC (e.g. AdmitRequest / SyncRequest)，但所有 RPC 仍携带 `workspace_id`。

### 8.3 进程状态文件

```text
$GRAFT_HOME/run/
  daemon.sock     unix socket 端点
  daemon.pid      当前 daemon PID，信息性
  validation/     verifier sandbox run dirs
  tmp/
```

daemon 启动时:

```text
1. 检查 daemon.pid:
   - 不存在 -> 启动。
   - 存在但进程死 (kill -0 失败) -> 覆盖 PID、启动。
   - 存在且进程活 -> 连接现有 daemon。
2. bind daemon.sock。如果冲突 -> 先探活；确认 stale 后覆盖重 bind。
3. 清理 validation/, tmp/ 中遗留内容。
```

socket bind + PID + `kill -0` 探活足够保障单 daemon；registry.toml 写入另有 flock 保护。

### 8.4 Idle timeout

```text
daemon 闲置 (所有 workspace 都 no wire op for N minutes) 后自动退出。
默认 N=30。在 $GRAFT_HOME/config.toml [daemon].idle_timeout_minutes 调整。
```

CI 场景：一条命令启动 daemon，idle 后退出。退出时 daemon 清理自己的 socket / PID 文件。

### 8.5 崩溃恢复

```text
daemon 崩溃 (oom / segfault) 后:
  store/ 中已写入的内容安全 (content-addressed，部分写入的文件 hash 不匹配
    被下次写覆盖或 gc 中检出)。
  state/aliases/* 是 atomic rename，不会读到部分内容。
  evidence_refs 是 read-mod-write + atomic rename，不会读到不一致状态。
  scratch 丢失（daemon-instance-scoped）。
  $GRAFT_HOME/run/validation/ tmp/ 可能有孤儿，下次启动清理。
  workspace `.worktrees/` inspection output 由 doctor/gc 按策略清理，不在 daemon 启动时盲删。

下次 daemon 启动 恢复有序。CLI 重试连接。
```

---

## 9. Garbage collection

```bash
graft workspace gc                    # 默认 dry run/report
graft workspace gc --apply
graft workspace gc --derived-only      # 只清 store/derived/
```

### 9.1 可达性 roots

```text
roots =
    state/aliases/{candidates,patches,promotions}/* 解析到的 ID
  ∪ 当前 properties.roto 顶层 property 函数解析到的 PropertyId 集合
  ∪ [admission].required_properties 解析到的 PropertyId
  ∪ [promotion].required_properties 解析到的 PropertyId
  ∪ [promote_targets.*].required_properties 解析到的 PropertyId
  ∪ daemon 内存中当前 active scratch / lease 中的 blob/tree
```

本地仓库不访问 remote。gc 仅看本地；sync 后本地 store 会被 roots 全覆盖。

### 9.2 达可 walk

从 roots 出发递归:

```text
candidate.application      -> application
patch.application          -> application
application.action         -> action
application.base_state     -> tree (若 Tree variant)
application.target_state   -> tree (若 Tree variant)
application.change         -> change
application.applicability_proof.steps[].matched blob/context -> blob
action.BlobExpr / TextHunk digests -> blob
change.base_state          -> tree (若 Tree variant)
change.target_state        -> tree (若 Tree variant)
change.ops[].blob          -> blob
tree.entries[].hash        -> blob
evidence_refs[<id>].evidence -> evidence (in store/derived/)
evidence.application       -> application
evidence.change            -> change (endpoint view, if present)
evidence.property          -> property
relation.inputs[]          -> any
relation.outputs[]         -> any
promotion.patch            -> patch
manifest.facts_tip / blobs_tip   (仅验证; 不作为达可 walk 起点)
```

标记可达；diff 得 orphan。

### 9.3 清理策略

```text
store/public/   orphan -> 默认只报告，--apply 后删除
store/private/  orphan -> 默认只报告，--apply 后删除
store/derived/  --derived-only 或常规 gc 都可清。
                 可重建 (verifier)。
store/public/blob/  默认保留仍由 reachable tree/object walk 保护
```

#### 不变量

```
Invariant 9.3.1  (DerivedAlwaysSafeToDelete)
  rm -rf .graft/store/derived/ 任意时候安全。
  daemon 下次需要某个 evidence body 时重跑 verifier 即可重建。
```

```
Invariant 9.3.2  (NoSilentLossInPublicGc)
  store/public/ 的 gc 默认 dry-run/report。仅在 --apply + 可达性 walk 不可达时删除。
  remote 中仍可达但本地被 gc 的对象下次 sync 可以重拉。
```

---

## 10. Invariants and failure modes

本节集中列出全文不变量和常见失败模式，以便实现时逐个检查。

### 10.1 全文不变量总表


| Inv   | 名称                                 | 位置   |
| ----- | ---------------------------------- | ---- |
| 2.5.1 | PropertyNameIsIdentityInput        | §2.5 |
| 2.6.1 | EvidenceContentAddressing          | §2.6 |
| 2.6.2 | ObservedEvidenceReproducibility    | §2.6 |
| 3.3.1 | NoDriftingExternalReferences       | §3.3 |
| 3.3.2 | LockSchemaUniformity               | §3.3 |
| 4.4.1 | PromotionIsTheOnlyTargetProjection | §4.4 |
| 5.2.1 | CwdIsNotAView                      | §5.2 |
| 5.2.2 | NoImplicitCwdWrites                | §5.2 |
| 6.6.1 | EvidenceRefsAreSetUnionAcrossSync  | §6.6 |
| 7.3.1 | CloneDoesNotMaterializeByDefault   | §7.3 |
| 7.3.2 | CloneStateIsAuthoritative          | §7.3 |
| 9.3.1 | DerivedAlwaysSafeToDelete          | §9.3 |
| 9.3.2 | NoSilentLossInPublicGc             | §9.3 |
| 12.5.1 | MetaConfigIsPatch                 | §12.5 |


### 10.2 错误码总表


| Code                            | 含义                                                                     | 处理                                                              |
| ------------------------------- | ---------------------------------------------------------------------- | --------------------------------------------------------------- |
| `[E_NO_WORKSPACE]`              | 当前 cwd 没有 parent workspace、route 或 GRAFT_WORKSPACE                     | graft workspace init、graft workspace attach 或设置 GRAFT_WORKSPACE |
| `[E_NO_CONFIG]`                 | workspace 缺少 graft.toml                                                | graft workspace init 或检查 registry route                         |
| `[E_NO_WORKSPACE_CONFIG]`       | 当前命令解析到的 workspace 缺少 graft.toml/properties                            | graft workspace init 或修复 workspace root                         |
| `[E_UNSUPPORTED_CONFIG_SCHEMA]` | graft.toml schema 不是当前支持的版本                                            | 迁移配置或降回 schema = 1                                              |
| `[E_LEGACY_ID]`                 | 输入了 gr_/grc_/ev_/... 旧 ID                                              | 采用 `<kind>:<digest>`                                            |
| `[E_PROMOTION_DIRTY_TARGET]`    | promote 的 local target dirty                                           | 清理 target 后重试                                                   |
| `[E_EMPTY_CHANGE]`              | 显式要求 non-empty 的 relation transform 产出空 endpoint diff             | 放宽要求或提供实际变更                                                |
| `[E_PROPERTY_LOCK_MISSING]`     | `graft.lock` 缺失，普通命令不会自动重建                                             | 运行 `graft property lock`                                        |
| `[E_PROPERTY_LOCK_DRIFT]`       | properties.roto 中 X 变但 lock 未同步                                        | 构造元配置 patch 刷新 lock                                             |
| `[E_PROPERTY_COMPILE]`          | properties.roto parse/typecheck/compile 失败，或 Roto compiler panic 被隔离捕获 | 修复 properties.roto；bug 时附最小 repro                               |
| `[E_PROPERTY_REQUIRES_CYCLE]`   | Property.requires 图中存在环                                                | 打断依赖环                                                           |
| `[E_PROPERTY_REQUIRES_UNKNOWN]` | Property.requires 引用了不存在的 property name                                | 修改 properties.roto 或 graft.toml                                 |
| `[E_REPO_LOCK_DRIFT]`           | graft.toml repo url/default_branch 与 lock 不一致                          | graft repo update                                               |
| `[E_UNKNOWN_PROPERTY]`          | properties.roto 中不存在指定的顶层 property 函数                                  | 修改 properties.roto 或 graft.toml                                 |
| `[E_REPO_OID_UNAVAILABLE]`      | patch materialize 需要某 oid 但本地/远端 git 都拉不到                              | git fetch 该 oid                                                 |
| `[E_CHANGE_INTEGRITY]`          | application 的 apply/replay core integrity 校验失败                       | 重新构造 application；检查 Action/proof/change                  |
| `[E_ADMISSION_UNMET]`           | required 某 atom 无 passed evidence                                      | graft patch validate                                            |
| `[E_PROPERTY_DRIFT]`            | candidate.expected 与当前 properties.roto 函数映射不一致                         | 二选一 (§4.3)                                                      |
| `[E_PROMOTION_UNMET]`           | promote required_properties 未满足                                        | 补 evidence 后重试                                                  |
| `[E_PROMOTION_NOT_FF]`          | remote-push target ref 不能 FF                                           | --force-push 或调整 base                                           |
| `[E_PROMOTION_TARGET_UNKNOWN]`  | --to 未在 graft.toml 的 [promote_targets] 声明                              | 补上 [promote_targets.]                                           |
| `[E_COMPOSE_CONFLICT]`          | compose / migrate / revert 遇 conflict，v1 不建模                           | 手动 candidate (§4.5)                                             |
| `[E_SCRATCH_LOST]`              | daemon 重启后 scratch 状态失效                                                | 用 `--base <base>` 重新开始                                          |
| `[E_SYNC_DEFAULT_WORKSPACE]`    | `ws:default` 是 machine-local workspace，不能 sync                         | 创建或 attach 一个 local workspace                                   |
| `[E_SYNC_DISABLED]`             | 当前 workspace 显式设置了 `[sync] enabled = false`                              | 删除该 override，或在 graft.toml 设置 `[sync] enabled = true`          |
| `[E_SYNC_REMOTE_REQUIRED]`      | `graft sync` 未传 `<remote>` 且 workspace 还没有 `.graft/state/remotes/default` | 先运行一次 `graft sync <remote>`                                      |
| `[E_SYNC_REMOTE_INVALID]`       | sync remote 不存在、不是 Git 仓库，或已有非空非 Git 内容                                | 检查 remote 路径；首次 push 可用空目录或新路径                                  |
| `[E_SYNC_DIVERGENCE]`           | sync manifest history 与本地 last_synced 不兼容                              | 先 fetch/review；或在允许 fetch 的 sync 中显式 `--on-divergence keep-remote` |
| `[E_REMOTE_INCOMPLETE]`         | remote refs 指向的 facts/blobs/manifests object 缺失或不可解析              | 检查/修复 remote；不要接受这次 sync                              |


### 10.3 常见状态转换错误

```text
未登记 cwd 里执行 scratch / candidate 命令:
  cwd 只参与 workspace route lookup；未命中时以 [E_NO_WORKSPACE] 失败。
  若要使用 machine-local ws:default，必须显式 `graft attach`。
  scratch 第一版必须显式 `--base <base>`，不会从 cwd 或 repo path 推断 base。
  expected 由 resolved workspace 的 graft.toml/properties 提供。

properties 改名后老 evidence 表现:
  顶层函数名进入 PropertyId；改名即 X -> X'。
  老 evidence 仍在、仍可查，但对新名字在 admission/promotion 中不再有效。
  若只是想改人读文本，改 description；它不改变 PropertyId。

properties 改 spec 后老 evidence 表现:
  checks 或 requires 漂移 X -> X'。
  老 evidence 仍在、仍可查，但对当前 property name 在 admission 中不再有效。
  graft patch show patch:X --evidence 标 "property drift; was X, now X'"。
  重新 admission 需要 graft patch validate <X> --expect <name> 补 X' evidence。

fetch 后 patch 未本地 verify:
  graft patch show patch:X 渲染 "cargo_tests_pass ✓ (referenced by remote, not yet locally
  rebuilt)"。admission 查询 evidence body 在 store/derived/ 缺失 -> [E_ADMISSION_UNMET]。
  提示跑 graft patch validate patch:X --expect <property> 或 graft verify-pending。
```

### 10.4 远端随意变动下的安全性

```text
remote 被别人 force push refs/graft/manifests:
  fetch 后 local.last_synced 不在 remote.history 中 -> case D divergence。
  实际是 remote rewrite；graft 默认 abort 让用户调查。

remote tree 被人删了某个 object 文件:
  fetch 验证阶段 需中 manifest.facts_tip 调用的 oid 是否可解析。
  如果不可 -> [E_REMOTE_INCOMPLETE]。不接受这次 sync。

remote 动了 git push 允许 fetch 部分对象:
  本地仅验证 facts/blobs/manifests 三个 ref 的 tip + history 验证。
  任何 patch_evidence 多出项 被识别为 Inv 6.6.1 下的 union 变动。
```

---

## 11. CLI 索引

本节以动词为主列举 CLI surface。详细参数参考各节。

### Help / agent guidance

```bash
graft explain agent-workflow       # 推荐 agent/tool 主流程；pi-graft graft_help 的默认 topic
graft explain workflow             # 同一流程的短 alias
graft explain <topic-or-command>    # scratch/candidate/admit/materialize 等概念说明
graft explain <diagnostic-or-property>
```

`agent-workflow` 是仓库维护的 walkthrough：scratch 只负责草稿，`graft patch from-scratch` 生成 candidate，`graft patch admit` 生成 patch，`graft patch materialize` / `graft run` 是检查输出；外部 `graft patch promote`、sync、compose/migrate/revert、`repo add/sync/lock/update`、`bundle import`、`workspace gc --apply` 等低频写命令可走手动 CLI 或 pi-graft `graft_cli_exec` argv。读/检查命令（如 patch show/search/incoming、repo list、bundle export、workspace gc dry-run）保留本地 CLI 路径，不经 `cli_exec`。

### Bootstrap / workspace routing

```bash
graft workspace init [--register-only]       # 创建或登记 cwd local workspace
graft workspace attach [--workspace <id>]    # cwd -> workspace route；git cwd 自动登记 repo
graft workspace detach                       # 删除 cwd route，不删 workspace repos
graft workspace attach --status              # 展示 cwd lookup 命中层与 route
graft workspace ps                           # 列 registry workspaces + daemon liveness
graft workspace doctor [--rebuild-registry]  # 检查 registry/daemon/workspace 健康
```

### State / view

```bash
graft workspace status                      # cwd lookup + workspace + daemon 状态
graft patch diff <a> <b>                    # object-to-object diff
graft patch materialize <state-ref> [--dry-run]   # 默认写 <workspace-root>/.worktrees/<state-slug>/
graft run <state-ref> [--cwd <path>] -- ... # 临时物化完整 state 并丢弃命令写入
```

### Lifecycle

```bash
graft scratch write/edit/delete [--repo <repo-id>] (--base <base> | --from <scratch-id>) ...
graft patch from-scratch <scratch-id> [--expect <property>...] [--producer <label>] [--message <msg>]
graft patch validate <id> [--expect <property>...]
graft patch admit <candidate-id> [--require <property>...]
graft patch promote <patch-id> --to <target-or-branch> [--require <property>...]
```

### Promote targets

```toml
[promotion]
required_properties = []

[promote_targets.release]
path = "../external-git-repo"              # 必须 clean；dirty -> E_PROMOTION_DIRTY_TARGET
branch = "main"
required_properties = ["cargo_tests_pass"]

[promote_targets.docs]
path = "../docs-repo"
branch = "graft-out"
```

`graft patch promote <patch> --to release --yes` writes the configured repo/ref. `graft patch promote <patch> --to main` without a matching `promote_targets.main` is an explicit branch dry-run unless `--yes` is passed.

### Sync / collaboration

```bash
graft sync [<remote>] [--fetch-only|--push-only]
graft patch incoming
graft verify-pending                        # hidden compatibility maintenance command; rebuild evidence locally for refs in store/public
```

Sync is disabled for `ws:default` and enabled by default for all other workspaces unless `[sync] enabled = false` is explicitly configured.

### Inspect

```bash
graft patch show <id> [--evidence] [--change]
graft patch search [--property <property>] [--base <id>] [--producer <label>] [--has-evidence <property>]
graft patch list [--candidates|--all]       # 列 patch 或 candidate
graft repo list
graft repo lock [<name>] | update [<name>]
```

### Relation

```bash
graft patch compose <patch:a> <patch:b>     # may [E_COMPOSE_CONFLICT]
graft patch migrate <patch:a> --onto <state> # may [E_COMPOSE_CONFLICT]
graft patch revert <patch:a>                # may [E_COMPOSE_CONFLICT]
```

### Scratch

```bash
graft scratch read [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --mode <hashlines|...>
graft scratch write [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --content <bytes>
graft scratch edit [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --edits <json>
graft scratch delete [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path>    # alias: rm
graft scratch diff <from-scratch-id> <to-scratch-id>
graft scratch drop <scratch-id>
graft scratch pin / unpin
graft scratch status

graft patch from-scratch <scratch-id> --expect <property>... --message <msg>
```

`scratch` namespace 只负责临时草稿读写、编辑、删除、diff、pin/drop；第一次操作用 `--base` 隐式创建 root scratch，后续操作用 `--from` 指向上一版 scratch。Rename 由 delete+write 表达，candidate / patch 生成在 scratch 外部完成。`graft patch from-scratch` 是 scratch→candidate 的 canonical lifecycle command；CLI 与 pi-graft 插件共同调用 daemon `candidate_from_scratch` wire op（request: `scratch`, `expected`, `producer`, `message`; response: `candidate`, `changed_paths`）。

### Maintenance

```bash
graft workspace gc [--apply] [--derived-only]
graft bundle export <path>
graft bundle import <path>
graftd status                               # uses $GRAFT_HOME/run/daemon.sock
graftd stop
```

### Validation & dev hygiene

```bash
just check      # cargo fmt --all -- --check + cargo clippy --locked --workspace --all-targets -- -D warnings
just test       # cargo test --locked --workspace --all-targets + cargo test --locked --doc --workspace
just smoke      # fail-fast tests/*.sh
just prek       # uvx prek run --all-files
just cov        # cargo llvm-cov test --locked --workspace --all-targets, writes lcov.info
```

---

## 12. Workspace discovery, registry, attach

P8 的核心翻转：workspace 是 user-level 对象，cwd 只是 attach key。Graft 永远不因为 cwd 是 Git repo 而拒绝；cwd 的 `.git/` 只在 attach / promote target 里作为可选外部 Git 信息使用。

### 12.1 `$GRAFT_HOME`

```text
$GRAFT_HOME                 # default: ~/.graft
  registry.toml             # machine-local registry, flock + .bak
  config.toml               # machine-local daemon defaults
  run/
    daemon.sock
    daemon.pid
    validation/
    tmp/
  workspaces/
    default/                # ws:default system workspace root
      graft.toml
      graft.lock
      properties.roto
      .graft/
      worktrees/
```

`$GRAFT_HOME` follows Cargo-style env override. If unset, use `~/.graft`.

### 12.2 registry.toml schema

`registry.toml` is machine-local. It is never synced and never interpreted as a patch.

```toml
schema = 1

[[workspaces]]
id = "ws:default"
kind = "system"                    # system | local
root = "/Users/me/.graft/workspaces/default"
created_at = "2026-06-02T00:00:00Z"

[[workspaces]]
id = "ws:project"
kind = "local"
root = "/Users/me/src/project"
created_at = "2026-06-02T00:00:00Z"

[[routes]]
cwd = "/Users/me/src/checkout"
workspace = "ws:default"
created_at = "2026-06-02T00:00:00Z"

[[repo_paths]]
repo_id = "repo:9b2c..."
paths = ["/Users/me/src/checkout"]
last_seen_at = "2026-06-02T00:00:00Z"
```

Tables:

- `[[workspaces]]`: index of known workspaces. No disk scan is performed implicitly.
- `[[routes]]`: cwd realpath -> workspace_id routing table.
- `[[repo_paths]]`: RepoId -> local clone paths. This is where local paths live; `graft.lock` never stores local paths.

Writes use `flock`, write `.bak`, then atomic rename. `.bak` is diagnostic material, not an automatic routing source: if `registry.toml` is corrupt, normal commands fail loud instead of silently recovering through stale backup data. `graft doctor --rebuild-registry` can rebuild known system workspace records under `$GRAFT_HOME/workspaces/*`; otherwise the user must re-register roots explicitly.

### 12.3 Workspace discovery order

Every CLI command begins by resolving a

```text
1. $GRAFT_WORKSPACE env
   - value is workspace id or absolute root
   - highest priority
2. cwd parent chain contains graft.toml + .graft/
   - local workspace root
   - auto-register in registry if missing
3. registry.toml [[routes]] exact/prefix match for cwd realpath
   - returns workspace id -> root
4. otherwise fail with [E_NO_WORKSPACE]
   - no implicit route writes
   - use `graft workspace init`, `graft workspace attach`, or `GRAFT_WORKSPACE`
```

Fallback notice is user-facing but short, e.g.:

```text
graft: attached /Users/me/src/checkout to ws:default (run `graft attach --status` for details)
```

### 12.4 ws:default bootstrap

`ws:default` is system workspace, rooted under `$GRAFT_HOME/workspaces/default`. It is lazily created only when explicitly requested, for example by `graft attach` with no `--workspace`.

Bootstrap creates an empty policy baseline:

```toml
# graft.toml
schema = 1

[admission]
required_properties = []

[promotion]
required_properties = []

[sync]
enabled = false
```

`properties.roto` starts as an empty comment-only property source. The daemon writes an empty `[properties]` lock and relies on core application integrity (`apply(action, base, proof) == target` and `replay(base, change.ops) == target`) for default admission/materialization/promotion. Workspaces add explicit top-level property functions such as `empty_change`, `docs_only`, or `cargo_tests_pass` when they need policy beyond that invariant.

Rules:

- `ws:default` is machine-local and **never syncs**.
- Other workspaces default to sync; `[sync] enabled = false` opts a workspace out of `graft sync` pushes.
- Admission is still meaningful without sync: it creates local public patches in `store/public/`.

### 12.5 Meta-config is patch-admitted

All workspace-owned files are changed through patch admit:


| File / tree                              | Channel                                         |
| ---------------------------------------- | ----------------------------------------------- |
| `graft.toml`                             | patch admit                                     |
| `graft.lock`                             | same patch as the triggering meta-config change |
| `properties.roto`                        | patch admit                                     |
| user code/docs/data tracked by workspace | patch admit                                     |
| `$GRAFT_HOME/registry.toml`              | daemon typed write, not a patch                 |
| `.graft/store/`*                         | daemon internal writes                          |


Meta-config patch examples:

- `graft repo add <repo_id> <url>` adds `[repos.<repo_id>]` and refreshes the matching lock entry.
- `graft repo update <repo_id>` refreshes `treeish` / `resolved_oid`.
- user adds `[promote_targets.release]`.
- user edits `properties.roto` property function `cargo_tests_pass`.

`graft attach` is deliberately not a meta-config patch: it only changes machine-local routing/index data in `$GRAFT_HOME/registry.toml`.

Invariant:

```text
Invariant 12.5.1  (MetaConfigIsPatch)
  Any workspace-owned configuration change is admitted as a patch under the
  current admission policy. registry.toml is not workspace-owned and is the
  only routing/index exception.
```

### 12.6 graft.toml / graft.lock repo schema

`graft.toml` contains user intent:

```toml
[repos.<repo_id>]
url = "https://github.com/owner/repo"
default_branch = "main"  # optional; repo add fills this from remote HEAD
```

`graft.lock` contains the derived resolved base:

```toml
[repos.<repo_id>]
url = "https://github.com/owner/repo"
treeish = "main"
resolved_oid = "tree-oid:..."
resolved_at = "2026-06-02T00:00:00Z"
```

`repo_id` is not a commit hash and is not derived from the URL. It is the stable workspace-local repository name used by config, lock, base refs, and `.graft/repos/<repo_id>`. `url` is repeated in the lock so base resolution can fail loudly when `graft.toml` points the same `repo_id` at a different repository. `resolved_oid` identifies the resolved base tree snapshot. Local external checkout paths discovered by attach live only in registry `[[repo_paths]]`.

### 12.7 attach / detach

`graft attach [--workspace <id>]` is a daemon primitive. Client IPC uses a typed workspace registry op, not `cli_exec`; the frontend starts or contacts the global daemon through the system default workspace anchor under `$GRAFT_HOME/workspaces/default`, so attaching an arbitrary cwd never initializes `.graft/` in that cwd.

Attach flow:

```text
1. resolve target workspace (default ws:default)
2. registry typed write:
   - upsert [[routes]] cwd -> workspace
3. if cwd is inside a Git worktree:
   - resolve the Git worktree root and remote origin URL
   - compute registry RepoId from the origin URL after stripping only Git's output line ending; do not whitespace-trim the URL
   - registry typed write: upsert [[repo_paths]] RepoId -> git worktree root
4. print concise summary
```

`graft attach` never mutates workspace-owned `graft.toml` or `graft.lock`. Use `graft repo add` when the external repository should become part of workspace configuration.

`graft detach` only removes the current cwd route from registry. It does not delete `[repos.*]` because repo declarations are workspace state and may still be used by patches.

`graft attach --status` shows:

- cwd realpath
- lookup layer hit (env/local/route)
- workspace id/root
- matched route if any
- Git repo detection and RepoId/path registration status

### 12.8 Global multi-tenant daemon

There is one daemon per `$GRAFT_HOME`:

```text
socket = $GRAFT_HOME/run/daemon.sock
pid    = $GRAFT_HOME/run/daemon.pid
```

Runtime state:

```rust
struct DaemonState {
    registry: Registry,
    workspaces: HashMap<WorkspaceId, WorkspaceState>,
}
```

Rules:

- `WorkspaceState` is lazy-loaded on first request.
- each workspace has an independent write lock.
- registry writes use daemon serialization + registry flock.
- all IPC requests carry `workspace_id`; no `kind = patch | local` field exists.
- daemon exits only when all workspaces are idle for the configured timeout.
- per-workspace run directory does not exist.

### 12.9 Promote targets

`graft patch promote` is the only target projection verb. It does not materialize cwd and does not change routes.

Configured targets:

```toml
[promotion]
required_properties = []

[promote_targets.release]
path = "/Users/me/src/repo"      # may be "." after cwd resolution
branch = "main"
required_properties = ["cargo_tests_pass"]

[promote_targets.docs]
path = "/Users/me/src/docs-repo"
branch = "graft-out"
```

Dirty policy:

- configured `promote_targets.<name>.path`: target repo/worktree must be clean before `--yes`. Dirty fails with `[E_PROMOTION_DIRTY_TARGET]`.
- explicit `--to <branch>` without configured target uses the current cwd Git repo as the external target and follows the same `--yes` side-effect boundary.

### 12.10 CLI and error-code deltas from v2

Removed / obsolete:

- cwd-root Git prohibition is removed; `.git/` is ignored for snapshot and allowed for attach/promote.
- the old Git-in-workspace error code is removed.
- per-workspace daemon socket flags are removed from normal CLI help.
- `graft patch materialize <state-ref>` no longer overwrites cwd; the old `--discard` flag is accepted only as hidden compatibility no-op.
- `state/cwd` no longer defines a default view.
- `graft discard` is obsolete; cwd is not a managed view and cannot be restored from Graft state.

New / changed:

- `graft workspace init [--register-only]` is idempotent and registers local workspace roots.
- `graft workspace attach`, `graft workspace detach`, `graft workspace attach --status` manage cwd routes.
- `graft workspace ps` lists registry workspaces and daemon liveness.
- `graft workspace doctor` diagnoses stale workspace roots, stale routes, orphan daemon, registry corruption.
- `graft patch promote --to <name>` selects either a configured `[promote_targets.<name>]` target or an explicit branch name.

---
