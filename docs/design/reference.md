# Graft 设计 · 参考（§10–§11）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化内核见 [`formal/kernel.lean`](../../formal/kernel.lean)。

## 10. 不变量与失败模式

本节集中列出全文不变量和常见失败模式，以便实现时逐个检查。

### 10.0 Lean 内核

完整形式化定义见 [`formal/kernel.lean`](../../formal/kernel.lean)。文档不重复列出 Lean 内部片段；实现时以该文件和 `lean formal/kernel.lean` 检查结果为准。

### 10.1 全文不变量总表


| Inv   | 名称                                 | 位置   |
| ----- | ---------------------------------- | ---- |
| 2.5.1 | ConstraintPlanIdentity             | [§2.5](./property.md) |
| 2.6.1 | EvidenceContentAddressing          | [§2.6](./property.md) |
| 2.6.2 | ObservedEvidenceReproducibility    | [§2.6](./property.md) |
| 3.3.1 | NoDriftingExternalReferences       | [§3.3](./workspace.md) |
| 3.3.2 | LockSchemaUniformity               | [§3.3](./workspace.md) |
| 4.4.1 | PromotionIsTheOnlyTargetProjection | [§4.4](./lifecycle.md) |
| 5.2.1 | CwdIsNotAView                      | [§5.2](./lifecycle.md) |
| 5.2.2 | NoImplicitCwdWrites                | [§5.2](./lifecycle.md) |
| 6.6.1 | EvidenceRefsAreSetUnionAcrossSync  | [§6.6](./runtime.md) |
| 7.3.1 | CloneDoesNotMaterializeByDefault   | [§7.3](./runtime.md) |
| 7.3.2 | CloneStateIsAuthoritative          | [§7.3](./runtime.md) |
| 9.3.1 | DerivedAlwaysSafeToDelete          | [§9.3](./runtime.md) |
| 9.3.2 | NoSilentLossInPublicGc             | [§9.3](./runtime.md) |
| 12.5.1 | MetaConfigIsPatch                 | [§12.5](./workspace.md) |


### 10.2 错误码总表


| Code                            | 含义                                                                     | 处理                                                              |
| ------------------------------- | ---------------------------------------------------------------------- | --------------------------------------------------------------- |
| `[E_NO_WORKSPACE]`              | 当前 cwd 没有 parent workspace、route 或 GRAFT_WORKSPACE                     | graft workspace init、graft workspace attach 或设置 GRAFT_WORKSPACE |
| `[E_NO_CONFIG]`                 | workspace 缺少 graft.toml                                                | graft workspace init 或检查 registry route                         |
| `[E_NO_WORKSPACE_CONFIG]`       | 当前命令解析到的 workspace 缺少 graft.toml/constraints                            | graft workspace init 或修复 workspace root                         |
| `[E_UNSUPPORTED_CONFIG_SCHEMA]` | graft.toml schema 不是当前支持的版本                                            | 迁移配置或降回 schema = 1                                              |
| `[E_LEGACY_ID]`                 | 输入了 `gr_` / `grc_` / `ev_` / ... 旧 ID                                              | 采用 `<kind>:<digest>`                                            |
| `[E_PROMOTION_DIRTY_TARGET]`    | promote 的 local target dirty                                           | 清理 target 后重试                                                   |
| `[E_EMPTY_CHANGE]`              | 显式要求 non-empty 的 relation transform 产出空 endpoint diff             | 放宽要求或提供实际变更                                                |
| `[E_CONSTRAINT_LOCK_MISSING]`     | `graft.lock` 缺失，普通命令不会自动重建                                             | 运行 `graft constraint lock`                                        |
| `[E_CONSTRAINT_LOCK_DRIFT]`       | constraints.roto 中 X 变但 lock 未同步                                        | 构造元配置 patch 刷新 lock                                             |
| `[E_CONSTRAINT_COMPILE]`          | constraints.roto parse/typecheck/compile 失败，或 Roto compiler panic 被隔离捕获 | 修复 constraints.roto；bug 时附最小 repro                               |
| `[E_SCOPED_CONSTRAINT_UNSUPPORTED]` | required constraint 名称带旧 scope 前缀                         | 使用 bare constraint name                                 |
| `[E_REPO_LOCK_DRIFT]`           | graft.toml repo url/default_branch 与 lock 不一致                          | graft repo update                                               |
| `[E_UNKNOWN_CONSTRAINT]`          | constraints.roto 中不存在指定的顶层 constraint 函数                                  | 修改 constraints.roto 或 graft.toml                                 |
| `[E_REPO_OID_UNAVAILABLE]`      | patch materialize 需要某 oid 但本地/远端 git 都拉不到                              | git fetch 该 oid                                                 |
| `[E_CHANGE_INTEGRITY]`          | application 的 apply/replay core integrity 校验失败                       | 重新构造 application；检查 Action/proof/change                  |
| `[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]`           | required 某 primitive 无 passed evidence                                      | graft patch validate                                            |
| `[E_CONSTRAINT_DRIFT]`            | candidate.constraint 与当前 constraints.roto 函数映射不一致；或 stable evidence reuse 重推时 constraint/stable policy 漂移 | 二选一 ([§4.3](./lifecycle.md))；复合体可补 fresh evidence ([§4.5](./lifecycle.md)) |
| `[E_PROMOTION_UNMET]`           | promote required 未满足                                        | 补 evidence 后重试                                                  |
| `[E_PROMOTION_NOT_FF]`          | remote-push target ref 不能 fast-forward                                           | `--force-push` 或调整 base                                           |
| `[E_PROMOTION_TARGET_UNKNOWN]`  | --to 未在 graft.toml 的 [promote_targets] 声明                              | 补上 [promote_targets.]                                           |
| `[E_COMPOSE_CONFLICT]`          | compose / migrate / revert 遇 conflict；不建模一等冲突对象                           | 手动 candidate ([§4.5](./lifecycle.md))                                             |
| `[E_SCRATCH_LOST]`              | daemon 重启后 scratch 状态失效                                                | 用 `--base <base>` 重新开始                                          |
| `[E_SYNC_DEFAULT_WORKSPACE]`    | `ws:default` 是 machine-local workspace，不能 sync                         | 创建或 attach 一个 local workspace                                   |
| `[E_SYNC_DISABLED]`             | 当前 workspace 显式设置了 `[sync] enabled = false`                              | 删除该 override，或在 graft.toml 设置 `[sync] enabled = true`          |
| `[E_SYNC_REMOTE_REQUIRED]`      | `graft sync` 未传 `<remote>` 且 workspace 还没有 `.graft/local/remotes/default` | 先运行一次 `graft sync <remote>`                                      |
| `[E_SYNC_REMOTE_INVALID]`       | sync remote 不存在、不是 Git 仓库/URL，或已有非空非 Git 内容                                | 检查 remote 路径/URL；本地首次 push 可用空目录或新路径                                  |
| `[E_SYNC_DIVERGENCE]`           | sync manifest history 与本地 last_synced 不兼容                              | 先 fetch/review；或在允许 fetch 的 sync 中显式 `--on-divergence keep-remote` |
| `[E_REMOTE_INCOMPLETE]`         | remote refs 指向的 facts/blobs/manifests object 缺失或不可解析              | 检查/修复 remote；不要接受这次 sync                              |


