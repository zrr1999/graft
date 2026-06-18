# Graft

Graft 是兼容 Git、但不依赖 Git 的约束感知补丁运行时。它不把当前目录当作 Git worktree，也不维护 `main` 视图；`.graft/` 是唯一事实空间，远端 Git 仓库只作为 `refs/graft/*` 的内容寻址存储分区。

核心问题：

```text
一组人类或智能体产生的文件修改，什么时候可以被认为是可信 patch？
```

Graft 的生命周期回答是：

```text
scratch operation -> candidate -> validate evidence -> admit patch -> materialize/run -> promote/sync
```

一句话：Graft 管「变更为什么可信」，Git 只在显式 `graft patch promote` 时承载「可信变更如何进入外部版本历史」。

## 帮助入口 / 智能体流程

仓库维护的推荐使用流程入口是：

```bash
graft explain agent-workflow
```

pi-graft 的 `graft_help` 工具应默认展示这个 topic；具体概念继续用 `graft explain scratch`、`graft explain candidate`、`graft explain admit`、`graft explain materialize` 等查询。高频智能体主路径是 scratch 草稿（`graft scratch read|write|edit|delete --base/--from ...`）→ `graft patch from-scratch` → `graft patch validate` → `graft patch admit` → `graft patch materialize` / `graft run` 检查输出；只有在已有当前目录 dirty state 时，才用 `graft scratch capture --base <ref>` 显式桥接并恢复当前目录。外部 `graft patch promote`、sync、compose/migrate/revert、`repo add/sync/lock/update`、`bundle import`、`workspace gc --apply` 等低频写命令可通过手动 CLI 或 pi-graft `graft_cli_exec` argv 显式执行，读/检查命令保留本地 CLI 路径。pi-graft typed tools 已验证为显式 UTF-8 文本 read/write/edit/delete 替代路径：首次草稿操作只传 `base`，后续只传 `from`，同一 workspace/daemon 上的 lifecycle 与 inspection 调用应顺序执行而不是并行执行。

## 当前状态

这是一个正在迭代的 Rust 项目，已覆盖三层存储主路径：

- workspace 是用户级对象：`$GRAFT_HOME` 默认 `~/.graft`，cwd 只是 attach/discovery key；Graft workspace root 拒绝 `.git/`，外部 Git 只通过 repo/promote 边界进入。
- `.graft/store/{public,private,derived}` 分层：candidate local-only；admitted patch / relation / promotion / evidence_refs 位于 public；evidence body 位于 derived，不参与 sync。
- `constraints.roto` + `graft.lock`：顶层 constraint 函数与 content-addressed `constraint:<digest>` 解耦；锁定的是静态 plan identity。
- daemon 是唯一 writer：CLI 写命令自动通过 `$GRAFT_HOME/run/daemon.sock` 发给全局 `graftd`；无 inline writer 路径。
- `StateId` 是完整 workspace snapshot；constraint 始终在完整 `.base` / `.target` state 上运行。
- materialize 不写 cwd：`graft patch materialize <patch-id>` 把 admitted patch 的 target state 解析成确定 state，再输出到 workspace `.worktrees/<state>/` 临时检查目录；`graft run <state-ref> -- <cmd>` 仍可对 `tree:*`、`candidate:*`、`patch:*` 或 `repo:<id>@<treeish>` 在临时 state root 执行命令并丢弃写入。
- sync 使用固定 Git refs：`refs/graft/facts`、`refs/graft/blobs`、`refs/graft/manifests`；candidate 和 evidence body 不 sync。
- `graft get` 只重建 `.graft/`，不会默认 materialize cwd。
- `graft patch promote` 是唯一会写外部 Git repo / branch / PR / release 的命令，发布的是 admitted patch 的 `target_state`，并记录 `promotion:<digest>`。

## 安装 / 构建

```bash
cargo build -p graft-cli
```

`graft-cli` 统一产出用户入口：

```text
target/debug/graft
target/debug/graftd
```

PyPI 发行包名预留为 `graftkit`，命令名仍是 `graft`。

## 快速开始

