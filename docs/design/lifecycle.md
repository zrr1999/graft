# Graft 设计 · 生命周期（§4–§5）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 4. Lifecycle

Graft 把变更生命周期拆成三道关，每一关都有显式动词和门槛：

```text
scratch operation (CLI or pi-graft client)
  -> graft scratch write/edit/delete/read [--repo <repo>] --base <base> | --from <scratch>
scratch                         daemon-backed draft, not synced, not a candidate
  -> graft patch from-scratch <scratch>
candidate                       store/private, local-only
  -> graft patch validate       produces evidence (store/derived)
  -> graft patch admit          gates: core integrity + [admission].required
patch                           store/public, synced only when workspace [sync] enabled
  -> graft patch promote        gates: core integrity + promotion/target/CLI required
target output                   outside Graft's patch graph (git ref / local file)
```

每道关的语义：

- **scratch**：临时草稿读写/编辑/删除，无 candidate/patch 写入、无 cwd 捕获。第一次操作显式给 `--base`，后续操作显式给 `--from`；rename 用 delete+write 表达。CLI 与 pi-graft 是两个 client，但共享同一 daemon wire protocol。
- **candidate-from-scratch**：把 scratch 中的变更落成可寻址 candidate，无外部副作用。空 change 允许存在；若 workspace 要显式标记或拒绝它，应声明 `empty_change` / `non_empty_change` constraint。
- **admit**：candidate 升 patch，等于「我（本地）愿意把这件事公开给团队」。门槛是 application core integrity（`apply(action, base, proof) == target` 且 `replay(base, change.ops) == target`）加 `[admission].required`。**这不是 review gate**——review 在 sync 之后由每个 clone 自己决定。
- **promote**：patch 投影到下游 target（当前实现为配置的外部 Git repo/ref 或显式 `--to <branch>`），等于「它能 ship 给非 Graft 用户或工具」。门槛 `[promotion].required`、`[promote_targets.<target>].required` 与 `--require`。

admit ≠ review。这点在分布式协作中至关重要：远端 patch 进入本地 store 不代表本地必须采用，alias 是否指向它由本地决定。

### 4.1 scratch + candidate-from-scratch: draft → candidate

```bash
graft scratch write  [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --content <bytes>
graft scratch edit   [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path> --edits <json>
graft scratch delete [--repo <repo-id>] (--base <base> | --from <scratch-id>) <path>
graft patch from-scratch <scratch-id> [--expect <constraint>...] [--producer <label>] [--message <msg>]
```

行为：

1. 解析 scratch source：
  - `--base <id>` 开始第一版 scratch。不写 `--repo` 时，bare treeish 在 workspace Git 上下文解析；写 `--repo C` 时，bare treeish 在 `[repos.C]` 上下文解析。`graft:empty`、`tree:...`、`candidate:...`、`patch:...` 是显式 base ref，不需要 repo base context。
  - `--from <scratch-id>` 续写前一版 scratch。scratch 是 daemon-instance-scoped，daemon 重启后可能失效。
  - `base` 与 `from` 互斥且必须提供其一。
2. scratch 命令只改 daemon scratch graph，返回新的 `scratch:<digest>` 与 changed paths；不写 candidate、patch、git ref 或 cwd 文件。
3. `graft patch from-scratch` 读取指定 scratch，把 scratch op chain lower 成 canonical `Action`，构造 `ApplicabilityProof`，应用到 base 得到 target，并派生 endpoint `Change`。
4. 空 change 允许存在；若 workspace 要显式标记或拒绝它，应声明 `empty_change` / `non_empty_change` constraint。`Action::Sequence([])` 仍按 [§2.4](./model.md) 的 canonicalization 规则保存。
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
graft patch validate <id> [--expect <constraint>...]
```

`<id>` 可以是 `candidate:...`、`patch:...`，或 `application:...`。当前主路径要求显式 id；先用 scratch 命令得到 `scratch:<digest>`，再用 `graft patch from-scratch <scratch>` 得到 candidate。`change:...` 只是 endpoint view，不是 constraint subject；若需要验证，必须通过拥有该 change 的 application / candidate / patch。旧的 `graft validate` 顶层入口是隐藏兼容 alias。

#### 流程

```text
1. 解析 <id> 拿到目标 application，并从 application 读取 action/base/proof/target/change。
2. 解析 candidate/patch 自带的 `constraint`，并把重复给出的 `--expect <constraint>` 名称按当前 `constraints.roto` / `graft.lock` 解析为额外 `Constraint`。
3. 遍历 required `Constraint` 的 primitive leaves；每个 primitive 是一个 `PlanId`。
   a. 读取 `Plan { observation, assertion }`。
   b. 对需要执行的 observation，物化输入 tree 到 `.graft/store/derived/worktrees/<tree-id>/`；同一 `RunPlan` 可按 `(argv, materialized tree id)` memoize。
   c. 运行或复用 observation，随后对 canonical result 执行 assertion。
   d. 构造 `EvidenceRecord { subject, plan, verifier, result, ... }`。
   e. 检查 store/derived/evidence/E.json：存在 -> noop；不存在 -> 写入。
   f. append E 到 evidence_refs[<id>].evidence（如果不在）。
