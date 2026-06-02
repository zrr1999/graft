# Graft core design

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
edit
  -> candidate
  -> property obligations
  -> verifier evidence
  -> admitted patch
  -> external git promotion
```

核心判断：

```text
Evidence ⊢ Property(patch)
```

一个 patch 不因为「agent 说完成了」而可信，而因为它有声明的 property 和可验证的 evidence。

### 1.3 与 Git 的关系

Graft 不替代 Git，也不把 cwd 里的 Git 仓库当成 workspace 本体。

- workspace 是 `$GRAFT_HOME` 或显式 `graft init` 管理的用户级对象；cwd 只是命令路由的 attach key。
- cwd 是否是 Git 仓库与 Graft workspace 概念正交；Graft 默认不写 cwd。
- 远端 Git 仓库是 Graft 的存储分区，没有"main 视图"，托管平台浏览体验由 Graft 自己另外提供（不在本版 scope）。
- 显式 `graft promote` 可把某个 patch 投影到任意 target：远端 Git ref、本地 Git ref，或本地非提交文件。这是唯一会把可信 patch 输出到外部世界的路径。

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
- 一等的 artifact 对象（evidence 一律假设可重现）。
- 持久化或跨 host 的 verifier 输出归档。
- 中央 review gate（admission = 本地认可，review 在 patch 层分布式发生）。
- main / HEAD 等"默认视图"概念。
- 任何形式的 host-bound state（PatchId / EvidenceId 不携带 hostname / timestamp）。

---

## 2. Object model

### 2.1 Three storage tiers

Graft 把所有持久数据按"内容寻址性 + sync 性 + 可重建性"划分为三层：

```text
store/public/    immutable, content-addressed, fully synced
store/private/   immutable, content-addressed, never synced
store/derived/   rebuildable, content-addressed, locally cached
```

| 层          | 例子                                                                  | sync？ | 丢了能恢复？     |
| ---------- | ------------------------------------------------------------------- | -----: | --------- |
| public     | tree, change, property, patch, evidence_refs(patch), relation, promotion, manifest, blob |     是 | 从 remote 拉 |
| private    | candidate, evidence_refs(candidate)                                 |     否 | 不能（local 决定） |
| derived    | evidence                                                            |     否 | 重跑 verifier |

这一划分是 layout 原则，也是 sync / gc / clone 各自规则的依据。`store/derived/` 可整目录 `rm -rf`，下次需要时按 `evidence_refs` 中记录的 ID 重建。

mutable durable state（cwd 指针、alias、remote 同步进度）不在 `store/`，在 `state/`，详见 §3.2。

### 2.2 Identity scheme

所有 typed object 用统一形式：

```text
<kind>:<digest>
```

`digest` 默认 12 字符 blake3 hex。冲突时按需增长到 16 / 20 / full。CLI 接受 ≥ 6 字符前缀；歧义时报错并列出全部匹配。

| Object                        | ID 形式                  | 文件位置                                            |
| ----------------------------- | ---------------------- | ----------------------------------------------- |
| blob                          | `<blake3-hex>`         | `store/public/blob/<blake3-hex>`                |
| tree                          | `tree:<digest>`        | `store/public/tree/<digest>.json`               |
| change                        | `change:<digest>`      | `store/public/change/<digest>.json`             |
| property                      | `property:<digest>`    | `store/public/property/<digest>.json`           |
| patch                         | `patch:<digest>`       | `store/public/patch/<digest>.json`              |
| evidence_refs (patch owner)   | by `<patch-digest>`    | `store/public/evidence_refs/<patch-digest>.json` |
| relation                      | `relation:<digest>`    | `store/public/relation/<digest>.json`           |
| promotion                     | `promotion:<digest>`   | `store/public/promotion/<digest>.json`          |
| manifest                      | `manifest:<digest>`    | `store/public/manifest/<digest>.json`           |
| candidate                     | `candidate:<digest>`   | `store/private/candidate/<digest>.json`         |
| evidence_refs (cand. owner)   | by `<cand-digest>`     | `store/private/evidence_refs/<cand-digest>.json` |
| evidence                      | `evidence:<digest>`    | `store/derived/evidence/<digest>.json`          |
| scratch                       | `scratch:<digest>`     | daemon memory only                              |
| view                          | `view:<digest>`        | response only                                   |

注：

- `blob` 是裸 blake3，不带 typed 前缀——它的 hash 就是内容。
- `evidence_refs` 文件名是 owner 的 digest，没有自己的 ID。它是 owner 上的外挂 append-only 索引，不参与 ID 体系。
- 旧前缀（`gr_`, `grc_`, `ev_`, `ch_`, `gt_`, `cf_`, `rel_`, `prm_`, `scr_`, `fv_`）已废止。遇到这类输入立即以 `[E_LEGACY_ID]` 失败并提示新形式。

### 2.3 State, change, op list

Graft 中 patch 的"状态"指 `StateId`，已收窄为：

```rust
enum StateId {
    Tree(TreeId),               // tree:<digest>，Graft 内部 tree
    Repo(RepoBaseState),        // repo:<id>@<treeish>#<resolved_oid>，外部 git 锚点
}
```

`Conflict` 不再是 StateId 的变体（v1 不建模 conflict）。

`Change` 是 base state 到 target state 的规范变换。canonical body 是 op list：

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

`TreeEntry = { path, hash, size, mode: FileMode }`，`FileMode = Regular | Executable | Symlink`。

`ChangeSet { files: Vec<FileChange> }`（endpoint diff）退化为 `Change.endpoint_diff()` 的 derived view，仅用于展示。`Change::compose / migrate / reverse` 在 op list 上工作。

### 2.4 Property: alias 和 ContentId 解耦

Property 是单个 atom 级 verifier 配置的内容寻址对象：

```rust
struct PropertyDef {
    kind: PropertyKind,                 // Builtin | Command | ...
    spec: PropertyKindSpec,             // command / check / timeout / env / ...
}

// PropertyId = blake3(canonical(kind, spec))
// 注意：name 不进 hash
```

Property 的源头是用户维护的 `properties/<Name>.toml` 文件：

```text
properties/
  ValidPatch.toml
  CargoTestsPass.toml
  CargoFmtClean.toml
```

文件名（不含 `.toml`）是 alias。文件内容是 `kind / spec`，**不写 name 字段**。

#### 解析

```text
ValidPatch.toml  --[parse]-->  PropertyDef
                 --[hash]-->   PropertyId
```

每个 alias 解析得到的 `PropertyId` 写入 `graft.lock` 的 `[properties.<Name>]` 节缓存（详见 §3.3）。

#### 不变量：alias 解耦

```
Invariant 2.4.1  (PropertyAliasDecoupling)
  Evidence 引用 property:<digest>，不引用 alias name。
  properties/<Name>.toml 是 alias，决定 CLI 解析 'Name' 时指向哪个 PropertyId。

  操作 → 影响：
    rename properties/A.toml -> properties/B.toml:
      PropertyId 不变。所有 evidence 仍然有效。当前生效 alias 从 'A' 变 'B'。
    edit properties/A.toml 的 spec:
      PropertyId 漂移到 X'。alias 'A' 现在指 X'。引用旧 X 的 evidence 不再
      满足"当前 admission 要求 A"，但对象仍在 store 中。
    delete properties/A.toml:
      alias 'A' 不存在。CLI 解析 'A' 失败。但 PropertyId X 下的 property
      和 evidence 仍在 store 中，graft show / graft search property:X 仍可查。