在非 Git 目录初始化：

```bash
mkdir demo && cd demo
graft workspace init
```

创建 candidate：

```bash
scratch=$(graft scratch write --base graft:empty hello.txt --content $'hello\n' | grep -oE 'scratch:[0-9a-f]+' | tail -n1)
candidate=$(graft patch from-scratch "$scratch" --message 'first candidate' | grep -oE 'candidate:[0-9a-f]+' | head -n1)
```

如果已经在 cwd 里有 dirty files，可以用 stash-like capture 进入同一条生命周期；capture 会拒绝 `.git/`，默认只跳过 `.graft/`、`.worktrees/` 和 `worktrees/`，并会跟踪 `graft.toml`、`graft.lock`、`constraints.roto` 等 workspace 元配置，成功写入 scratch 后把被捕获路径恢复到 base：

```bash
scratch=$(graft scratch capture --base graft:empty | grep -oE 'scratch:[0-9a-f]+' | head -n1)
candidate=$(graft patch from-scratch "$scratch" --message 'captured cwd' | grep -oE 'candidate:[0-9a-f]+' | head -n1)
```

验证 application core integrity 并接纳（默认没有 constraint gate）：

```bash
graft patch validate "$candidate"
patch=$(graft patch admit "$candidate" | grep -oE 'patch:[0-9a-f]+' | head -n1)
```

查看 admitted patch：

```bash
graft patch show "$patch" --evidence --change
```

把 patch target state 显式物化到 workspace 临时检查目录：

```bash
graft patch materialize "$patch"
ls .worktrees/
graft run "$patch" -- test -f hello.txt
graft workspace status
```

## 工作区布局

```text
.graft/
  config.toml
  store/
    public/{blob,tree,action,application,change,patch,evidence_refs,relation,promotion}/
    private/{candidate,evidence_refs,relation}/
    derived/evidence/
  local/{aliases/,index.sqlite,remotes/}
  run/{daemon.sock,daemon.pid,trials/,worktrees/,tmp/}

graft.toml
constraints.roto
graft.lock
worktrees/         # 本地受管仓库 checkout/output 区域；cwd capture 会忽略它
```

`graft.lock` 是派生锚点：`[constraints]` 固定 constraint content IDs，`[repos.<id>]` 固定外部 repo treeish 解析结果。它属于 Graft 跟踪的 workspace 元配置锁，确保 clone/get 后解析一致。

## ID 形式

```text
tree:<digest>
action:<digest>
application:<digest>
change:<digest>
constraint:<digest>
evidence:<digest>
candidate:<digest>
patch:<digest>
relation:<digest>
promotion:<digest>
scratch:<digest>
manifest:<digest>
```

`blob` 使用 raw bytes blake3，不带 typed prefix。旧 `gr_/grc_/ev_/ch_/gt_` 输入会以 `[E_LEGACY_ID]` 失败。

## 约束

Constraint 定义位于单个 `constraints.roto`。每个顶层 `fn name(app: Application) -> Constraint` 都是一个命名约束；函数名是配置和 CLI 里使用的名字，没有 PascalCase alias，也没有 `constraint_registry()`。primitive 叶子由 `Plan { observation, assertion }` 内容寻址，函数名和描述只是标签。例如：

```roto
fn empty_change(app: Application) -> Constraint {
    primitive(app.changed_paths(["**"]), no_match, "the change touches no paths")
}

fn cargo_tests_pass(app: Application) -> Constraint {
    primitive(
        app.run(["cargo", "test", "--all-targets"]),
        exit_zero,
        "cargo tests pass",
    )
}

fn safe_patch(app: Application) -> Constraint {
    both(empty_change(app), cargo_tests_pass(app))
}
```

常用命令：

```bash
graft constraint lock
graft constraint check
graft constraint list
graft constraint show cargo_tests_pass
```

## Daemon

写命令默认自动启动 `$GRAFT_HOME` 级全局 daemon：

```text
$GRAFT_HOME/run/daemon.sock
$GRAFT_HOME/run/daemon.pid
```