### 10.3 常见状态转换错误

```text
未登记 cwd 里执行 scratch / candidate 命令:
  cwd 只参与 workspace route lookup；未命中时以 [E_NO_WORKSPACE] 失败。
  若要使用 machine-local ws:default，必须显式 `graft attach`。
  scratch 第一版必须显式 `--base <base>`，不会从 cwd 或 repo path 推断 base。
  expected 由 resolved workspace 的 graft.toml/constraints 提供。

constraints 改名后老 evidence 表现:
  顶层函数名只影响配置/lock key，不进入 primitive PlanId；若 body 不变，旧 primitive evidence 仍可查询。
  但需要刷新 graft.lock 的 [constraints] 映射。

constraints 改 observation/assertion 后老 evidence 表现:
  primitive PlanId 漂移 X -> X'。
  老 evidence 仍在、仍可查，但对当前 constraint primitive 在 admission 中不再有效。
  graft patch show patch:X --evidence 标 "constraint drift; was X, now X'"。
  重新 admission 需要 graft patch validate <X> --expect <name> 补 X' evidence。

fetch 后 patch 未本地验证:
  graft patch show patch:X 渲染 "cargo_tests_pass ✓ (referenced by remote, not yet locally
  rebuilt)"。admission 查询 evidence body 在 store/derived/ 缺失 -> [E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]。
  提示跑 graft patch validate patch:X --expect <constraint> 或 graft verify-pending。
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

本节以动词为主列举 CLI 接口。详细参数参考各节。

### 帮助 / 智能体指引

```bash
graft explain agent-workflow       # 推荐智能体/tool 主流程；pi-graft graft_help 的默认 topic
graft explain workflow             # 同一流程的短 alias
graft explain <topic-or-command>    # scratch/candidate/admit/materialize 等概念说明
graft explain <diagnostic-or-constraint>
```

`agent-workflow` 是仓库维护的流程说明：scratch 只负责草稿，`graft patch from-scratch` 生成 candidate，`graft patch admit` 生成 patch，`graft patch materialize` / `graft run` 用于检查输出；外部 `graft patch promote`、sync、compose/migrate/revert、`repo add/sync/lock/update`、`bundle import`、`workspace gc --apply` 等低频写命令可走手动 CLI 或 pi-graft `graft_cli_exec` argv。读/检查命令（如 patch show/search/incoming、repo list、bundle export、workspace gc dry-run）保留本地 CLI 路径，不经 `cli_exec`。

### 引导 / 工作区路由

```bash
graft workspace init [--register-only]       # 创建或登记当前目录本地工作区
graft workspace attach [--workspace <id>]    # cwd -> workspace route；git cwd 自动登记 repo
graft workspace detach                       # 删除 cwd route，不删 workspace repos
graft workspace attach --status              # 展示 cwd lookup 命中层与 route
graft workspace ps                           # 列 registry workspaces + daemon liveness
graft workspace doctor [--rebuild-registry]  # 检查 registry/daemon/workspace 健康
```

### 状态 / 视图

```bash
graft workspace status                      # cwd lookup + workspace + daemon 状态
graft patch diff <a> <b>                    # object-to-object diff
graft patch materialize <state-ref> [--dry-run]   # 默认写 <workspace-root>/.worktrees/<state-slug>/
graft run <state-ref> [--cwd <path>] -- ... # 临时物化完整 state 并丢弃命令写入
```

### 生命周期

```bash
graft scratch write/edit/delete [--repo <repo-id>] (--base <base> | --from <scratch-id>) ...
graft patch from-scratch <scratch-id> [--expect <constraint>...] [--producer <label>] [--message <msg>]
graft patch validate <id> [--expect <constraint>...]
graft patch admit <candidate-id> [--require <constraint>...]
graft patch promote <patch-id> --to <target-or-branch> [--require <constraint>...]
```

### 推广目标

```toml
[promotion]
required = []

