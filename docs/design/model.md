# Graft 设计 · 对象模型（§2.1–§2.4）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化内核见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 2. 对象模型

### 2.1 三层存储

Graft 把所有持久数据按「内容寻址性、同步性、可重建性」划分为三层：

```text
store/public/    immutable, content-addressed, syncable when workspace [sync] enabled
store/private/   immutable, content-addressed, never synced
store/derived/   rebuildable, content-addressed, locally cached
```


| 层       | 例子                                                                                                             | 是否同步                     | 丢失后是否可恢复              |
| ------- | -------------------------------------------------------------------------------------------------------------- | ------------------------- | ------------------- |
| public  | tree, action, application, change, constraint, plan, patch, evidence_refs(patch), relation, promotion, manifest, blob | 可选（由工作区 `[sync]` 决定） | 已同步时可从远端拉取 |
| private | candidate, evidence_refs(candidate)                                                                            | 否                         | 不能；由本地决定        |
| derived | evidence, verifier clean worktree cache                                                                        | 否                         | 可重跑验证器或重新物化   |


这一划分是目录布局原则，也是同步、垃圾回收和克隆规则的依据。`store/derived/` 可整目录 `rm -rf`；下次需要时按 `evidence_refs` 中记录的 ID 重建。

可变且持久的本地簿记数据（别名、远端同步进度、查询索引）不放在 `store/`，而放在工作区 `local/`；当前目录路由和本地仓库路径放在 `$GRAFT_HOME/registry.toml`。详见 [§3.2](./workspace.md) 和 [§12](./workspace.md)。目录名刻意不用 `state/`，以免与补丁理论中的 `State` / `StateId` 混淆。

### 2.2 身份标识方案

所有带类型对象使用统一形式：

```text
<kind>:<digest>
```

`digest` 默认 12 字符 blake3 hex。冲突时按需增长到 16 / 20 / full。CLI 接受 ≥ 6 字符前缀；歧义时报错并列出全部匹配。


| 对象                      | ID 形式                  | 文件位置                                             |
| --------------------------- | ---------------------- | ------------------------------------------------ |
| blob                        | `<blake3-hex>`         | `store/public/blob/<blake3-hex>`                 |
| tree                        | `tree:<digest>`        | `store/public/tree/<digest>.json`                |
| action                      | `action:<digest>`      | `store/public/action/<digest>.json`              |
| application                 | `application:<digest>` | `store/public/application/<digest>.json`         |
| change                      | `change:<digest>`      | `store/public/change/<digest>.json`              |
| constraint                  | `constraint:<digest>` | `store/public/constraint/<digest>.json`        |
| plan                        | `plan:<digest>`       | `store/public/plan/<digest>.json`              |
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

- `blob` 是裸 blake3，不带类型前缀；它的哈希就是内容。
- `action` 是对基础状态多态的纯语法抽象语法树；`application` 是 `(action, base, applicability_proof, target, change)` 的具体实例。两者都是内容寻址、不可变、公开的对象，正文结构见 §2.4。`candidate` / `patch` 的正文引用 `application:<digest>`，而不内嵌 `base`、`target` 或 `change`。详见 [§2.7](./admission.md)。
- `evidence_refs` 文件名是拥有者的 digest，没有自己的 ID。它是拥有者上的追加式索引，不参与 ID 体系。
- 旧前缀（`gr`_, `grc_`, `ev_`, `ch_`, `gt_`, `cf_`, `rel_`, `prm_`, `scr_`, `fv_`）已废止。遇到这类输入立即以 `[E_LEGACY_ID]` 失败并提示新形式。

### 2.3 类型论模型

从类型论看，Graft 不应把补丁建模成只属于单一基础状态的裸箭头 `Patch<A, B>`。更合适的做法是先区分核心状态迁移层和准入层：

