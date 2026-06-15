# Graft 设计 · 工作区（§3 + §12）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 3. Workspace layout

### 3.1 cwd

```text
cwd/
  graft.toml              # 项目定义，进 snapshot
  graft.lock              # workspace 元配置锁（constraints + repos），进 snapshot
  constraints.roto          # constraint source，进 snapshot
  src/                    # workspace files，进 snapshot
  worktrees/              # local managed-repo checkout/output area; 默认不进 snapshot
  README.md
  ...
```

约束：

- 这里的 `cwd/` 只描述 `graft workspace init` 创建的 **local workspace root**。一般命令的 cwd 可以是任意目录；它通过 §12 的 lookup / routes 解析到 workspace。
- local workspace root 拒绝 `.git/`；Git checkout 只能作为外部 repo/promote target，而不是 Graft workspace 本体。
- snapshot 包含什么：
  - 包含：`graft.toml`、`graft.lock`、`constraints.roto`、所有普通工作区文件。
  - 不包含：`.graft/`（本地事实空间）、`.worktrees/`（临时 materialize 输出）、`worktrees/`（local managed-repo checkout/output area）。发现 `.git/` 时拒绝 capture，而不是静默忽略。
  - 排除规则的具体语法在 §3.4。

local workspace root 是工作区，不是默认视图。Graft 不维护"当前 cwd 是 selected snapshot 的物化"这种隐式不变量；显式 materialize 总是写隔离 worktree，详见 [§5](./lifecycle.md) 与 §12。

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
      constraint/    <digest>.json
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

  local/                            # mutable durable local bookkeeping, atomic write
    index.sqlite                    # patch/evidence query index (derived)
    aliases/
      candidates/<name>             # 单文件: candidate:<digest>
      patches/<name>                # 单文件: patch:<digest>
      promotions/<name>             # 单文件: promotion:<digest>
    remotes/<remote>/
      last_synced                   # manifest:<digest>
      transport.cache/              # bare git odb (transport-only)
    remotes/default                 # 默认 sync remote 路径（单文件）

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
| `local/`         | 可变指针，atomic write                         | 否    | 否    |
| `.worktrees/`    | explicit materialize 输出，临时检查目录；不建议编辑 | 否    | 可清理  |


#### 写规则

- `store/public/` `store/private/`：daemon 写一次后内容永不修改（content-addressed）。JSON body / index 通过 temp file + atomic rename 发布；删除只通过 gc。
- `store/derived/`：daemon 重建时写入；用户可整目录 `rm -rf` 而不破坏正确性。
- `local/`：atomic rename 写入。每个文件短小，单次 read/write 即一致快照。
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

`store/private/` `store/derived/` `local/` `.worktrees/` `$GRAFT_HOME/run/` 永不 sync。`ws:default` 强制永不 sync。

#### Alias locality

`local/aliases/*` 是 workspace-local mutable bindings：

```text
local/aliases/patches/release-candidate -> patch:abc123
```

它们不进入 manifest，不写 remote refs，不与其他 clone merge。两个 clone 可以把同一个 alias 名指向不同 patch，这不是 sync conflict。远端 patch fetch 到本地后，用户可以显式设置本地 alias 指向它；不设置 alias 就只是 store 中多了一个可查对象。

Constraint names 是另一回事：`constraints.roto` 顶层 constraint 函数是 patchable workspace source；函数名是配置/lock key，primitive 身份来自函数体里的 `PlanId = blake3(canonical(observation, assertion))`。

#### gc 范围

reachability roots：

```
local/aliases/{candidates,patches,promotions}/*  解析得到的对象 ID
当前 constraints.roto 顶层 constraint 函数解析得到的 Constraint body id 与 primitive PlanId 集合
当前 graft.toml [admission].required / [promotion].required / [promote_targets.*].required 解析到的 Constraint body id / PlanId 集合
daemon 内存中持有 lease 的 active scratch
```

从 roots walk，标记可达对象。`store/{public,private}/` 中不可达的对象在 gc 时删除（`store/derived/` 整目录可清，按需重建）。详见 [§9](./runtime.md)。

### 3.3 graft.toml / graft.lock 双锚

`graft.toml` 是 workspace 元配置；`graft.lock` 是派生缓存兼解析锚。二者都属于 workspace 的受管文件：任何变更都通过 patch admit，并且必须在同一个 patch 中原子同步。

`graft.lock` 是 Graft-tracked workspace 元配置锁、可跨 clone，**不包含本地路径**。本地路径归 `$GRAFT_HOME/registry.toml [[repo_paths]]`（§12）。

#### 形态

```toml
# graft.toml
schema = 1

[admission]
required = []

[promotion]
required = []

[repos.linux-stable]
url = "https://git.kernel.org/.../linux-stable.git"
default_branch = "linux-6.6.y"  # 可选；graft repo add 默认写 remote HEAD

[repos.cpython]
url = "https://github.com/python/cpython"

[promote_targets.gh-main]
path = "../external-git-repo"
branch = "main"
required = ["cargo_tests_pass"]
```

```toml
# graft.lock, @generated by graft; do not edit by hand
version = 1
locked_at = "2026-06-01T08:30:00Z"

# Constraints: constraint function name -> Constraint body id
[constraints]
empty_change = "constraint:374d33205102"
cargo_tests_pass = "constraint:044b52a36644"

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

#### Constraints 节

来源：`constraints.roto`。每次需要 constraint catalog 的命令都会解析并 typecheck：

```text
constraints.roto top-level function foo(app: Application) -> Constraint
  -> ConstraintDef { name: "foo", description, body }
  -> body_id = blake3(canonical(body))
  -> graft.lock [constraints].foo = body_id
  -> primitive plans materialized under store/public/plan/
