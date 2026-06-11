# Graft 设计 · 参考（§10–§11）

> 本文件是 Graft 设计文档的一个模块，从 [`../design.md`](../design.md)（索引）拆出。
> 完整形式化 kernel 见 [`../graft-kernel.lean`](../graft-kernel.lean)。

## 10. Invariants and failure modes

本节集中列出全文不变量和常见失败模式，以便实现时逐个检查。

### 10.0 Lean kernel summary

完整 kernel 在 [`docs/graft-kernel.lean`](../graft-kernel.lean)（唯一源，结构为核心模型 +
核心公理 + 核心定义 + TODO）；本表只给 fragment → section 映射。

| Kernel fragment | Owning section | Implementation consequence |
| ---------------- | -------------- | -------------------------- |
| `State`, `Action`, `sem`, `idAction`, `composeAction`, `sem_id`, `sem_seq` | [§2.3](./model.md) Action semantics in Lean | `sem : Action → State → Option State` 是自由 action 幺半群→`Option`-Kleisli 同态；`Sequence` 是 n 元拍平，由结合律保障 sound。 |
| `Application`, `applicable`, `Application.target`, `composable`, `composeApplicable`, `composeApplication`, `targetCompose` | [§2.4](./model.md) Application algebra in Lean | `Application` 只存 base/action/valid；`target` 是派生量（不存）；compose 需显式端点 link。 |
| `Constraint`, `satisfies`, `top`, `bottom`, `both`, `either`, `satisfies_*` | [§2.5](./property.md) Constraint lattice | admission/promotion 用单一 `Constraint` AST；primitive 是被解析的属性引用。 |
| `Stable`, `stable_top`, `stable_bottom`, `stable_both`, `certifyComposedStable` | [§2.5](./property.md) Constraint lattice | 约束传递性：`Stable c` 表“C 顺序组合下封闭”；`⊓` 封闭、`⊔` 不封闭；不授权跳过 evidence。 |
| `Graft`, `certify`, `certifyComposed` | [§2.7](./admission.md) Candidate, patch, admit | `Patch` 是持久化的 proof-carrying `Graft`；`admit` 是 runtime 的 certification 步骤。 |
| `wellformed`/`consistent`/`resolved`（TODO） | [§2.5](./property.md) / [§2.7](./admission.md) | 状态三分未落地，作为 kernel 末尾 TODO。 |

### 10.1 全文不变量总表


| Inv   | 名称                                 | 位置   |
| ----- | ---------------------------------- | ---- |
| 2.5.1 | PropertyNameIsIdentityInput        | [§2.5](./property.md) |
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
| `[E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]`           | required 某 primitive 无 passed evidence                                      | graft patch validate                                            |
| `[E_PROPERTY_DRIFT]`            | candidate.constraint 与当前 properties.roto 函数映射不一致                         | 二选一 ([§4.3](./lifecycle.md))                                                      |
| `[E_PROMOTION_UNMET]`           | promote required_properties 未满足                                        | 补 evidence 后重试                                                  |
| `[E_PROMOTION_NOT_FF]`          | remote-push target ref 不能 FF                                           | --force-push 或调整 base                                           |
| `[E_PROMOTION_TARGET_UNKNOWN]`  | --to 未在 graft.toml 的 [promote_targets] 声明                              | 补上 [promote_targets.]                                           |
| `[E_COMPOSE_CONFLICT]`          | compose / migrate / revert 遇 conflict，v1 不建模                           | 手动 candidate ([§4.5](./lifecycle.md))                                             |
| `[E_SCRATCH_LOST]`              | daemon 重启后 scratch 状态失效                                                | 用 `--base <base>` 重新开始                                          |
| `[E_SYNC_DEFAULT_WORKSPACE]`    | `ws:default` 是 machine-local workspace，不能 sync                         | 创建或 attach 一个 local workspace                                   |
| `[E_SYNC_DISABLED]`             | 当前 workspace 显式设置了 `[sync] enabled = false`                              | 删除该 override，或在 graft.toml 设置 `[sync] enabled = true`          |
| `[E_SYNC_REMOTE_REQUIRED]`      | `graft sync` 未传 `<remote>` 且 workspace 还没有 `.graft/local/remotes/default` | 先运行一次 `graft sync <remote>`                                      |
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
  rebuilt)"。admission 查询 evidence body 在 store/derived/ 缺失 -> [E_CONSTRAINT_UNMET] / [E_ADMISSION_UNMET]。
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
graft bundle import <path>                  # v2 bundles only; rejects legacy v1 patch fields
graft bundle import --upgrade-from-v1 <path> # explicit one-shot rewrite of v1 properties/admitted_at into Constraint
graftd status                               # installed by graft-cli; uses $GRAFT_HOME/run/daemon.sock
graftd stop
```

Legacy registry bundles created before the Constraint lattice are not silently accepted. A plain `graft bundle import <path>` fails with `[E_UNSUPPORTED_STORE_SCHEMA]` when candidate bodies still contain `expected` or patch bodies still contain `properties` / `admitted_at`. Use `--upgrade-from-v1` only for trusted legacy bundles; it rewrites candidate `expected: [...]` to `constraint = all_of(...)`, patch `properties: [...]` to `constraint = all_of(<property>...)`, drops `admitted_at`, records the same upgraded constraint in `admission.constraint`, and recomputes upgraded candidate/patch ids before writing them.

### Validation & dev hygiene

```bash
just check      # cargo fmt --all -- --check + cargo clippy --locked --workspace --all-targets -- -D warnings
just test       # cargo test --locked --workspace --all-targets + cargo test --locked --doc --workspace
just smoke      # fail-fast tests/*.sh
just prek       # uvx prek run --all-files
just cov        # cargo llvm-cov test --locked --workspace --all-targets, writes lcov.info
```

---