4. 保留 derived worktree cache；store/derived 可由 gc 安全清理。
5. 渲染结果；若 force rerun 与已有 evidence_refs 不匹配，报告本地复现差异。
```

#### 显式 id 版本

`graft patch validate <id>` 始终隔离运行，与 cwd 状态无关。cwd 只用于 workspace 路由，不用于推断要验证的 change。详见 §5.2。

#### 后续追加 evidence

```bash
graft patch validate patch:91sx8q2h --expect cargo_fmt_clean
```

patch body 永远不变；evidence_refs 是 append-only。post-admit 追加 evidence 是常态——本地 verify 远端 patch 的复现性就是这个路径（[§6.3](./runtime.md)）。

### 4.3 graft patch admit: candidate → patch

```bash
graft patch admit <candidate-id> [--require <constraint>...]
```

```text
1. 解析 candidate:C。
2. 检查 application core integrity：`apply(action, base, proof) == target` 且 `replay(base, change.ops) == target`，失败 -> `[E_CHANGE_INTEGRITY]`。
3. required = `[admission].required` ⊓ candidate.constraint ⊓ repeatable `--require <constraint>` additions。
4. 对 required constraint 递归执行 `satisfy`（§2.6）；每个 primitive 查询 evidence_refs[C]:
   ∃ E ∈ refs[C].evidence:
     E.subject == C
     AND E.plan == primitive.plan
     AND E.result == passed
   失败任何一条 -> [E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]。
5. 通过：
   构造 Patch body（C 的 application/provenance + admitted constraint + admission summary）。
   PatchId = hash(Patch body)，不包含 local admitted_at。
6. mv:
   store/private/candidate/<C-digest>.json -> store/public/patch/<P-digest>.json
   store/private/evidence_refs/<C-digest>.json -> store/public/evidence_refs/<P-digest>.json
     （body 中 owner 字段从 candidate:C 改为 patch:P）
7. 删除 candidate alias（如果有指向 C 的 local/aliases/candidates/<name>）。
8. 不修改 cwd route 或 worktrees（admit 不切视图）。
```

admit 完成后 candidate 在文件系统上消失。要追溯 patch 来自哪个 candidate，看 `patch.provenance` 字段。

admit 保持纯粹：只接受已有 candidate，不捕获 cwd，不 materialize，也不写 promote target。用户需要先通过 scratch 具体命令生成 scratch，再用 `graft patch from-scratch` 进入 candidate lifecycle。

#### Failure modes

```text
[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]
  required Constraint 的某 primitive 找不到 passed evidence。
  原因：本地 evidence body 缺失（refs 有但 store/derived 中没有重建）
        或本地从未跑过该 verifier。
  提示：graft patch validate <candidate> --expect <constraint>

[E_CONSTRAINT_DRIFT]
  required primitive 的 PlanId 与 candidate.constraint 中对应 primitive 不一致。
  原因：constraints.roto 中对应顶层 constraint 函数在 candidate 创建后被修改或改名。
  解决：要么用现行 PlanId 跑新 evidence，
        要么 revert constraints.roto 改动并刷新 graft.lock。
