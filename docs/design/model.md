# Graft 设计 · 对象模型（§2.1–§2.4）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`../graft-kernel.lean`](../graft-kernel.lean)。

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

mutable durable local bookkeeping（alias、remote 同步进度、查询索引）不在 `store/`，在 workspace `local/`；cwd route / repo local paths 在 `$GRAFT_HOME/registry.toml`，详见 [§3.2](./workspace.md) 和 [§12](./workspace.md)。目录名刻意不用 `state/`，以免与 patch 论里的 `State` / `StateId` 混淆。

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
- `action` 是 base-polymorphic 的纯语法 AST；`application` 是 (action, base, applicability_proof, target, change) 的具体实化。两者都是内容寻址 immutable public object，body schema 在 §2.4。candidate / patch 的 body 引用 `application:<digest>` 而不是内嵌 base/target/change（详见 [§2.7](./admission.md)）。
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

#### Action semantics in Lean

The formal kernel interprets an `Action` as a partial state transition. `idAction`
is the identity transition, and `composeAction a1 a2` means "run `a1`, then run
`a2`". The Rust `Action::Sequence` representation is the concrete n-ary lowering
of this binary composition law.

形式定义见 [`docs/graft-kernel.lean`](../graft-kernel.lean)：`State` / `Action` / `sem` /
`idAction` / `composeAction` 在「核心模型」段，`sem_id`、`sem_seq` 两条 axiom 在「核心公理」
段。`sem : Action → State → Option State` 是指称语义解释器（graft 的 apply 函数），数学上是
*自由 action 幺半群 → `Option`-Kleisli 幺半群的同态*；`sem_seq` 把 `composeAction a1 a2`
解释为 Kleisli 合成 `(sem a1 s).bind (sem a2)`，即先 `a1` 后 `a2`。同态性使语义结合律免费，
这正是 Rust `Action::Sequence` n 元拍平的 sound 依据。偏性落在 `Option`：`sem a s = none`
即“不可应用”（stale 锚 / 删不存在 / conflict），不再用 `Part`/`PFun`，零 Mathlib。

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

#### Application algebra in Lean

The persisted `Application` object stores enough data to witness Lean's
`Application` structure: the base `state`, the `action`, and a proof that the
action is applicable at that state. `target_state` is the cached value of
`Application.target app`, while `change` is the endpoint view used for
materialization and presentation.

形式定义见 [`docs/graft-kernel.lean`](../graft-kernel.lean) 的「核心模型/核心定义」段：
`Application`（字段 `base` / `action` / `valid`）、`applicable` / `Application.target` /
`composable` / `composeApplicable` / `composeApplication` / `targetCompose`。要点：

- `Application` 只存 `base` / `action` 加“可应用”证据 `valid : (sem action base).isSome`；
  输出端 `app.target` 是**派生量**（`noncomputable def Application.target`，对标 `List.length`），
  不存储，从而 `Application` 完全由 `(base, action)` 加该事实决定，无多义。（`sem` opaque，
  经它抽出的*数据*不能约简，故 `target` / `composeApplication` / `certifyComposed` 标
  `noncomputable`；命题与 `certify` 不受影响。）
- `composable app1 app2 := app1.target = app2.base` 是**有向**的，不对称；compose /
  revert 讨论方向时不要误以为可交换。
- `targetCompose` 是 **theorem 不是 axiom**：由 `sem_seq` + `Option.some_get` + bind 定义化简
  推出 `(composeApplication app1 app2 link).target = app2.target`。整个 kernel 的法则 axiom
  只留 `sem_id` / `sem_seq` / `satisfies_*` 三类（另加不可解释的签名 axiom）。

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
ApplicationId   one action + one base + one proof + cached after-state + endpoint view
ChangeId        endpoint view of Application; replay(base, ops) == target
CandidateId     local/private wrapper around Application + Constraint + provenance
PatchId         admitted/public Graft wrapper around Application + Constraint + admission metadata
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