```text
State                : Type
Action               : Type     # 对基础状态多态的变更意图 / 纯程序 AST
Applies(action, s)   : Type     # Action 可应用到某个具体 State 的原因
Application          : Type     # 绑定具体 base 后的 Action 实例
Constraint(app)        : Type     # 针对一个具体 Application 的证明义务
Evidence             : Type     # 运行时生成的 Constraint(app) witness
Admission(app)       : Type     # 必需约束已满足的本地决策
Candidate            : Type     # 本地私有的 Application 提议
Patch                : Type     # 已接纳且携带证明的 Application
Promotion            : Type     # 消费 Patch 的外部副作用
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

#### Lean 中的 Action 语义

形式定义以 [`formal/kernel.lean`](../../formal/kernel.lean) 为准。文档只保留对象边界：
`Action` 是可迁移的变更意图，`Application` 是绑定具体 base 和 applicability proof 的一次应用。

这里最关键的是：`Applies(a, s)` 不是 `bool`，而是一个可能有 inhabitant 的类型。若能构造 `p : Applies(a, s)`，说明 action 可以应用到该 state；若构造不出来，系统必须返回结构化失败原因。于是“一个 action 能应用的 base 是一个集合”可以写成：

```text
ApplicableBases(action) = Σ (state : State). Applies(action, state)
```

这不是在说一个已接纳补丁有多个基础状态，而是在说一个可迁移的 `Action` 可能有多个 `(base, proof)` inhabitant。每个 inhabitant 都会产生一次新的具体 `Application`。

也就是说，`Action` 对基础状态多态：它表达可迁移的变更意图，但不单独产生可信对象。`Application` 绑定具体基础状态：它把某个 `action`、某个具体 `base`、以及说明为什么可应用的 `proof` 一起端点化为 `base -> target`。

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
  EvidenceRefs(app, admission.required)
```

物理存储上，`Action` 与 `Application` 都作为一等、不可变、公开的对象持久化。`Candidate` / `Patch` 只是围绕 `ApplicationId` 的生命周期封装。补丁正文和 `evidence_refs` 可以分开存放；语义上它们共同表达“这个具体应用已被本地运行时接纳”。因此 `patch:<id>` 的 subject 必须固定到一个 `application:<id>`，否则内容寻址 ID、证据重建和同步都会失去稳定含义。

落到对象边界：

- `ActionId` 是共享变更意图，可以跨 base 迁移或重新应用。
- `ApplicationId` 是一次具体应用，绑定 action、base、applicability proof、target 与端点变更。
- `ScratchId` 是可变草稿状态图中的节点，用来构造或试探 `Action` / `Application`。
- `CandidateId` 是本地私有的 `Application` 提议。
- `PatchId` 是已接纳、可同步且携带证明的具体 `Application`。
- `PromotionId` 记录把某个 `Patch` 投影到外部目标的副作用结果。

因此迁移语义应是：

```text
migrate(action, new_base)
  -> proof : Applies(action, new_base)
  -> new Application
  -> 新 candidate/patch 实例
  -> 为新 Application 重跑 evidence
```

旧证据不能默认复用到新 `Application`，因为 `Constraint(Application)` 的 subject 绑定具体 `base`、`action`、`proof` 和 `target`。如果以后引入 action 级约束，也应建模成另一类 subject，而不是混用 application 级证据。

Constraint / Evidence 的边界：

```text
Constraint(Application) : Type
Evidence              : 运行时生成的 Constraint(Application) inhabitant
```

`EvidenceRecord` 不是约束源能自行构造的数学证明，而是 Graft 运行时生成的可重建、携带证明的数据：给定 `(Application, plan, verifier, execution_contract)`，本地能在隔离验证中重跑并得到同一 `EvidenceId`，就等价于重新构造了该约束 primitive 的 witness。

#### 为什么不从范畴论开始建模

范畴论里可以把类似结构画成 `span` 或 profunctor，但在实现设计上，类型论中的 `Applies(action, state)` 更直接：它把“是否适用”从隐含判断变成显式数据和证据，也更容易映射到 Rust 枚举、验证器、诊断信息和冒烟测试。

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

`span` 能说明“同一个 action 关联多组 base/target”，但它没有直接说明 proof 如何存储、失败如何分类、诊断信息如何返回。因此它适合作为解释图，不适合作为对象结构的主表达。

### 2.4 State、action、application、change

本节给出 §2.3 模型对应的一等持久对象结构。

#### State

Graft 中 patch 的“状态”指 `StateId`，已收窄为：

```rust
enum StateId {
    GraftTree(TreeId),          // tree:<digest>，Graft 内部 tree
    RepoTree(RepoBaseState),    // repo:<repo_id>@<treeish>#<resolved_tree_oid>，外部 git 锚点
    GitTree(String),            // local git treeish, resolved only at materialization boundary
}
```

`Conflict` 不再是 StateId 的变体；当前不建模一等冲突对象。

`TreeEntry = { path, hash, size, mode: FileMode }`，`FileMode = Regular | Executable | Symlink`。

#### Action 对象

`Action` 是对基础状态多态的程序，不是端点差异。它可以从不同 base 构造不同 `Application`，但每一次 application 都必须携带具体 `ApplicabilityProof`。