`graftd` 串行执行 wire op，CLI 请求携带 workspace 路由信息。可在 workspace `.graft/config.toml` 调整 idle timeout：

```toml
[daemon]
idle_timeout_minutes = 30
```

手动检查/停止：

```bash
graftd status --socket "$GRAFT_HOME/run/daemon.sock"
graftd stop   --socket "$GRAFT_HOME/run/daemon.sock"
```

## 同步 / 克隆

Graft remote 是 Git 仓库，但只使用固定 storage refs：

```text
refs/graft/facts
refs/graft/blobs
refs/graft/manifests
```

同步 public store：

```bash
# enabled by default for normal workspaces; set false to opt out:
# [sync]
# enabled = true
graft sync /path/to/storage.git
graft sync                         # reuse the last explicit sync remote
graft sync /path/to/storage.git --fetch-only
graft sync /path/to/storage.git --push-only
graft patch incoming
graft verify-pending
```

`ws:default` 永不同步。其他工作区默认同步，除非在 `graft.toml` 中设置 `[sync] enabled = false`。第一次显式执行 `graft sync <remote>` 会记录该工作区的默认 remote；后续 `graft sync` 使用这个已记录 remote。

`evidence_refs` 会同步；`store/derived/evidence/` 不同步，新 clone 需要通过 `graft verify-pending` 在本地重建。

克隆不物化当前目录：

```bash
graft get /path/to/storage.git ./clone
cd clone
graft patch incoming
graft patch materialize patch:<digest>
```

## 推广

`graft patch promote` 是唯一会写外部 Git repo 的路径。推荐在 `graft.toml` 配置 target：

```toml
[promotion.required]

[promote_targets.docs]
path = '../external-git-repo'
branch = 'graft-out'
required = ['only_touches_docs']
```

执行：

```bash
graft patch promote patch:<digest> --to docs --yes
```

## Scratch 草稿

`scratch` 是由 daemon 支持的临时草稿状态图。第一次读、写、编辑或删除直接用 `--base`，后续草稿变更用 `--from` 续写；`--repo <id>` 只用于指定 `--base` 的 repo 上下文，不写则使用 workspace。candidate 生成不属于 `scratch` namespace，而是独立的 candidate 生命周期入口：

```bash
graft scratch read --base patch:<digest> path/to/file --mode hashlines
graft scratch read --repo C --base main graft.toml --mode text
graft scratch write --base patch:<digest> new.txt --content $'hello
'
graft scratch edit --from scratch:<digest> file.txt --edits '[...]'
graft scratch delete --from scratch:<digest> file.txt   # alias: rm
graft scratch diff scratch:<before> scratch:<after>
graft scratch pin scratch:<digest>
graft scratch unpin lease_<digest>
graft scratch drop scratch:<digest>

graft patch from-scratch scratch:<digest> --expect only_touches_docs --message 'ready for validation'
```

`graft patch from-scratch` 调用 daemon `candidate_from_scratch` protocol；CLI 与 pi-graft 插件共享这个规范 op 来写 change、candidate 与空 evidence index。旧的 `graft candidate from-scratch` 仍作为隐藏兼容入口接受，但 README 使用当前 help 暴露的 `patch` namespace。Rename 用 `scratch delete --from <scratch> old/path` 加 `scratch write --from <scratch> new/path --content ...` 表达。

## 开发检查

本地与 PR 门禁使用同一组入口：

```bash
just check      # cargo fmt --all -- --check + cargo clippy --locked --workspace --all-targets -- -D warnings
just test       # cargo test --locked --workspace --all-targets + cargo test --locked --doc --workspace
just smoke      # fail-fast 执行 tests/*.sh
just prek       # uvx prek run --all-files
just cov        # cargo llvm-cov test --locked --workspace --all-targets，生成 lcov.info
```

`just smoke` 会逐个执行 `tests/*.sh`，任一 smoke 失败即停止。CI 的 static/test workflow 调用同一组 `just` recipe；test workflow 额外用 `just cov` 生成覆盖率上传。PR 还会运行标题与正文模板检查。
