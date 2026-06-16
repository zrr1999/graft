# Graft 设计 · 准入与关系（§2.7–§2.8）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化内核见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 2. 对象模型（续）

### 2.7 Candidate、patch、admit

Lean 内核把已接纳且携带证明的 application 称为 `Graft`。

形式定义见 [`formal/kernel.lean`](../../formal/kernel.lean) 的「核心定义」段：`Graft`（`application` + `constraint` + `valid : satisfies application constraint`）、`certify`、`certifyComposed`。`admit` 是运行时的认证步骤；compose 路径的 `certifyComposed` 要求调用方提供复合后 application 的新 `satisfies` proof，不从分量自动传播。该文件末尾保留三条延后待办：逆语义、组合下的约束传播（`Stable` 传递性），以及状态三分 / 是否引入否定（wellformed / consistent / resolved）。三者都是内核上的增量项，而非对现有定义的修改。

在存储层，`candidate:<id>` 是本地提议，携带一个 `Application` 以及它预期满足的 `Constraint`。`patch:<id>` 是已认证 `Graft` 的公开生命周期封装：同一个具体 `Application`、已接纳的 `Constraint`，以及概括运行时证明决策的准入元数据。

```rust
struct Candidate {
    id:          CandidateId,       // candidate:<digest>
    application: ApplicationId,     // concrete application:<digest>
    constraint:  Constraint,        // declared obligation for this application
    provenance:  Provenance,
}
// CandidateId seed = body fields；evidence_refs / local created_at 不在正文

struct Patch {
    id:          PatchId,           // patch:<digest>
    application: ApplicationId,     // concrete application:<digest>
    constraint:  Constraint,        // satisfied declared obligation
    provenance:  Provenance,
    admission:   AdmissionSummary,  // no host/time fields
}
// PatchId seed = body fields；evidence_refs / local admitted_at 不在正文
```

`base_state`、`target_state` 与 `change` 不再重复内嵌在 candidate / patch 正文中；它们从 `store/public/application/<digest>.json` 读取。这样 `candidate:<id>` / `patch:<id>` 只是生命周期封装，subject 始终是同一个不可变 `application:<id>`。

#### Candidate = 仅本地

Candidate 是私有提议，**不同步**。它存放在 `store/private/candidate/`。其他 clone 看不到、不能评审、不能继承。

评审在 patch 层分布式发生：admit 之后 patch 被同步出去，每个 clone 自行决定是否在自己的工作流中接受该 patch。本地“不接受”的表达方式是不让 alias 指向它、不 materialize，并在需要时从 incoming 默认列表隐藏。

#### admit = 跨层搬移

admit 的物理含义是把对象**从 private 跨子目录搬到 public**：

```
store/private/candidate/<C-digest>.json
   → store/public/patch/<P-digest>.json     # body 变化（schema 不同），ID 重算

store/private/evidence_refs/<C-digest>.json
   → store/public/evidence_refs/<P-digest>.json  # 文件名 = 新 PatchId
                                                 # body 中 owner 字段更新
```

操作完成后 candidate **在文件系统上消失**；它不是历史记录。要追溯 patch 来自哪个 candidate，依赖 `patch.provenance` 字段，其中包含原 candidate ID 等元数据。

#### admit 失败模式

```text
[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]
  required Constraint 的某 primitive 找不到 passed evidence。
  原因可能是：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
            或者本地从未跑过该 verifier。
  提示：graft patch validate candidate:C --expect <constraint>

[E_CONSTRAINT_DRIFT]
  required primitive 的 PlanId 与 candidate.constraint 中对应 primitive 不一致。
  原因是 constraints.roto 中对应顶层 constraint 函数在 candidate 创建后被修改或改名。
  解决：要么用现行 PlanId 跑新 evidence，要么 revert constraints.roto 改动。
```

### 2.8 Relation 与 promotion

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

Relation 是已接纳对象的可派生性历史事实记录，不参与 admission 本身。`graft patch show patch:c` 会展开 relation：`patch:c is compose(patch:a, patch:b)`。Relation 不指向 private candidate，以免同步出本地私有对象引用。

#### Promotion

Promotion 是把 patch 投影到显式目标的事件记录。目标可以是配置的外部 Git repo/ref（`[promote_targets.<name>]`），也可以是命令行 `--to <branch>` 指定的本地 Git ref：

```rust
struct PromotionRecord {
    id:           PromotionId,        // promotion:<digest>
    patch_id:     PatchId,
    target:       String,             // target:<name>:<ref> 或 branch/release/pr label
    dry_run:      bool,
    status:       String,
    effects:      Vec<PromotionEffect>, // external writes actually attempted/observed
    promoted_at:  String,             // 本地元数据；不是证据
}
```

`graft patch promote patch:X --to <target-or-branch>` 触发：

1. 若 `<target-or-branch>` 命中 `graft.toml [promote_targets.<target>]`，解析其 `path`、`branch` 与 `required`；否则按显式分支/PR/release 目标处理。
2. 对 `[promotion].required`、配置 target 的 `required` 与 CLI `--require <constraint>` 重新跑 admission 查询。
3. 从 `patch.application.target_state` 构造目标 commit/ref；只有 `--yes` 会真正写外部 Git repo/ref，默认只试运行。
4. apply 时在 `store/public/promotion/<digest>.json` 落一条记录。
5. 当前工作区若启用同步，下次同步会把这条 promotion record 推到 Graft 远端。

Promotion 是显式副作用边界：除了 `graft patch promote --yes`，Graft 命令不把 patch 输出到外部目标。Promotion effects 可记录 `GitRefUpdate`、`LocalFileWrite`、`RemotePush` 等外部写入事实；它们证明“patch 被投影到哪里”，不证明 `Constraint(Application)`，也不改变任何 `EvidenceId`。

`[promote_targets.<name>]` 在 `graft.toml` 配置，`required` 在该处声明（详见 [§11](./reference.md) 和 [§12](./workspace.md)）。

---