规范 `Action` DSL 是内部抽象语法树；CLI、pi-graft、hashline 编辑器或文本语法都只是前端。不要把某个界面输入格式当成 `ActionId` 的来源。

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

`EditText` 不直接存界面行号。它存规范化文本 hunks 和 anchor predicate：

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

`ActionId = hash(canonical(Action))`。规范化只做语法级规范化，不试图证明语义等价：

- path 是 UTF-8 相对路径，使用 `/` 分隔，禁止绝对路径、空段、`.` 和 `..`。
- object key 顺序、enum tag、整数、bytes/base64 表达固定；不保留注释或用户别名。
- blob 内容先做内容寻址，Action 中引用 `BlobId`。
- `Sequence` 展平嵌套 sequence；空 sequence 保留为 `Sequence([])`。是否拒绝空变更是 constraint/admission 策略，不是 Action parser 的默认门禁。
- 不跨路径排序 sequence。两个 action 是否可交换是更高层 relation/migration 问题，不参与 `ActionId` 相等性判断。
- 对同一 path 的相邻 edit 可由前端预合并，但 canonicalizer 不做依赖 base 的改写。

#### Applicability proof 与 Application 对象

`ApplicabilityProof` 是 `Applies(action, base)` 的可序列化 witness。它不单独获得带类型对象 ID，但规范 proof digest 进入 `ApplicationId`。proof 必须足以让 daemon 对同一 `(action, base_state)` 重放 applicability 检查并得到同一 target；不能只存 `ok: true`。

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

以上枚举表示语义形状；实现可以拆成更细的规范记录，但必须保持：成功 proof 是一等数据，失败是结构化诊断信息。Applicability 失败必须返回结构化原因：

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
    change:              ChangeId,      // 该 application 的端点视图
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

`Application` 的核心完整性要求：

```text
apply(action, base_state, applicability_proof) == target_state
replay(base_state, change.ops) == target_state
change.base_state == base_state
change.target_state == target_state
```

任何一条失败都是 `[E_CHANGE_INTEGRITY]`；这种对象在语义上类型不成立，不能被 admit、validate 或 promote。

#### Lean 中的 Application 代数

应用组合和端点派生的形式定义以
[`formal/kernel.lean`](../../formal/kernel.lean) 为准。实现层只需保持：
`Application` 存 base / action / applicability proof，`target_state` 是派生 target 的缓存。

#### Change 对象

`Change` 是 `Application.endpoint_diff()` 的规范端点视图。它可以用于物化、推广、验证工作树物化、差异展示和关系变换，但不是 `Action` 的完整语义。

```rust
struct Change {
    base_state:   StateId,
    target_state: StateId,
    ops:          Vec<ChangeOp>,    // 规范顺序
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
ActionId        跨 base 稳定；可迁移或重新应用
ApplicationId   一个 action + 一个 base + 一个 proof + 缓存后的状态 + 端点视图
ChangeId        Application 的端点视图；replay(base, ops) == target
CandidateId     Application + Constraint + provenance 的本地私有封装
PatchId         Application + Constraint + admission metadata 的已接纳公开封装
```

`ChangeSet { files: Vec<FileChange> }`（端点差异）退化为 `Change.endpoint_diff()` 的派生视图，仅用于展示。关系命令可用 Change 端点代数作为实现手段，但输出仍必须是新的 `action:<id>` + `application:<id>` + `candidate:<id>`，不能只产出裸 `change:<id>`。

待办：当前关系变换实现更接近端点压缩；若以后需要类似 Git 的 action 追加语义，应保留顺序 action/proof 历史，把 `change` 继续作为端点视图或缓存，而不是由 `change` 反向决定 action。

#### Hashline 降级

`LINE#HASH` / hashline 是界面锚点协议，不是 `Action` 表示。

降级流程：

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

- `LINE#HASH` 的行号只用于定位当前 file view；不进入 `ActionId`。
- line hash / selected text / context 降级为 `AnchorPredicate`；如果 anchor 不唯一或已漂移，返回 `AnchorNotFound` / `AnchorAmbiguous` / `AnchorDrift`。
- `scratch:<digest>` 是 daemon scratch graph 节点，不是 `ActionId`。scratch op chain 可以用于构造 `Action`，但最终 candidate 必须绑定具体 `Application`。
- rename 在 hashline/scratch 界面中仍可用 delete+write 表达；若前端能证明是同一 blob 的移动，可降级为 `Action::Rename`，否则保留为 delete/create sequence。
