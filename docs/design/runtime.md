# Graft 设计 · 运行时（§6–§9）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化内核见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 6. 同步协议

Sync 是 Graft 唯一识别的同步动词。不引入 push / pull 心智模型。

```bash
graft sync [<remote>] [--fetch-only] [--push-only]
graft patch incoming
```

### 6.1 远端约定

Graft 远端是一个 Git 仓库，负责存储以下三个固定 refs：

```text
refs/graft/facts          镜像 store/public/{tree,action,application,change,
                                                  constraint,patch,evidence_refs,
                                                  relation,promotion}/
refs/graft/blobs          镜像 store/public/blob/
refs/graft/manifests      同步检查点链
```

三个都是**非分支**命名空间 (`refs/graft/`*而非 `refs/heads/graft/*`)，不污染托管平台 branch UI。

#### 不同 ref 的原因

- `facts`：多数是小 JSON（< 1 KB），合在一个 tree 结构中 push 友好。
- `blobs`：可能很大（资源文件、二进制依赖）。单独 ref 让 lazy fetch / partial clone 可能。
- `manifests`：每次 sync 附加一个 manifest commit，chain 作为同步历史。

**注意**：evidence body 不进入任何 ref。evidence_refs 进入 `facts`。

### 6.2 Manifest 结构

每次 sync 产出一个 manifest object，写进 `store/public/manifest/`，同时 push 为 `refs/graft/manifests` 的 commit：

```json
{
  "id": "manifest:abc123def456",
  "version": 2,
  "facts_tip": "<refs/graft/facts 上本次 commit 的 oid>",
  "blobs_tip": "<refs/graft/blobs 上本次 commit 的 oid>",
  "prev_manifest": "manifest:<prev>",
  "summary": {
    "facts_files": 312,
    "blob_files": 48
  }
}
```

Manifest 是同步一致点：fetch 后必须验证 manifest 引用的所有 oid 都存在，才接受这次 sync。Manifest body 不携带 host/timestamp；这些只属于本地传输日志元数据，不参与 manifest id。

### 6.3 Sync 状态机

```text
1. 检查当前 workspace 的 [sync] 配置；ws:default 强制不 sync。
2. 若命令提供 `<remote>`，同步成功后写为 `.graft/local/remotes/default`；若未提供 `<remote>`，读取该默认 remote，缺失时报 `[E_SYNC_REMOTE_REQUIRED]`。
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 验证远端格式:
   - 所有 manifest 的 prev_manifest 能连成 chain 或允许的 merge-DAG。
   - facts_tip / blobs_tip 在 fetch 下来的 history 中存在。
   - 每个 typed object 的 filename/id 与 canonical body hash 匹配；同 ID 不同 bytes 是 hard error。
5. 比较 local 与 remote 的 latest manifest:
   case A: local.last_synced == remote.latest_manifest
     全部一致 -> 不操作或补齐 store/public/ 缺失对象。
   case B: local.last_synced 在 remote.history 中 (remote ahead)
     远端领先，本地只拉 -> 写 remote object 到 store/public/；evidence_refs 用 set union。
   case C: remote.latest_manifest 在 local.history 中 (local ahead)
     本地领先，远端需推 -> push 合并 + 新 manifest。
   case D: local 和 remote 都有对方没有的 manifest
     divergence -> 显式失败；用户先 fetch/review 后再人工处理。
6. 实际写入 / push:
   - immutable public object：按 ID union；缺失则复制；同 ID 不同 bytes 失败。
   - evidence_refs：按 evidence ID set union；owner 不变；updated_at 仅 local display metadata。
   - local/aliases、local/remotes/last_synced：仅本地写，不参与 remote merge。
   - case B: store/public/ 写入缺失对象；local/remotes/<>/last_synced 更新。
   - case C: 构造本次 sync 的 manifest，推 facts/blobs commit + manifest commit。
   - case D: 默认直接报告分歧；用户可显式选择 `--on-divergence keep-remote`
     接受远端 manifest frontier 并跳过本轮 push。
7. 列出 incoming patch tree (§6.5)。
```

### 6.4 分歧策略

当前提供两个策略：

- `--on-divergence abort`：默认策略。如果远端 manifest history 与本地 `last_synced`
  不兼容，`graft sync` 拒绝继续并提示先 fetch/review 或人工处理。
- `--on-divergence keep-remote`：仅在本轮允许 fetch 时可用。Graft 接受远端 latest
  manifest 作为新的 `local/remotes/<remote>/last_synced`，fetch 远端 public objects，
  并跳过本轮 push。这个策略不删除本地 immutable public objects；它只让远端 sync
  frontier 在本轮获胜，避免静默丢数据。

`keep-local` 和 `save-both` 尚未实现：前者需要显式的远端覆盖/删除语义，后者需要 manifest 从单 `prev_manifest` 演进到 merge-DAG。当前 CLI 不接受未实现策略，避免用 flag 表示尚未具备的数据模型。

### 6.5 传入补丁树渲染

sync 完成后，或手动运行 `graft patch incoming` 时：