```

### 4.4 graft patch promote: patch → target projection

```bash
graft patch promote <patch-id> --to <target-or-branch> [--require <constraint>...]
```

#### 流程

```text
1. 解析 patch:P；检查 application core integrity。
2. 若 --to 命中 [promote_targets.<target>]，再解析该 target 的 path/branch/required。
3. required = `--require <constraint>` 给出 ∪ `[promotion].required` ∪ `[promote_targets.<target>].required`。
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
[E_PROMOTION_UNMET]            required 不满足。
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

语义在 op list 上工作（[§2.4](./model.md)）。每条命令产出新 candidate，并在 candidate provenance 中记录 pending relation；candidate admit 成 patch 后再写 public relation（[§2.8](./admission.md)）。

#### compose-admission 的 stable evidence reuse

`compose a b` 生成的 candidate 是新 `Application` / 新 id。默认仍需为 required primitive 查询或重跑 evidence；但对 [§2.5](./property.md) 定义的可复用 primitive，可把父 patch 的已通过 evidence 作为复合体 admission 的派生依据。

复合 candidate 自动携带的可复用约束是：

```text
all_of(
  primitive p
  where p is required by both parent patches
    and (p is target-only or p is explicitly stable_under_composition)
)
```

其中：

- **target-only** primitive 只观察 `app.target()` 派生的 tree/run/file；由 `targetCompose` 知道复合体 target = 右侧父 patch target，因此可复用右侧父 patch 的 evidence。
- **explicit stable** primitive 是后续实现可加入的 constraint-source 声明；只有两个父 patch 都有 passing evidence 时，才能用 kernel 的 `certifyComposedShared` 推导复合体满足该 primitive。当前实现尚不自动执行这类 stable reuse。
- `extra` 约束不自动传播：若 parent constraints 是 `p ⊓ extra_a` 与 `p ⊓ extra_b`，只自动携带共享且可复用的 `p`；`extra_a` / `extra_b` 对复合体仍需 fresh evidence 或显式要求。

`stable` 不进入 `PlanId` 哈希，是 admission-time policy。验证复合 patch 时，Graft 通过 public Compose relation、父 evidence 与当前 constraint policy 重新推导可复用 primitive；若 constraint drift、stable 被撤回、父 evidence 缺失/不可复现，重推 **fail loud**，按 [§10.2](./reference.md) 的 `[E_CONSTRAINT_DRIFT]` 处理，并提示对复合体运行 `graft patch validate <candidate-or-patch> --expect <constraint>` 补 fresh evidence。

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

cwd 不是 view。cwd 只用于命令路由（[§12](./workspace.md)）。`StateId` 表示完整 workspace snapshot；`application`、`patch`、`candidate`、`repo:<id>@<treeish>` 和 Git treeish 都只是解析到 state 的入口。

### 5.1 Materialize 输出目录

```text
<workspace-root>/.worktrees/<state-slug>/
```

`<workspace-root>` 是 local workspace root 或 system workspace root（例如 `$GRAFT_HOME/workspaces/default`）。`graft patch materialize <state-ref>` 默认永不写当前 cwd。输出目录按 resolved state identity 命名，不按输入 ref 或 patch id 命名。

多 repo 内容是 materialized state root 内的普通目录，例如：

```text
<workspace-root>/.worktrees/<state-slug>/
  graft.toml
  constraints.roto
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

`<state-ref>` 可以是 `graft:empty`、`tree:...`、`application:...`、`candidate:...`、`patch:...` 或 `repo:<id>@<treeish>`。内部先解析成确定 `StateId`，再物化完整 workspace state。显式写外部目标只能通过 `graft patch promote`。

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

`call([...], app.target())` 与 `graft run <state-ref> -- ...` 使用同一个 state materialization + command execution model；区别是 validate 会为 constraint 生成 evidence，run 只返回一次性命令结果。

#### 辅助命令

```bash
graft workspace status             # 展示 cwd lookup 命中层、workspace、daemon 状态
graft patch diff <id-a> <id-b>     # object-to-object diff；不默认绑定 cwd
graft workspace attach --status    # 展示 cwd -> workspace route
```

---