```

如果 `graft.lock` 中 `[constraints].foo` 与当前算出的 body id 不一致 → drift detected。修复方式不是旁路写 lock，而是构造一个元配置 patch，把 `constraints.roto` 与 `graft.lock` 的新解析结果一起 admit。

命名与边界：

- Constraint name 是顶层函数名的原样拼写，例如 `empty_change`、`cargo_tests_pass`；不做 PascalCase alias 转换。
- Runtime primitive id 使用 `snake_case`，例如 `changed_paths`, `match`, `all_match`, `call`, `exit_code_is`, `same_output`。
- Runtime primitive 是内部 observe/compute/decide 的 building block，不承载 workspace policy name，也不把多个可独立命名的 policy requirement 捆成一个 primitive。
- `apply(action, base, proof) == target` 与 `replay(base, change.ops) == target` 是 Graft core application integrity，不是 constraint，也不是 runtime primitive；admit/materialize/promote 默认都会检查它。
- 空 change 是普通 constraint，可由 workspace 在 `constraints.roto` 中声明，例如 `fn empty_change(app: Application) -> Constraint { ... }`；非空要求也应作为 workspace policy 显式声明，而不是默认 gate。

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
[E_CONSTRAINT_LOCK_DRIFT]
  constraints.roto 中 X 的 body id 与 graft.lock [constraints].X 不一致。

  解决：构造元配置 patch，刷新 lock。
  注意：refresh 后 primitive PlanId 可能漂移，旧 evidence 不再满足新 admission。
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
  sync 进度：workspace .graft/local/remotes/<name>/last_synced
```

### 3.4 Snapshot 与 ignore 规则

当前主路径不会把 cwd 隐式捕获成 candidate。snapshot ignore 规则只用于显式 snapshot/materialize/verifier 内部路径；scratch 主路径通过 `scratch write/edit/delete [--repo <repo>] --base/--from` 明确指定文件和来源：

```
内置排除（不可关闭）：
  .graft/                         # graft 自身事实空间
  .worktrees/                     # materialize 的本地检查输出
  worktrees/                      # local managed-repo checkout/output area; not implicit source

内置拒绝：
  .git/                           # Git checkout 不是 Graft workspace 本体

用户可配（graft.toml [snapshot.ignore]）：
  patterns = ["target/**", "node_modules/**", "*.log"]
```

`[snapshot.ignore]` 模式语法是 gitignore-compatible 子集。

具体匹配规则、symlink 处理、大文件阈值等实现细节在 [§10](./reference.md) invariants 列出。

---

## 12. Workspace discovery, registry, attach

P8 的核心翻转：workspace 是 user-level 对象，cwd 只是 attach key。Graft workspace 本体保持 Git-independent：local workspace root 发现 `.git/` 时应拒绝；外部 Git checkout 只通过 repo/promote 边界登记或写入。

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
      constraints.roto
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
required = []

[promotion]
required = []

[sync]
enabled = false
```

`constraints.roto` starts as an empty comment-only constraint source. The daemon writes an empty `[constraints]` lock and relies on core application integrity (`apply(action, base, proof) == target` and `replay(base, change.ops) == target`) for default admission/materialization/promotion. Workspaces add explicit top-level constraint functions such as `empty_change`, `docs_only`, or `cargo_tests_pass` when they need policy beyond that invariant.

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
| `constraints.roto`                        | patch admit                                     |
| user code/docs/data tracked by workspace | patch admit                                     |
| `$GRAFT_HOME/registry.toml`              | daemon typed write, not a patch                 |
| `.graft/store/`*                         | daemon internal writes                          |


Meta-config patch examples:

- `graft repo add <repo_id> <url>` adds `[repos.<repo_id>]` and refreshes the matching lock entry.
- `graft repo update <repo_id>` refreshes `treeish` / `resolved_oid`.
- user adds `[promote_targets.release]`.
- user edits `constraints.roto` constraint function `cargo_tests_pass`.

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
required = []

[promote_targets.release]
path = "/Users/me/src/repo"      # may be "." after cwd resolution
branch = "main"
required = ["cargo_tests_pass"]

[promote_targets.docs]
path = "/Users/me/src/docs-repo"
branch = "graft-out"
```

Dirty policy:

- configured `promote_targets.<name>.path`: target repo/worktree must be clean before `--yes`. Dirty fails with `[E_PROMOTION_DIRTY_TARGET]`.
- explicit `--to <branch>` without configured target uses the current cwd Git repo as the external target and follows the same `--yes` side-effect boundary.

### 12.10 CLI and error-code deltas from v2

Removed / obsolete:

- cwd-root Git prohibition remains for Graft workspace roots: `.git/` is rejected instead of ignored during snapshot/capture.
- Git integration is explicit at repo/promote boundaries, not by treating the workspace root as a Git worktree.
- per-workspace daemon socket flags are removed from normal CLI help.
- `graft patch materialize <state-ref>` no longer overwrites cwd; the old `--discard` flag is accepted only as hidden compatibility no-op.
- legacy `.graft/state/` (pre-`local/` rename) and `state/cwd` no longer define a default view; `init` migrates `state/` → `local/` when present.
- `graft discard` is obsolete; cwd is not a managed view and cannot be restored from Graft state.

New / changed:

- `graft workspace init [--register-only]` is idempotent and registers local workspace roots.
- `graft workspace attach`, `graft workspace detach`, `graft workspace attach --status` manage cwd routes.
- `graft workspace ps` lists registry workspaces and daemon liveness.
- `graft workspace doctor` diagnoses stale workspace roots, stale routes, orphan daemon, registry corruption.
- `graft patch promote --to <name>` selects either a configured `[promote_targets.<name>]` target or an explicit branch name.

---