[promote_targets.release]
path = "../external-git-repo"              # 必须 clean；dirty -> E_PROMOTION_DIRTY_TARGET
branch = "main"
required = ["cargo_tests_pass"]

[promote_targets.docs]
path = "../docs-repo"
branch = "graft-out"
```

`graft patch promote <patch> --to release --yes` 会写入已配置的 repo/ref。`graft patch promote <patch> --to main` 未命中 `promote_targets.main` 时表示显式分支试运行；只有传入 `--yes` 才产生写入。

### 同步与协作

```bash
graft sync [<remote>] [--fetch-only|--push-only]
graft patch incoming
graft verify-pending                        # 隐藏兼容维护命令；为 store/public 中的 refs 本地重建 evidence
```

`ws:default` 禁用同步；其他工作区默认启用同步，除非显式配置 `[sync] enabled = false`。

### 检查

```bash
graft patch show <id> [--evidence] [--change]
graft patch search [--constraint <constraint>] [--base <id>] [--producer <label>] [--has-evidence <constraint>]
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

### Scratch 草稿

```bash
graft scratch read [--repo <repo-id>] [--base <base> | --from <scratch-id>] <path> --mode <hashlines|...>
graft scratch write [--repo <repo-id>] [--base <base> | --from <scratch-id>] <path> --content <bytes>
graft scratch edit [--repo <repo-id>] [--base <base> | --from <scratch-id>] <path> --edits <json>
graft scratch delete [--repo <repo-id>] [--base <base> | --from <scratch-id>] <path>    # alias: rm
graft scratch diff <from-scratch-id> <to-scratch-id>
graft scratch drop <scratch-id>
graft scratch pin / unpin
graft scratch status

graft patch from-scratch <scratch-id> --expect <constraint>... --message <msg>
```

`scratch` namespace 只负责临时草稿读写、编辑、删除、diff、pin/drop；第一次操作用 `--base` 隐式创建 root scratch，或在 `--base`/`--from` 都省略时使用进程环境 `GRAFT_BASE_REF`。显式 `--base` 优先于环境，显式 `--from` 不读取环境；缺少三者时命令以 `E_MISSING_BASE` 失败。Rename 由 delete+write 表达，candidate / patch 生成在 scratch 外部完成。`graft patch from-scratch` 是 scratch→candidate 的规范生命周期命令；CLI 与 pi-graft 插件共同调用 daemon `candidate_from_scratch` wire op（request: `scratch`, `expected`, `producer`, `message`; response: `candidate`, `changed_paths`）。

### 维护

```bash
graft workspace gc [--apply] [--derived-only]
graft bundle export <path>
graft bundle import <path>                  # 仅接受当前 bundle；拒绝旧 patch 字段
graft bundle import --upgrade-from-v1 <path> # 显式一次性迁移旧 constraints/admitted_at 到 Constraint
graftd status                               # 由 graft-cli 安装；使用 $GRAFT_HOME/run/daemon.sock
graftd stop
```

在 Constraint lattice 之前创建的旧 registry bundle 不会被静默接受。当 candidate 正文仍包含 `expected`，或 patch 正文仍包含 `properties` / `admitted_at` 时，普通 `graft bundle import <path>` 会以 `[E_UNSUPPORTED_STORE_SCHEMA]` 失败。仅对可信旧 bundle 使用 `--upgrade-from-v1`；它会把 candidate 的 `expected: [...]` 改写为 `constraint = all_of(...)`，把 patch 的旧 `properties: [...]` 改写为 `constraint = all_of(<constraint>...)`，删除 `admitted_at`，在 `admission.constraint` 中记录同一迁移后约束，并在写入前重新计算迁移后的 candidate/patch id。

### 验证与开发卫生

```bash
just check      # cargo fmt --all -- --check + cargo clippy --locked --workspace --all-targets -- -D warnings
just test       # cargo test --locked --workspace --all-targets + cargo test --locked --doc --workspace
just smoke      # fail-fast tests/*.sh
just prek       # uvx prek run --all-files
just cov        # cargo llvm-cov test --locked --workspace --all-targets，写入 lcov.info
```

---