```text
$ graft patch incoming

incoming patches reachable from origin (since last sync):

base: tree:551a2bf3 (local route context)
├── patch:bc12ef34  "fix: gc reachability"
│   target: tree:7a2f1c9d
│   constraint: cargo_tests_pass ✓
│   ev refs:    2 referenced by remote, 0 locally rebuilt
│   └── patch:dd991122  "tweak gc traversal"
│       target: tree:8b3c4d77
│       constraint: docs_only ✓
│       ev refs:    1 referenced by remote, 0 locally rebuilt
└── patch:91sx8q2h  "feat: scratch read mode"
    target: tree:9d8e3f01
    constraint: cargo_tests_pass ✓  cargo_fmt_clean ✓
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

1. **按 base_state 分组**。同一 base 下的 patch 可能串联或并列，由 base->target 的本地拓扑决定展示为 sibling 还是 nested。
2. **本地当前目录的 base 置顶**，其他 base 始终按“不在本地 store”“较旧”等标签分组。
3. **patch 核心信息以 constraint 为主**，evidence 提供下钻信息。`graft patch show patch:X --evidence` 可展开详情。
4. **constraint drift 标注 stale**：如果 patch 的 PlanId 与当前同名 constraint 函数解析结果不一致，渲染为 `cargo_tests_pass ✓ (constraint drift; was X, now X')`。
5. **未本地重建的 evidence** 标注为 `referenced by remote, 0 locally rebuilt`。

### 6.6 Evidence sync 细则

```text
通过 wire 同步：
  store/public/evidence_refs/<owner>.json     → refs/graft/facts
  store/derived/evidence/<id>.json            ✖ (不传输)

push 时:
  evidence_refs 中包含远端还没有的 evidence ID 不是问题——ID 是
  内容寻址的，远端看到后可以选择本地 verify-pending 补上 body。

fetch 时:
  拉到远端 evidence_refs 后，本地 store/derived/evidence/ 中应该查
  evidence_refs 中出现但本地缺失的 ID，标为 "pending local rebuild" / "referenced by remote"。

追加式并集（重要）：
  同一个 evidence_refs[<owner>].json 可能 local 和 remote 都 append 了
  不同 entry（A 本地 verify 了 cargo_fmt_clean，B 本地 verify 了 cargo_tests_pass
  都 push 了）。这不是 conflict，而是 union。
  sync 算法读 local 和 remote 两份后按 evidence ID set union 写一份。
  updated_at 取较新者仅用于展示；不参与 EvidenceId 或 owner identity。

复用与失效：
  evidence body 只有在本地 store/derived/evidence/<id>.json 存在，且 body 中
  (subject Application/Patch/Candidate, PlanId, execution contract, canonical result/effects/outputs)
  与当前查询完全匹配时，才能满足 admission/promote/search。
  description / top-level constraint name 变化不影响既有 primitive evidence；PlanId 变化会让旧 evidence 不再
  满足当前 constraint primitive 要求。ApplicationId 或 execution contract 变化也必须重跑。
```

#### 不变量

```
Invariant 6.6.1  (EvidenceRefsAreSetUnionAcrossSync)
  evidence_refs 是 Graft 中唯一允许 sync 两边都修改的对象。
  其重复动作按 evidence ID 集合 union；其他带类型对象都是内容寻址
  不可变对象，不会出现“两边都改”的场景。
```

### 6.7 Repo base 外部依赖处理

patch.application 的 `base_state = RepoTree {repo_id, treeish, resolved_tree_oid}` 时：

```text
sync push 时:
  不试图同步这个 oid（外部 Git repo 不受 Graft 控制）。
  manifest.summary 中记 repo:<id>@<oid> 依赖。

fetch 后表现:
  graft patch show patch:X 显示 base = repo:linux-stable@<oid>。
  graft patch materialize patch:X:
    检查隔离 worktree 能不能拿到 oid。
    能  -> 隐式 fetch oid 后 materialize。
    不能 -> [E_REPO_OID_UNAVAILABLE]，提示检查 graft.toml + git fetch。
```

---

## 7. 克隆

```bash
graft get <remote> <dir>
```

### 7.1 行为

```text
1. mkdir <dir> + cd <dir>。检查并且拒绝已存在 .graft/；.git/ 不属于 Graft workspace，可存在但不被写入。
2. 创建 workspace 骨架并登记 registry:
   .graft/config.toml          { remotes.origin.url = <remote> }
   .graft/store/{public,private,derived}/
   .graft/local/
   worktrees/
3. fetch refs/graft/{facts, blobs, manifests} 从 <remote>。
4. 走 sync 状态机 (§6.3)；case B （remote ahead） 在这里是唯一可能。
5. 写入 store/public/。local/remotes/origin/last_synced 设为远端 latest。
6. cwd 留空（不创建 graft.toml，不创建 constraints.roto）。
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

有 3 个已接纳 patch 可达。选择下一步：

  graft patch incoming                     查看全部可物化对象
  graft patch materialize <application:|patch:|tree:>  输出到 .worktrees/<state-slug>/
  （也可以不执行任何操作；.graft/ 已完整填充，可用于只读检查）
```

### 7.3 不变量

```
Invariant 7.3.1  (CloneDoesNotMaterializeByDefault)
  graft get 后 cwd 不被写入，需要显式 graft patch materialize 才输出到 .worktrees/<state-slug>/。
  原因：任何“默认视图”都会退化成 main 心智，与 Graft 的明确所有权原则不符。
```

```
Invariant 7.3.2  (CloneStateIsAuthoritative)
  graft get 拉下的 store/public/ 与 remote 完全对齐 (manifest 验证后)。
  本地不产生额外修改。local/remotes/origin/last_synced 准确反映 fetch tip。
```

---

## 8. Daemon

### 8.1 唯一写入者

Graftd 是 `$GRAFT_HOME` 与所有 workspace `.graft/` 的唯一写入者。任何写命令（CLI 进程、skill、SDK）都通过 wire op 发送到全局 daemon。

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
  compute, write store/local/registry, respond
```

#### 为什么需要唯一写入者

- store/ 中 evidence_refs 是 append-only，需要 read-modify-write 原子性。
- local/aliases/* 可能跨 alias 互相引用（admit 删 candidate alias 中间态），需要事务。
- registry.toml 是全局 routing/index，需要 flock + daemon 串行化。
- sync 是多步骤操作 (fetch / write / push)，需要串行。

CI / sandbox 场景依靠 daemon 的 idle timeout 自动退出（§8.4）。

### 8.2 IPC 协议

当前使用 `cli_exec` wire op 承载多数由 daemon 拥有的 workspace 写命令：daemon 接收 argv，并在 daemon 进程内解析和执行同一套 command logic。每个 routed 请求必须携带 `workspace_id`；daemon 通过 registry 解析 workspace，并把可选 `workspace_root` 只当作一致性校验。`cli_exec` 不是泛用 argv 后门：scratch/candidate 走 typed RPC，`attach`/`detach` 走 workspace registry typed op，status/show/constraint/init 等本地或只读命令不允许通过 `cli_exec`。

```json
{
  "op": "cli_exec",
  "workspace_id": "ws:default",
  "workspace_root": "/Users/me/project",
  "argv": ["graft", "--cwd", "/Users/me/project", "patch", "validate", "candidate:abc"]
}
```

后续可迁移到粒度更细的 typed RPC（例如 AdmitRequest / SyncRequest），但所有 RPC 仍携带 `workspace_id`。

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

### 8.4 空闲超时

```text
daemon 闲置（所有 workspace 连续 N 分钟没有 wire op）后自动退出。
默认 N=30。在 `$GRAFT_HOME/config.toml [daemon].idle_timeout_minutes` 调整。
```

CI 场景：一条命令启动 daemon，idle 后退出。退出时 daemon 清理自己的 socket / PID 文件。

### 8.5 崩溃恢复

```text
daemon 崩溃（OOM / segfault）后：
  store/ 中已写入的内容安全 (content-addressed，部分写入的文件 hash 不匹配
    被下次写覆盖或 gc 中检出)。
  local/aliases/* 是 atomic rename，不会读到部分内容。
  evidence_refs 是 read-mod-write + atomic rename，不会读到不一致状态。
  scratch 丢失（daemon-instance-scoped）。
  $GRAFT_HOME/run/validation/ tmp/ 可能有孤儿，下次启动清理。
  workspace `.worktrees/` inspection output 由 doctor/gc 按策略清理，不在 daemon 启动时盲删。

下次 daemon 启动时恢复有序状态。CLI 重试连接。
```

---

## 9. 垃圾回收

```bash
graft workspace gc                    # 默认 dry run/report
graft workspace gc --apply
graft workspace gc --derived-only      # 只清 store/derived/
```

### 9.1 可达性根

```text
roots =
    local/aliases/{candidates,patches,promotions}/* 解析到的 ID
  ∪ 当前 constraints.roto 顶层 constraint 函数解析到的 Constraint body id 与 primitive PlanId 集合
  ∪ [admission].required 解析到的 Constraint body id / PlanId 集合
  ∪ [promotion].required 解析到的 Constraint body id / PlanId 集合
  ∪ [promote_targets.*].required 解析到的 Constraint body id / PlanId 集合
  ∪ daemon 内存中当前 active scratch / lease 中的 blob/tree
```

本地仓库不访问 remote。gc 仅看本地；sync 后本地 store 会被 roots 全覆盖。

### 9.2 可达性遍历

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
evidence.subject           -> candidate / patch / application owner
evidence.plan              -> plan
constraint primitive       -> plan
relation.inputs[]          -> any
relation.outputs[]         -> any
promotion.patch            -> patch
manifest.facts_tip / blobs_tip   (仅验证; 不作为达可 walk 起点)
```

标记可达对象；差集即为 orphan。

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
  store/public/ 的 gc 默认只试运行并报告。仅在 --apply + 可达性遍历不可达时删除。
  remote 中仍可达但本地被 gc 的对象下次 sync 可以重拉。
```

---