```

也就是 **`properties/*.toml` 是 alias 视图，PropertyId 是真相**。

#### Admission 表达式

`PropertyExpr` 是 admission 表达式，不是 property 配置：

```rust
enum PropertyExpr {
    True,
    Atom { id: PropertyId, name: Option<String> },   // name 仅展示
    And  { terms: Vec<PropertyExpr> },
}
```

`PropertyExpr::Atom` 必须携带已解析的 PropertyId，否则同一个 candidate body 在 alias 漂移之后语义会漂移，违反 content-addressed 不变性。

`PropertyExpr` 出现在：

- `candidate.expected`
- `patch.properties`
- `[admission].base_properties` of graft.toml
- `[promote_targets.*].required_properties` of graft.toml

### 2.5 Evidence

#### 模型

Evidence 是 `(change, property)` 在某次 verifier 运行下的结果：

```rust
struct EvidenceRecord {
    id:        EvidenceId,                      // blake3(canonical(seed))
    change:    ChangeId,
    property:  PropertyId,
    verifier:  String,                          // 例如 "valid_patch" / "cargo-test"
    result:    EvidenceResult,                  // passed | failed | unknown | skipped
    created_at: String,
}
// EvidenceId seed = (change, property, verifier, result, ...canonical fields)
// 注意：seed 不含 hostname、timestamp、candidate/patch id。
```

#### 不变量：Evidence reproducibility

```
Invariant 2.5.1  (EvidenceContentAddressing)
  EvidenceId 完全由 (change, property, verifier, result) 决定，不绑定运行环境。

  推论：
    在 host A 运行得到 evidence:E1
    在 host B 运行同 (change, property, verifier)，结果一致 ⇒ 同 ID = E1
    结果不一致 ⇒ 不同 ID（典型例子：host A passed, host B failed）

  这让"本地重建 evidence"成为内容寻址 hash 匹配，无需跨 host 信任。
```

```
Invariant 2.5.2  (EvidenceReproducibility)
  Evidence 假设可重现：给定 (change, property)，verifier 在隔离 worktree 中
  跑必然给出相同 result，与运行 host 无关（相同 verifier 二进制版本下）。

  非假设场景（如 reference hardware benchmark）不在 v1 scope。
  任何依赖不可重现状态的 verifier 必须自行约束输出（如把 host fingerprint
  纳入 result enum），让结果差异表现为不同 EvidenceId。
```

#### 隔离运行

Verifier **永远在 trial worktree 中运行**：

```text
1. 在 .graft/run/trials/<run-id>/ 物化 evidence 涉及的 base_state
2. 应用 change，得到 target_state
3. 在该 worktree 中按 property.spec 跑 verifier
4. 收集 result（exit code / 命令输出 / builtin check 返回值）
5. 写 evidence:<digest>.json 到 store/derived/evidence/
6. 删除 trial worktree
```

verifier 跑过程**不读 cwd**。Evidence 的输入完全由 evidence body 中的 `change` 决定；这条让 evidence 跨 host 可重现，也让 cwd dirty 状态不影响 evidence 计算。

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

`evidence_refs` 是 append-only：admit 复制、post-admit `graft validate patch:...` 追加。owner body 永久不可变。

#### Sync 模式

evidence sync 的核心设计：**body 不 sync，refs sync**。

```
sync over the wire:
  store/public/evidence_refs/         ✓
  store/derived/evidence/             ✗（不传输 evidence body）

local rebuild:
  fresh clone 拿到 patch + evidence_refs，但 evidence body 缺失
  graft show patch:X 看到 "ValidPatch ✓ (not yet locally verified)"
  graft validate patch:X --expect ValidPatch
    -> 在隔离 worktree 重跑 verifier
    -> 算出 evidence:E
    -> 检查 E ∈ evidence_refs[patch:X].evidence
       是 → "复现成功"，evidence body 写入 store/derived/evidence/E.json
       否 → "本地 host 行为与远端 attestation 不一致"，evidence:E' 是新增条目
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
                    E.change == C.change
                    AND E.property == id
                    AND E.result == passed
      And{ts}  -> ∀ t ∈ ts: satisfy(t)

  if all satisfy:
    move candidate body to patch body (re-hashed; new PatchId)
    move evidence_refs[C] to evidence_refs[P]  (rename owner field, recompute filename)
    delete candidate (no leftover)
```

admit 不复制 evidence body——只复制 evidence ID 列表。一份 evidence 同时被 candidate 和 patch 引用是常态。

注意 admission 查询 `E.result == passed` 是对 evidence body 的查询；本地需要拿到 evidence body 才能算。如果 evidence body 不在 `store/derived/evidence/`（远端 attest 但本地未 rebuild），admit fail loud，提示 `graft validate <C> --expect <Property>`。

### 2.6 Candidate, patch, admit

```rust
struct Candidate {
    id:           CandidateId,         // candidate:<digest>
    base_state:   StateId,
    target_state: StateId,
    change:       ChangeId,
    expected:     Vec<PropertyExpr>,
    provenance:   Provenance,
}
// CandidateId seed = body fields；evidence_refs 不在 body

struct Patch {
    id:           PatchId,             // patch:<digest>
    base_state:   StateId,
    target_state: StateId,
    change:       ChangeId,
    properties:   Vec<PropertyExpr>,   // 满足声明
    provenance:   Provenance,
    admitted_at:  String,
}
// PatchId seed = body fields；evidence_refs 不在 body
```

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
  提示：graft validate candidate:C --expect <Property>

[E_PROPERTY_DRIFT]
  required atom 的 PropertyId 与 candidate.expected 中对应 atom 不一致。
  原因是 properties/<Name>.toml 在 candidate 创建后被修改。
  解决：要么用现行 PropertyId 跑新 evidence，要么 revert properties 改动。
```

### 2.7 Relation, promotion

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

例：`compose(patch:a, patch:b) -> patch:c` 写一条 `Relation { kind: Compose, inputs: [a, b], outputs: [c] }`。

Relation 是 derivability 历史的事实记录，不参与 admission。`graft show patch:c` 会展开 relation：`patch:c is compose(patch:a, patch:b)`。

#### Promotion

Promotion 是把 patch 投影到显式 target 的事件记录。target 可以是远端 Git ref、本地 Git ref，或本地非提交文件：

```rust
struct Promotion {
    id:           PromotionId,        // promotion:<digest>
    patch:        PatchId,
    target:       String,             // [targets.<name>] alias
    target_kind:  String,             // remote-push | local-git-commit | local-file
    target_ref:   Option<String>,     // git ref，local-file 时为空
    output_id:    String,             // commit oid / file content hash / target-specific id
    promoted_at:  String,
    by:           String,
}
```

`graft promote patch:X --target <target>` 触发：

1. 解析 `graft.toml [targets.<target>]`。
2. 对 target 要求的 properties 重新跑 admission 查询。
3. 按 target kind 执行投影：remote-push 写远端 ref，local-git-commit 写本地 Git commit/ref，local-file 写本地文件。
4. 在 `store/public/promotion/<digest>.json` 落一条记录。
5. 当前 workspace 若启用 sync，下次 sync 把这条 promotion record 推到 Graft remote。

Promotion 是显式 side-effect 边界：除了 `graft promote`，Graft 命令不把 patch 输出到外部 target。

`[targets.<name>]` 在 `graft.toml` 配置，`required_properties` 在该处声明（详见 §11 和 §12）。

---

## 3. Workspace layout

### 3.1 cwd

```text
cwd/
  graft.toml              # 项目定义，进 snapshot
  graft.lock              # 派生锚（properties + repos），不进 snapshot
  properties/
    ValidPatch.toml       # 进 snapshot
    CargoTestsPass.toml
    ...
  src/                    # workspace files，进 snapshot
  README.md
  ...
```

约束：

- 这里的 `cwd/` 只描述 `graft init` 创建的 **local workspace root**。一般命令的 cwd 可以是任意目录；它通过 §12 的 lookup / routes 解析到 workspace。
- cwd 根目录允许存在 `.git/`。cwd 是否是 Git 仓库只影响 attach 时能否自动登记 repo，不影响 workspace 是否存在。
- snapshot 包含什么：
  - 包含：`graft.toml`、`properties/*.toml`、所有普通工作区文件。
  - 不包含：`graft.lock`（派生）、`.graft/`（本地状态）、`.git/`（外部 VCS）、`.gitignore` 类工具忽略的常见生成物。
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

  state/                            # mutable durable, atomic write
    aliases/
      candidates/<name>             # 单文件: candidate:<digest>
      patches/<name>                # 单文件: patch:<digest>
      promotions/<name>             # 单文件: promotion:<digest>
    remotes/<remote>/
      last_synced                   # manifest:<digest>
      transport.cache/              # bare git odb (transport-only)

worktrees/
  <patch-or-tree-id>/                # explicit materialize output
```

全局 daemon 的 process state 不在 workspace 内，而在 `$GRAFT_HOME/run/`（§12）。

四个 `.graft/` 顶级目录 + workspace-level `worktrees/`，每个角色单一：

| 顶级               | 内容性质                       | 备份  | sync | 启动清理     |
| ---------------- | -------------------------- | --: | --: | -------- |
| `config.toml`    | 用户可改本地配置                   |   是 |   否 | 否        |
| `store/public/`  | 内容寻址不可变，按 workspace sync policy 决定是否 sync |   是 | 可选 | 否        |
| `store/private/` | 内容寻址不可变，local-only        |   是 |   否 | 否        |
| `store/derived/` | 可重建本地数据                    |   可选 |   否 | 可选       |
| `state/`         | 可变指针，atomic write          |   是 |   否 | 否        |
| `worktrees/`     | explicit materialize 输出      |   否 |   否 | 可清理     |

#### 写规则

- `store/public/` `store/private/`：daemon 写一次后内容永不修改（content-addressed）。删除只通过 gc。
- `store/derived/`：daemon 重建时写入；用户可整目录 `rm -rf` 而不破坏正确性。
- `state/`：atomic rename 写入。每个文件短小，单次 read/write 即一致快照。
- `evidence_refs/` 是 append-only：daemon 通过 read → append → atomic rename 实现。同一 owner 的 refs 文件不并发写（daemon 串行化）。
- `worktrees/`：materialize 输出目录；daemon 可按 gc/doctor 策略清理过期目录。
- `$GRAFT_HOME/run/`：全局 daemon 启动时清理 `trials/` `tmp/` 等 ephemeral，重建 `daemon.sock` `daemon.pid`。

#### sync 范围

```
sync = if workspace [sync] is enabled:
         mirror store/public/ to remote refs/graft/{facts, blobs, manifests}
       else:
         no-op for distribution; admission still writes store/public/ locally
       (详细映射见 §6 和 §12)
```

`store/private/` `store/derived/` `state/` `worktrees/` `$GRAFT_HOME/run/` 永不 sync。`ws:default` 强制永不 sync。

#### gc 范围

reachability roots：

```
state/aliases/{candidates,patches,promotions}/*  解析得到的对象 ID
state/cwd 中引用的 tree/patch ID
当前 properties/*.toml 解析得到的 PropertyId 集合
daemon 内存中持有 lease 的 active scratch
```

从 roots walk，标记可达对象。`store/{public,private}/` 中不可达的对象在 gc 时删除（`store/derived/` 整目录可清，按需重建）。详见 §9。

### 3.3 graft.lock 双锚

`graft.lock` 是派生缓存兼解析锚，不进 snapshot，不跨 clone。它由两类条目组成，schema 同质。

#### 形态

```toml
# @generated by graft; do not edit by hand
version = 1

# Properties: alias -> PropertyId
[properties.ValidPatch]
id         = "property:374d33205102"
spec_hash  = "..."

[properties.CargoTestsPass]
id         = "property:044b52a36644"
spec_hash  = "..."

# Repos: alias + treeish -> resolved oid
[repos.linux-stable]
treeish      = "v6.6"
resolved_oid = "abc123def4567890..."
resolved_at  = "2026-06-01T08:30:00Z"

[repos.cpython]
treeish      = "3.13.0"
resolved_oid = "def987654321..."
resolved_at  = "2026-06-01T08:30:00Z"
```

#### Properties 节

来源：`properties/<Name>.toml`。每次 graft 命令启动时 daemon 解析：

```text
properties/Foo.toml -> PropertyDef -> PropertyId
                    -> spec_hash
```

如果 `graft.lock` 中 `[properties.Foo].spec_hash` 与当前算出不一致 → drift detected → daemon 自动刷新（property 是纯派生，无外部世界依赖，refresh 安全）。

#### Repos 节

来源：`graft.toml` 中 `[repos.<id>]`。每个 repo 在 graft.toml 给 `treeish`（可以是 branch / tag / oid）：

```toml
# graft.toml
[repos.linux-stable]
url     = "https://git.kernel.org/.../linux-stable.git"
treeish = "v6.6"
```

Patch 引用外部 repo 时，`base_state = Repo { repo: "linux-stable", oid: <resolved_oid from lock> }`。**永远引用 lock 里的 oid，不引用 treeish**。

##### graft repo lock / update

```bash
graft repo lock                     # 解析所有 repos 的 treeish 到 oid
                                    # 已 lock 的不重解（除非 treeish 变了）
graft repo lock <name>              # 单独解析一个

graft repo update <name>            # 强制重新解析；允许 oid 漂移
graft repo update --all
```

#### 漂移检测

```text
[E_REPO_LOCK_DRIFT]
  graft.toml [repos.X].treeish 与 graft.lock [repos.X].treeish 不一致。

  示例：
    graft.toml = "v6.7"
    graft.lock = "v6.6"  resolved_oid = "abc..."

  解决：
    graft repo update X     # 重新解析 v6.7 到新 oid，覆盖 lock
    或 revert graft.toml 改动
```

```text
[E_PROPERTY_LOCK_DRIFT]
  properties/X.toml 的 spec_hash 与 graft.lock [properties.X].spec_hash 不一致。

  daemon 自动 refresh，不 fail。
  这是因为 property 是纯派生（无外部世界依赖），refresh 不会引入信任问题。
  注意：refresh 后 PropertyId 可能漂移，旧 evidence 不再满足新 admission。
```

#### 不变量

```
Invariant 3.3.1  (NoDriftingExternalReferences)
  patch.base_state 中的 repo:<id>@<treeish>#<oid>，oid 字段必须存在且
  必须等于 graft.lock [repos.<id>].resolved_oid（commit 创建时刻的快照）。

  treeish 字段在 hash 之外，仅供展示。
```

```
Invariant 3.3.2  (LockSchemaUniformity)
  graft.lock 的所有顶级 entry 都是 (alias, content-addressed-id) 映射，
  无运行状态、无 sync 进度、无 cwd 信息。

  sync 进度：state/remotes/<name>/last_synced
  cwd 指针：state/cwd
```

### 3.4 Snapshot 与 ignore 规则

cwd 物化时 graft 把 `state/cwd` 引用的 tree 写入 cwd。捕获 cwd 成 candidate 时反过来。哪些路径属于 snapshot 由 ignore 规则决定：

```
内置排除（不可关闭）：
  .graft/                         # graft 自身
  .git/                           # 永远禁止；存在即 fail，不只是 ignore
  graft.lock                      # 派生锚

用户可配（graft.toml [snapshot.ignore]）：
  patterns = ["target/**", "node_modules/**", "*.log"]
```

`[snapshot.ignore]` 模式语法是 gitignore-compatible 子集。

具体匹配规则、symlink 处理、大文件阈值等实现细节在 §10 invariants 列出。

---

## 4. Lifecycle

Graft 把变更生命周期拆成三道关，每一关都有显式动词和门槛：

```text
edit (cwd / scratch)
  -> graft create / scratch promote
candidate                       store/private, local-only
  -> graft validate             produces evidence (store/derived)
  -> graft admit                gates: [admission].base_properties
patch                           store/public, synced via sync
  -> graft promote              gates: [promote_targets.<t>].required_properties
external git ref / PR           outside Graft's domain
```

每道关的语义：

- **create**：编辑变可寻址，无外部副作用。门槛仅 sanity（非空 change，非空 cwd 或显式 `--from graft:empty`）。
- **admit**：candidate 升 patch，等于「我（本地）愿意把这件事公开给团队」。门槛 `[admission].base_properties`。**这不是 review gate**——review 在 sync 之后由每个 clone 自己决定。
- **promote**：patch 进入下游 Git，等于「它能 ship 给非 Graft 用户」。门槛 `[promote_targets.<target>].required_properties`。

admit ≠ review。这点在分布式协作中至关重要：远端 patch 进入本地 store 不代表本地必须采用，alias 是否指向它由本地决定。

### 4.1 graft create: cwd → candidate

```bash
graft create [--from <base>] [--expect <Property>...] [--message <msg>]
```

行为：

1. 解析 base：
   - `--from <id>` 显式指定（`tree:...` / `patch:...` / `repo:...` / `graft:empty`）。
   - 不指定时按 `state/cwd` 推断。
   - cwd 是 git 仓库时（理论上不应该，因为禁 `.git/`），不接受 git treeish 作为 base；必须显式 `--from`。
2. 扫描 cwd（按 §3.4 ignore 规则），构造 target tree。
3. 计算 base → target 的 op list，得到 `Change`。
4. 拒绝空 change：`[E_EMPTY_CHANGE]`。
5. 写入：
   - `store/public/blob/`（新增内容）
   - `store/public/tree/`（target tree）
   - `store/public/change/`
   - `store/private/candidate/<C-digest>.json`
   - `store/private/evidence_refs/<C-digest>.json`（空 evidence 列表）
6. 不写 `state/aliases/candidates/`；alias 由用户后续 `graft alias` 设定（v1 简化：`--alias <name>` 同时设定）。

注意 blob/tree/change 都进 public——它们是内容寻址不可变事实，将来 admit 后这些对象就是 patch 的一部分，提前进 public 不浪费。candidate 自己进 private。

### 4.2 graft validate: 跑 verifier 产 evidence

```bash
graft validate <id> [--expect <Property>...] [--all-expected]
graft validate                           # 无参数版本：从 cwd 推断
```

`<id>` 可以是 `candidate:...`、`patch:...`，或 `change:...`。

#### 流程

```text
1. 解析 <id> 拿到目标 change。
2. 解析 --expect 列表为 PropertyId（通过 properties/<Name>.toml 当前 alias 表）。
   --all-expected: 取 candidate.expected / patch.properties 中所有 atom。
3. 对每个 (change, property)：
   a. 计算 evidence body 的 seed（顺序: change, property, verifier, result placeholder）。
      result 还没跑出来，所以这步只确定 (change, property, verifier) 部分。
   b. 在 .graft/run/trials/<run-id>/ 物化 base_state，应用 change。
   c. 运行 verifier（隔离环境；不读 cwd；不读其他 trial）。
   d. 收集 result，构造完整 EvidenceRecord，hash 得 evidence:E。
   e. 检查 evidence:E 是否已在 store/derived/evidence/：
      存在 -> noop（content-addressed，重复跑得同 ID）。
      不存在 -> 写入 store/derived/evidence/E.json。
   f. append E 到 evidence_refs[<id>].evidence（如果不在）。
4. 删除 trial worktree。
5. 渲染结果。
```

#### 无参数版本

`graft validate`（无参数）从 cwd 推断 change：

```text
if state/cwd.dirty:
  fail [E_DIRTY_CWD_AMBIGUOUS]
  提示：先 graft create 落成 candidate，再 graft validate <candidate>
else:
  fail [E_NOTHING_TO_VALIDATE]
  提示：cwd 与 state/cwd.base_state 一致，没有变化要验证
```

显式版本（`graft validate <id>`）始终隔离运行，与 cwd 状态无关。详见 §5.2。

#### 后续追加 evidence

```bash
graft validate patch:91sx8q2h --expect CargoFmtClean
```

patch body 永远不变；evidence_refs 是 append-only。post-admit 追加 evidence 是常态——本地 verify 远端 patch 的复现性就是这个路径（§6.3）。

### 4.3 graft admit: candidate → patch

```bash
graft admit <candidate-id> [--require <Property>...]
graft admit --capture [--alias <name>] [--require <Property>...]
graft admit --capture --then <patch|tree>
```

#### 普通模式

```text
1. 解析 candidate:C。
2. required = --require 给出 ∪ [admission].base_properties。
3. 对 required 中每个 atom，admission 算法（§2.5）查 evidence_refs[C]:
   ∃ E ∈ refs[C].evidence:
     E.change == C.change
     AND E.property == atom.id
     AND E.result == passed
   失败任何一条 -> [E_ADMISSION_UNMET]。
4. 通过：
   构造 Patch body（C 的字段 + admitted_at + properties）。
   PatchId = hash(Patch body)。
5. mv:
   store/private/candidate/<C-digest>.json -> store/public/patch/<P-digest>.json
   store/private/evidence_refs/<C-digest>.json -> store/public/evidence_refs/<P-digest>.json
     （body 中 owner 字段从 candidate:C 改为 patch:P）
6. 删除 candidate alias（如果有指向 C 的 state/aliases/candidates/<name>）。
7. 不修改 state/cwd（admit 不切视图）。
```

admit 完成后 candidate 在文件系统上消失。要追溯 patch 来自哪个 candidate，看 `patch.provenance` 字段。

#### `--capture` 模式：cwd dirty 救火

cwd dirty 时大部分用户操作 block（§5）。`--capture` 把 dirty 状态原子化处理：

```text
graft admit --capture [--alias <name>]:
  1. 等价于 graft create --alias <name>（落 candidate）
  2. 等价于 graft validate <candidate> --all-expected（重跑 expected verifier）
  3. 等价于 graft admit <candidate>
  整个过程在 daemon 的一次 transaction 内，中间任一步失败回滚。

graft admit --capture --then <id>:
  上面三步 + materialize <id>。
  failure mode：admit 失败时不 materialize，cwd 留 dirty + candidate 已落盘
  （failure 后用户能看到 candidate，决定是否手工解决）。
```

这是 cwd dirty 状态的唯一前进路径。其他写命令在 dirty 时一律 fail。

#### Failure modes

```text
[E_ADMISSION_UNMET]
  required PropertyExpr 的某 atom 找不到 passed evidence。
  原因：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
        或本地从未跑过该 verifier。
  提示：graft validate <candidate> --expect <Property>
        或 graft validate <candidate> --all-expected

[E_PROPERTY_DRIFT]
  required atom 的 PropertyId 与 candidate.expected 中对应 atom 不一致。
  原因：properties/<Name>.toml 在 candidate 创建后被修改。
  解决：要么用现行 PropertyId 跑新 evidence，
        要么 revert properties 改动并 graft repo lock 重算。

[E_EMPTY_CAPTURE]
  --capture 但 cwd 与 state/cwd.base_state 一致，无可捕获 change。
  提示：移除 --capture，直接 graft admit <已有 candidate>。
```

### 4.4 graft promote: patch → external git

```bash
graft promote <patch-id> --to <target> [--branch <name>] [--require <Property>...]
```

#### 流程

```text
1. 解析 patch:P 和 [promote_targets.<target>] 配置。
2. required = --require 给出 ∪ [promote_targets.<target>].required_properties。
3. 对 required 跑 admission 算法（与 admit 同；查 evidence_refs[P]）。
   失败 -> [E_PROMOTION_UNMET]
4. 在 [promote_targets.<target>].url 上构造 commit:
   - tree from patch.target_state（如果是 Tree）；
     或者从 patch.base_state（Repo）继承 + apply change；
   - parent commit 由 --branch 解析后的远端 ref tip 决定（fast-forward 检查）。
5. push 到 url:refs/heads/<branch> 或 PR head ref。
6. 落 promotion record:
   store/public/promotion/<digest>.json
   { id, patch, target_url, target_ref, commit_id, promoted_at, by }
7. 不更新 state/cwd（promote 不切视图）。
```

下次 sync 自动把 promotion record 推到 graft origin，团队成员可见。

#### 不变量

```
Invariant 4.4.1  (PromotionIsTheOnlyExternalGitWrite)
  Graft 命令中只有 graft promote 写非-Graft git repo。
  graft sync 写 Graft remote（refs/graft/*），不写 refs/heads/* 或 PR head。
  graft create / admit / materialize / validate 永远不写任何 git。
```

#### Failure modes

```text
[E_PROMOTION_UNMET]      required_properties 不满足。
[E_PROMOTION_NOT_FF]     远端 branch 不能 fast-forward；用户决定 force / abort。
[E_PROMOTION_TARGET_UNKNOWN]   --to <target> 在 graft.toml 找不到。
```

### 4.5 关系操作: compose / migrate / revert

```bash
graft compose <patch:a> <patch:b>          # 输出新 patch:c
graft migrate <patch:a> --onto <state>     # 输出新 patch:a'
graft revert <patch:a>                     # 输出新 patch:reverse_of_a
```

语义在 op list 上工作（§2.3）。每条命令产出新 patch + 一条 relation（§2.7）。

**v1 不建模 conflict**：上述命令在不可解时**直接 fail**，不产出 conflict 对象。错误信息提供具体冲突位置：

```text
[E_COMPOSE_CONFLICT]
  cannot compose patch:a and patch:b:
    src/foo.rs line 42: a writes "X", b writes "Y"
    src/bar.rs: a deletes, b modifies
  to resolve manually:
    1. graft materialize <some clean state>
    2. apply both intents in cwd
    3. graft create + graft admit
```

user 自己起 candidate 编码 resolution，admit 后产出 patch:c'，并写一条 `Relation { kind: Compose, inputs: [a, b, ...], outputs: [c'] }` 关联三者作为 derivability 历史。

---

## 5. CWD as view

cwd 是工作区，不是 view。但 cwd 通常**对应**某个 graft 状态——它要么是某个 patch 的物化，要么是用户在该物化之上的临时编辑。这个对应关系由 `state/cwd` 显式表达。

### 5.1 state/cwd 指针

```text
.graft/state/cwd          # 单文件，atomic rename 写入
```

内容：

```text
tree:7a2f1c9d04bc          # 当前 cwd 物化的 base state（tree 形式）
```

或：

```text
patch:91sx8q2hcc0e         # 当前 cwd 物化自该 patch 的 target_state
```

两种形式语义对等（都指向一个 tree），区别仅在 provenance：从 patch 派生的 cwd 知道自己来自哪个 patch，便于后续 `graft create` 隐式选 base、`graft show patch:X` 标 "this is your current cwd"。

初始情况：`graft init` 创建空 `state/cwd`，等同于 `tree:graft:empty`。

### 5.2 Dirty 状态

`cwd dirty` 定义为：

```text
let base = state/cwd 解析出的 tree
let current = 扫描 cwd 后计算出的 tree
dirty = (current != base)
```

Dirty 不是隐式记录的状态——每次需要时按需扫描 cwd 计算。daemon 可以缓存最近一次扫描结果（按文件 mtime/size 索引）以避免重复 hash。

#### Dirty 时的命令规则

```text
blocked when dirty (用户面）:
  graft materialize         [E_DIRTY_CWD_BLOCKS_MATERIALIZE]
  graft sync                [E_DIRTY_CWD_BLOCKS_SYNC]
  graft validate (无参数)    [E_DIRTY_CWD_AMBIGUOUS]
  graft admit (不带 --capture)  [E_DIRTY_CWD_NEEDS_CAPTURE]
  graft create (不带 --force)   需要考虑：实际上 graft create 本身就是捕获 dirty
                                 这里 dirty 不是问题；正常路径。

allowed when dirty (用户面）:
  graft create              正是为 dirty 设计的
  graft admit --capture     救火路径
  graft show / graft incoming / graft search       只读
  graft validate <id>       隔离运行，与 cwd 无关

内部路径不受 dirty 影响:
  daemon 内部调用 verifier、gc 可达性扫描、
  sync 远端 diff 计算 (需要在隔离 worktree 里重建 state) 不考虑 cwd。
```

#### 不变量

```
Invariant 5.2.1  (DirtyIsAUserFacingGate)
  cwd dirty 是用户面门禁，不是底层算法门禁。
  daemon 内部调用 verifier / gc / sync diff 等均不考虑 cwd 状态。
  原因：它们的输入完全来自 content-addressed object，与 cwd 独立。
```

```
Invariant 5.2.2  (NoSilentLoss)
  任何会覆盖 dirty cwd 的命令 (graft materialize) 在 dirty 状态下必须 fail，
  除非用户显式 --discard 或通过 graft admit --capture --then 先捕获。
```

### 5.3 graft materialize

```bash
graft materialize <id> [--discard]
```

`<id>` 可以是 `tree:...` 或 `patch:...`。

#### 流程

```text
1. 解析 <id> 到 target tree T。
   patch -> patch.target_state为 tree T（如果 base_state 是 Repo，需要隔离 worktree apply）。
2. 检查 cwd dirty:
   dirty + 无 --discard -> [E_DIRTY_CWD_BLOCKS_MATERIALIZE]
3. 在隔离 worktree 中先构造 T 的完整实例（stage）。
4. atomic swap 到 cwd:
   - 删除 cwd 中 ignore 规则之外的现有文件。
   - rename / link 进 T 的内容。
5. 更新 state/cwd:
   - <id> 是 patch -> state/cwd = patch:<id>
   - <id> 是 tree  -> state/cwd = tree:<id>
6. 不迫切 sync，不写 admission/promotion record。
```

step 3 的 staging 是为了崩溃安全：中途崩溃时 cwd 要么全是 T 要么全是原状态，不存在部分处理。实现上可能是 hardlink + rename 或者拷贝，详见 §8。

#### Drift detection 辅助

```bash
graft status                       # 展示 state/cwd 指向 + dirty 文件列表
graft diff [--against <id>]        # 默认 against state/cwd
graft discard                      # = graft materialize <state/cwd current> --discard
```

### 5.4 cwd 没有 graft.toml 的情况

clone 或 materialize 一个不包含 graft.toml 的 tree（不是 workspace-shaped）后：cwd 没有 graft.toml 也没有 properties/。这个状态下：

```text
允许:
  graft show / search / incoming / status / diff      只读命令
  graft validate <id>                                  隔离运行，verifier 从
                                                       evidence.change.target_state 读
                                                       graft.toml + properties/*.toml
  graft materialize <id>                               切到包含 graft.toml 的状态

拒绝:
  graft create / admit (无参数)                       [E_NO_WORKSPACE_CONFIG]
  graft validate (无参数)                              [E_NO_WORKSPACE_CONFIG]
```

原因：alias 解析需要当前 cwd 的 properties/*.toml。隔离运行的 verifier 从 evidence 自带 state 读取 properties，不受 cwd 限制；但需要 cwd 作为 alias 读取点的命令 fail。

---

## 6. Sync protocol

Sync 是 Graft 唯一识别的同步动词。没有 push / pull 心智。

```bash
graft sync <remote> [--fetch-only] [--push-only]
                    [--on-divergence=abort|keep-local|keep-remote|save-both]
                    [--quiet]
graft incoming [<remote>]
```

### 6.1 远端约定

Graft 远端是一个 Git 仓库，负责存储以下三个固定 refs：

```text
refs/graft/facts          镜像 store/public/{tree,change,property,patch,
                                                  evidence_refs,relation,
                                                  promotion}/
refs/graft/blobs          镜像 store/public/blob/
refs/graft/manifests      sync checkpoint chain
```

三个都是**非分支**命名空间 (`refs/graft/*`而非 `refs/heads/graft/*`)，不污染托管平台 branch UI。

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
  "created_at": "2026-06-01T08:30:00Z",
  "by": "agent-foo@host",
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

Manifest 是同步一致点：fetch 后验证 manifest 调用的所有 oid 存在，才接受这次 sync。

### 6.3 Sync 状态机

```text
1. 检查 cwd 根目录 .git/ -> [E_GIT_IN_WORKSPACE]
2. 检查 cwd dirty -> [E_DIRTY_CWD_BLOCKS_SYNC]  (除非 --force-dirty)
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 验证远端格式:
   - 所有 manifest 的 prev_manifest 能连成 chain。
   - facts_tip / blobs_tip 在 fetch 下来的 history 中存在。
   - 随机抽检几个 object digest（hash完整性）。
5. 比较 local 与 remote 的 latest manifest:
   case A: local.last_synced == remote.latest_manifest
     全部一致 -> noop 或补齐 store/public/ 缺失对象。
   case B: local.last_synced 在 remote.history 中 (remote ahead)
     远端领先，本地只拉 -> 写 remote object 到 store/public/。
   case C: remote.latest_manifest 在 local.history 中 (local ahead)
     本地领先，远端需推 -> push 合并 + 新 manifest。
   case D: local 和 remote 都有对方没有的 manifest
     divergence -> --on-divergence 决定（下文）。
6. 实际写入 / push:
   - case B: store/public/ 写入缺失对象；state/remotes/<>/last_synced 更新。
   - case C: 构造 本次 sync 的 manifest，推 facts/blobs commit + manifest commit。
   - case D: 按 --on-divergence。
7. 列出 incoming patch tree (§6.5)。
```

### 6.4 Divergence 策略

```text
--on-divergence=abort         (默认)
  报告 divergence 详情。不 push、不作决定。退出黙认不是 0。

--on-divergence=keep-local
  本地 manifest 作为新 latest，远端 merge-base 之后的 manifest 被废弃
  （远端对象仍保留在 store/public/）。谨慎使用。

--on-divergence=keep-remote
  远端 manifest 作为 latest，本地 manifest 被废弃。本地超出的 patch
  只要其对象进了 store/public/ 仍可访问，但不在 manifest history。

--on-divergence=save-both
  创建一个 merge manifest：同时引用 local.latest 和 remote.latest 作为
  prev_manifest。这是唯一允许 manifest history 是 DAG 而非 chain 的场合。
  push。
```

CI 默认 abort；agent 交互式会话应该提示用户选择。

### 6.5 Incoming tree 渲染

sync 完（除 `--quiet`）后、或手动 `graft incoming`：

```text
$ graft incoming origin

incoming patches reachable from origin (since last sync):

base: tree:551a2bf3 (current cwd)
├── patch:bc12ef34  "fix: gc reachability"
│   target: tree:7a2f1c9d
│   properties: ValidPatch ✓  CargoTestsPass ✓
│   ev refs:    2 attested by remote, 0 locally rebuilt
│   └── patch:dd991122  "tweak gc traversal"
│       target: tree:8b3c4d77
│       properties: ValidPatch ✓
│       ev refs:    1 attested by remote, 0 locally rebuilt
└── patch:91sx8q2h  "feat: scratch read mode"
    target: tree:9d8e3f01
    properties: ValidPatch ✓  CargoTestsPass ✓  CargoFmtClean ✓
    ev refs:    3 attested by remote, 0 locally rebuilt

base: tree:ffeedd00 (not in your store)
└── patch:778899ab  "experimental: fact compactor"
    base unknown locally; fetch source repo or migrate

cwd is at tree:551a2bf3 (clean).

suggested:
  graft materialize patch:bc12ef34         # smallest delta
  graft materialize patch:dd991122         # follow chain
  graft materialize patch:91sx8q2h         # alternative branch
  graft verify-pending                     # rebuild evidence locally
```

#### 渲染原则

1. **按 base_state 分组**。同一 base 下的 patch 可能是串联或并列，由 base->target 的本地拓扑决定取 sibling 还是 nested。
2. **本地 cwd 的 base 置顶**，其他 base 被始终按 "不在本地 store"/"远古老" 等标签分组。
3. **patch 核心信息以 property 为主**，evidence 给 drill-down。`graft show patch:X --evidence` 展开。
4. **alias 漂移 标注 stale**：如果 patch 的 PropertyId 在当前 alias 表中对不上（name 表示不了 X但能表示 X'），渲染为 `ValidPatch ✓ (alias drift; was X, now X')`。
5. **未本地重建的 evidence** 标 "attested by remote, 0 locally rebuilt"。

### 6.6 Evidence sync 细则

```text
sync over the wire:
  store/public/evidence_refs/<owner>.json     → refs/graft/facts
  store/derived/evidence/<id>.json            ✖ (不传输)

push 时:
  evidence_refs 中包含远端还没有的 evidence ID 不是问题——ID 是
  内容寻址的，远端看到 后可以 选择本地 verify-pending 补上 body。

fetch 时:
  拉到远端 evidence_refs 后，本地 store/derived/evidence/ 中应该查
  evidence_refs 中出现但本地缺失的 ID，标为 "pending local rebuild"。

append-only union (重要):
  同一个 evidence_refs[<owner>].json 可能 local 和 remote 都 append 了
  不同 entry（A 本地 verify 了 CargoFmtClean，B 本地 verify 了 CargoTestsPass
  都 push了）。这 NOT 是 conflict——是 union。
  sync 算法 读 local 和 remote 两份后按 evidence ID set union 写一份。
  updated_at 取较新者。
```

#### 不变量

```
Invariant 6.6.1  (EvidenceRefsAreSetUnionAcrossSync)
  evidence_refs 是 Graft 中唯一允许 sync 两边都修改的对象。
  其重复动作按 evidence ID 集合 union；其他 typed object 都是 content-addressed
  不可变，不可能出现 两边都改 的场景。
```

### 6.7 Repo base 外部依赖处理

patch 的 `base_state = Repo {repo, oid}` 时：

```text
sync push 时:
  不试图 顺象 这个 oid (外部 git repo 不受 graft 控制)。
  manifest.summary 中记 repo:<id>@<oid> 依赖。

fetch 后表现:
  graft show patch:X 显示 base = repo:linux-stable@<oid>。
  graft materialize patch:X:
    检查当前 cwd 或者隔离 worktree 能不能拿到 oid。
    能  -> 隐式 fetch oid 后 materialize。
    不能 -> [E_REPO_OID_UNAVAILABLE]，提示检查 graft.toml + git fetch。
```

---

## 7. Clone

```bash
graft clone <remote> <dir>
```

### 7.1 行为

```text
1. mkdir <dir> + cd <dir>。检查并且拒绝已存在 .git/ 或 .graft/。
2. 创建 .graft/ 骨架:
   .graft/config.toml          { remotes.origin.url = <remote> }
   .graft/store/{public,private,derived}/
   .graft/state/
   .graft/run/
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 走 sync 状态机 (§6.3)；case B （remote ahead） 在这里是唯一可能。
5. 写入 store/public/。state/remotes/origin/last_synced 设为远端 latest。
6. cwd 留空（不创建 graft.toml，不创建 properties/）。
7. 提示用户选择下一步:
   - graft incoming origin
   - graft materialize <patch:|tree:>
   - graft init  (如果想从 graft:empty 起新工作流)
```

### 7.2 初始提示输出

```text
$ graft clone https://example.com/foo.git ./foo
fetched .graft from origin:
  refs/graft/facts:     312 objects
  refs/graft/blobs:     48 blobs
  refs/graft/manifests: 17 manifests
last_synced = manifest:abc123de

cwd is empty.

3 admitted patches reachable。选择下一步:

  graft incoming origin                    查看全部可物化对象
  graft materialize <patch:|tree:>         选择某个 state作为 cwd
  (or do nothing; .graft/ is fully populated for read-only inspection)
```

### 7.3 不变量

```
Invariant 7.3.1  (CloneDoesNotMaterializeByDefault)
  graft clone 后 cwd 为空，需要显式 graft materialize 才设定视图。
  原因：任何 "默认视图" 都会退化成 main 心智，与 Graft 所有权 明确同意 原则不符。
```

```
Invariant 7.3.2  (CloneStateIsAuthoritative)
  graft clone 拉下的 store/public/ 与 remote 完全对齐 (manifest 验证后)。
  本地不产生额外修改。state/remotes/origin/last_synced 准确反映 fetch tip。
```

---

## 8. Daemon

### 8.1 唯一 writer

Graftd 是 .graft/ 的唯一 writer。任何写命令 (CLI 进程、skill、SDK) 都走 wire op 到 daemon。

```text
CLI 进程:
  parse argv
  resolve .graft/run/daemon.sock
  if not exists or stale -> spawn graftd
  send wire frame
  await response
  render output

daemon 进程:
  bind socket
  serialize incoming wire op (FIFO)
  for each op:
    读 state/
    计算、写 store/、写 state/
    发响应。
```

#### 为什么需要唯一 writer

- store/ 中 evidence_refs 是 append-only，需要 read-modify-write 原子性。
- state/aliases/* 可能跨 alias 互相引用（admit 删 candidate alias 中间态），需要事务。
- sync 是大多步骤操作 (fetch / write / push)，需要串行。

本版**不提供** `--local` 或 inline writer 路径。CI / sandbox 场景靠 daemon 的 idle timeout 自动退出（§8.4）。

### 8.2 IPC 协议

当前实现使用 `cli_exec` wire op——daemon 接收 argv、在 daemon 进程内解析并执行同一套 command logic。低复杂度，全量命令默认出 daemon。

将来可能迁移到粒度更细的 typed RPC (e.g. AdmitRequest / SyncRequest)，但不在本文档 scope。

### 8.3 进程状态文件

```text
.graft/run/
  daemon.sock     unix socket 端点
  daemon.pid      当前 daemon PID，信息性
  trials/         verifier 隔离 worktree
  worktrees/      materialize stage
  tmp/
```

daemon 启动时:

```text
1. 检查 daemon.pid:
   - 不存在 -> 启动。
   - 存在但进程死 (kill -0 失败) -> 覆盖 PID、启动。
   - 存在且进程活 -> 退出 (另一个 daemon 在跑)。
2. bind daemon.sock。如果冲突 -> 覆盖后重 bind（骨架 PID 检查已保证独一性）。
3. 清理 trials/, worktrees/, tmp/ 中遗留内容。
```

socket bind + PID + `kill -0` 探活足够保障唯一性；v1 不需要额外 flock。

### 8.4 Idle timeout

```text
daemon 闲置 (no wire op for N minutes) 后自动退出。
默认 N=30。在 .graft/config.toml [daemon].idle_timeout_minutes 调整。
```

CI 场景：一条命令 启动 daemon 跳 idle wait 退出。退出时 daemon 清理自己的 socket / PID 文件。

### 8.5 崩溃恢复

```text
daemon 崩溃 (oom / segfault) 后:
  store/ 中已写入的内容安全 (content-addressed，部分写入的文件 hash 不匹配
    被下次写覆盖或 gc 中检出)。
  state/aliases/* 是 atomic rename，不会读到部分内容。
  evidence_refs 是 read-mod-write + atomic rename，不会读到不一致状态。
  scratch 丢失（daemon-instance-scoped）。
  trials/ worktrees/ tmp/ 可能有孤儿，下次启动清理。

下次 daemon 启动 恢复有序。CLI 重试连接。
```

---

## 9. Garbage collection

```bash
graft gc --dry-run                    # 默认 dry run
graft gc --apply
graft gc --keep-newer-than 7d
graft gc --derived-only               # 只清 store/derived/
graft gc --include-orphan-blobs
```

### 9.1 可达性 roots

```text
roots =
    state/aliases/{candidates,patches,promotions}/* 解析到的 ID
  ∪ state/cwd 引用的 tree 或 patch ID
  ∪ 当前 properties/*.toml 解析到的 PropertyId 集合
  ∪ [admission].base_properties 解析到的 PropertyId
  ∪ [promote_targets.*].required_properties 解析到的 PropertyId
  ∪ daemon 内存中当前 active scratch / lease 中的 blob/tree
```

本地仓库不访问 remote。gc 仅看本地；sync 后本地 store 会被 roots 全覆盖。

### 9.2 达可 walk

从 roots 出发递归:

```text
candidate.change           -> change
patch.change               -> change
change.base_state          -> tree (若 Tree variant)
change.target_state        -> tree (若 Tree variant)
change.ops[].blob          -> blob
tree.entries[].hash        -> blob
evidence_refs[<id>].evidence -> evidence (in store/derived/)
evidence.change            -> change
evidence.property          -> property
relation.inputs[]          -> any
relation.outputs[]         -> any
promotion.patch            -> patch
manifest.facts_tip / blobs_tip   (仅验证; 不作为达可 walk 起点)
```

标记可达；diff 得 orphan。

### 9.3 清理策略

```text
store/public/   orphan + older than --keep-newer-than -> 删除
store/private/  orphan + older than --keep-newer-than -> 删除
store/derived/  --derived-only 或常规 gc 都可清。
                 可重建 (verifier)，不需 keep-newer-than 保护。
store/public/blob/  --include-orphan-blobs 才会删
                    (默认保留，因为 tree 可能在远端 manifest 中引用)
```

#### 不变量

```
Invariant 9.3.1  (DerivedAlwaysSafeToDelete)
  rm -rf .graft/store/derived/ 任意时候安全。
  daemon 下次需要某个 evidence body 时重跑 verifier 即可重建。
```

```
Invariant 9.3.2  (NoSilentLossInPublicGc)
  store/public/ 的 gc 默认 dry-run。仅在 --apply + 可达性 walk 不可达 +
  --keep-newer-than 阈值三者均成立时删除。
  remote 中仍可达但本地被 gc 的对象下次 sync 可以重拉。
```

---

## 10. Invariants and failure modes

本节集中列出全文不变量和常见失败模式，以便实现时逐个检查。

### 10.1 全文不变量总表

| Inv  | 名称                                  | 位置   |
| ---- | ----------------------------------- | ---- |
| 2.4.1 | PropertyAliasDecoupling             | §2.4 |
| 2.5.1 | EvidenceContentAddressing           | §2.5 |
| 2.5.2 | EvidenceReproducibility             | §2.5 |
| 3.3.1 | NoDriftingExternalReferences        | §3.3 |
| 3.3.2 | LockSchemaUniformity                | §3.3 |
| 4.4.1 | PromotionIsTheOnlyExternalGitWrite  | §4.4 |
| 5.2.1 | DirtyIsAUserFacingGate              | §5.2 |
| 5.2.2 | NoSilentLoss                        | §5.2 |
| 6.6.1 | EvidenceRefsAreSetUnionAcrossSync   | §6.6 |
| 7.3.1 | CloneDoesNotMaterializeByDefault    | §7.3 |
| 7.3.2 | CloneStateIsAuthoritative           | §7.3 |
| 9.3.1 | DerivedAlwaysSafeToDelete           | §9.3 |
| 9.3.2 | NoSilentLossInPublicGc              | §9.3 |

### 10.2 错误码总表

| Code                              | 含义                                                       | 处理                       |
| --------------------------------- | -------------------------------------------------------- | ------------------------ |
| `[E_GIT_IN_WORKSPACE]`            | cwd 根发现 .git/                                          | rm -rf .git 或选别的 cwd      |
| `[E_NO_CONFIG]`                   | graft.toml 不存在                                          | graft init 或进入正确目录          |
| `[E_NO_WORKSPACE_CONFIG]`         | cwd 未包含 graft.toml 但调用了需要它的命令              | materialize 一个 workspace-shaped patch |
| `[E_LEGACY_ID]`                   | 输入了 gr_/grc_/ev_/... 旧 ID                              | 采用 `<kind>:<digest>`           |
| `[E_DIRTY_CWD_BLOCKS_MATERIALIZE]` | dirty cwd 拒绝 materialize                                | --discard 或 admit --capture --then |
| `[E_DIRTY_CWD_BLOCKS_SYNC]`       | dirty cwd 拒绝 sync                                       | --force-dirty 或先 capture        |
| `[E_DIRTY_CWD_AMBIGUOUS]`         | graft validate 无参数下 cwd dirty 无法推断唯一语义      | graft validate <id>             |
| `[E_DIRTY_CWD_NEEDS_CAPTURE]`     | admit 不带 --capture 但 cwd dirty                          | admit --capture                  |
| `[E_EMPTY_CHANGE]`                | scratch_promote / create 时 endpoint diff 为空              | 确保有实际变更                  |
| `[E_EMPTY_CAPTURE]`               | admit --capture 但 cwd 与 base 一致                          | 去掉 --capture                  |
| `[E_PROPERTY_LOCK_DRIFT]`         | properties/X.toml 变但 lock 未同步 (自动修复)               | (auto)                       |
| `[E_REPO_LOCK_DRIFT]`             | graft.toml repo treeish 与 lock 不一致                       | graft repo update <name>         |
| `[E_UNKNOWN_PROPERTY]`            | alias name 在 properties/ 找不到                           | 添加文件或修改 graft.toml          |
| `[E_REPO_OID_UNAVAILABLE]`        | materialize 需要某 oid 但本地/远端 git 都拉不到            | git fetch 该 oid                |
| `[E_ADMISSION_UNMET]`             | required 某 atom 无 passed evidence                        | graft validate <candidate>     |
| `[E_PROPERTY_DRIFT]`              | candidate.expected 与当前 alias 表不一致                  | 二选一 (§4.3)                  |
| `[E_PROMOTION_UNMET]`             | promote required_properties 未满足                       | 补 evidence 后重试             |
| `[E_PROMOTION_NOT_FF]`            | promote target branch 不能 FF                            | --force-push 或调整 base       |
| `[E_PROMOTION_TARGET_UNKNOWN]`    | --to <target> 未在 graft.toml 声明                         | 补上 [promote_targets.<>]      |
| `[E_COMPOSE_CONFLICT]`            | compose / migrate / revert 遇 conflict，v1 不建模         | 手动 candidate (§4.5)          |
| `[E_SCRATCH_LOST]`                | daemon 重启后 scratch lease 失效                          | 重新 scratch open              |
| `[E_DIVERGENCE]`                  | sync 到 case D 且 --on-divergence=abort                  | --on-divergence=<选择>          |

### 10.3 常见状态转换错误

```text
clone 后立即 graft create:
  cwd 空 -> base 推断为 graft:empty。OK。
  但 expected 需要 ValidPatch，而 cwd 没有 graft.toml -> [E_NO_WORKSPACE_CONFIG]。
  提示：先 graft materialize 一个包含 graft.toml 的 patch，或者 graft init。

properties 改名后老 evidence 不可见:
  PropertyId 不变，只是 alias 指向不同。
  graft show patch:X --evidence 仍能看。
  alias "新名" 在当前起指向同一个 PropertyId，evidence 仍有效。

properties 改 spec 后老 evidence 表现:
  PropertyId 漂移 X -> X'。
  老 evidence 仍在、仍可查，但对 alias "名字" 在 admission 中不再有效。
  graft show patch:X --evidence 标 "alias drift; was X, now X'"。
  重新 admission 需要 graft validate <X> --expect <name> 补 X' evidence。

fetch 后 patch 未本地 verify:
  graft show patch:X 渲染 "ValidPatch ✓ (attested by remote, not yet locally
  rebuilt)"。admission 查询 evidence body 在 store/derived/ 缺失 -> [E_ADMISSION_UNMET]。
  提示跑 graft validate patch:X --all-expected 或 graft verify-pending。
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

### Bootstrap

```bash
graft init                                  # 创建 .graft/ + graft.toml + properties/骨架
graft clone <remote> <dir>                  # §7
```

### State / view

```bash
graft status                                # state/cwd + dirty
graft diff [--against <id>]                 # cwd vs base
graft materialize <id> [--discard]          # §5.3
graft discard                               # = materialize state/cwd current --discard
```

### Lifecycle

```bash
graft create [--from <base>] [--expect <P>...] [--message <msg>] [--alias <name>]
graft validate <id> [--expect <P>...] [--all-expected]
graft validate                              # 需 cwd clean, 推断 base
graft admit <candidate-id> [--require <P>...]
graft admit --capture [--alias <name>] [--require <P>...]
graft admit --capture --then <id>           # rescue path
graft promote <patch-id> --to <target> [--branch <name>] [--require <P>...]
```

### Sync / collaboration

```bash
graft sync <remote> [--fetch-only|--push-only]
                    [--on-divergence=abort|keep-local|keep-remote|save-both]
                    [--quiet]
graft incoming [<remote>]
graft verify-pending                        # rebuild evidence locally for refs in store/public
```

### Inspect

```bash
graft show <id> [--evidence] [--change] [--full]
graft search [--property <P>] [--state <id>]
graft candidates                            # 列 store/private/candidate/
graft show evidence:<digest>
graft repo list
graft repo lock [<name>] | update [<name>|--all]
```

### Relation

```bash
graft compose <patch:a> <patch:b>           # may [E_COMPOSE_CONFLICT]
graft migrate <patch:a> --onto <state>      # may [E_COMPOSE_CONFLICT]
graft revert <patch:a>                      # may [E_COMPOSE_CONFLICT]
```

### Scratch

```bash
graft scratch open --base <id>
graft scratch read <scratch-id> <path> --mode <hashlines|...>
graft scratch write <scratch-id> <path> --content <bytes>
graft scratch edit <scratch-id> <path> --edits <json>
graft scratch diff <scratch-id>
graft scratch promote <scratch-id> --expect <P>... --message <msg>
graft scratch drop <scratch-id>
graft scratch pin / unpin
graft scratch status
```

### Maintenance

```bash
graft gc [--dry-run|--apply] [--keep-newer-than <dur>] [--derived-only] [--include-orphan-blobs]
graft alias set candidates|patches|promotions <name> <id>
graft alias unset ...
graft alias list
graftd status --socket .graft/run/daemon.sock
graftd stop   --socket .graft/run/daemon.sock
```

### Validation & dev hygiene

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --doc --workspace
# smoke: tests/*.sh
```

---
