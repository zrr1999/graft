# Graft 设计 · 准入与关系（§2.7–§2.8）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`../graft-kernel.lean`](../graft-kernel.lean)。

### 2.7 Candidate, patch, admit

The lean kernel calls an admitted proof-carrying application a `Graft`.

形式定义见 [`docs/graft-kernel.lean`](../graft-kernel.lean) 的「核心定义」段：
`Graft`（`application` + `constraint` + `valid : satisfies application constraint`）、
`certify`、`certifyComposed`。`admit` 是 runtime 的 certification 步骤；compose 路径的
`certifyComposed` 要求调用方提供复合后 application 的新 `satisfies` proof（不从分量
自动传播）。该文件末尾保留三条延后 TODO：**inverse semantics**、**constraint
propagation under composition（`Stable` 传递性）**与**状态三分 / 是否引入否定（wellformed /
consistent / resolved）**，三者都是 kernel 上的加法项而非修改。

In storage, `candidate:<id>` is the local proposal that carries an
`Application` plus the `Constraint` it is expected to satisfy. `patch:<id>` is
the public lifecycle wrapper for the certified `Graft`: the same concrete
`Application`, the admitted `Constraint`, and admission metadata summarizing the
runtime proof decision.

```rust
struct Candidate {
    id:          CandidateId,       // candidate:<digest>
    application: ApplicationId,     // concrete application:<digest>
    constraint:  Constraint,        // declared obligation for this application
    provenance:  Provenance,
}
// CandidateId seed = body fields；evidence_refs / local created_at 不在 body

struct Patch {
    id:          PatchId,           // patch:<digest>
    application: ApplicationId,     // concrete application:<digest>
    constraint:  Constraint,        // satisfied declared obligation
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
[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]
  required Constraint 的某 primitive 找不到 passed evidence。
  原因可能是：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
            或者本地从未跑过该 verifier。
  提示：graft patch validate candidate:C --expect <property>

[E_PROPERTY_DRIFT]
  required primitive 的 PropertyId 与 candidate.constraint 中对应 primitive 不一致。
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

`[promote_targets.<name>]` 在 `graft.toml` 配置，`required_properties` 在该处声明（详见 [§11](./reference.md) 和 [§12](./workspace.md)）。

---
