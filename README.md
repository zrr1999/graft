# Graft

Graft 是一个 Git-compatible、但 Git-independent 的 property-aware patch runtime。它不把当前目录当作 Git worktree，也不维护 `main` view；`.graft/` 是唯一事实空间，远端 Git 仓库只作为 `refs/graft/*` 的 content-addressed storage partition。

核心问题：

```text
一组人类/agent 产生的文件修改，什么时候可以被认为是可信 patch？
```

Graft 的生命周期回答是：

```text
cwd/scratch -> candidate -> validate evidence -> admit patch -> sync/promote/materialize
```

一句话：Graft 管「变更为什么可信」，Git 只在显式 `graft promote` 时承载「可信变更如何进入外部版本历史」。

## 当前状态

这是一个正在迭代的 Rust 项目，当前实现已覆盖 v2 store-tier 主路径：

- 纯 Graft workspace：cwd root 不允许 `.git/`，遇到 Git worktree 以 `[E_GIT_IN_WORKSPACE]` 失败。
- `.graft/store/{public,private,derived}` 分层：candidate local-only；admitted patch / relation / promotion / evidence_refs 位于 public；evidence body 位于 derived，不参与 sync。
- `properties/<Alias>.toml` + `graft.lock`：property alias 与 content-addressed `property:<digest>` 解耦。
- daemon 是唯一 writer：CLI 写命令自动通过 `.graft/run/daemon.sock` 发给 `graftd`；无 inline writer 路径。
- cwd view 显式管理：`graft materialize` 写 cwd；`graft status/diff/discard` 管理 `.graft/state/cwd` 与 dirty gate。
- sync 使用固定 Git refs：`refs/graft/facts`、`refs/graft/blobs`、`refs/graft/manifests`；candidate 和 evidence body 不 sync。
- `graft clone` 只重建 `.graft/`，不会默认 materialize cwd。
- `graft promote` 是唯一会写外部 Git repo 的命令，并记录 `promotion:<digest>`。

## 安装 / 构建

```bash
cargo build -p graft-cli -p graft-daemon
```

得到：

```text
target/debug/graft
target/debug/graftd
```

PyPI 发行包名预留为 `graftkit`，命令名仍是 `graft`。

## 快速开始

在非 Git 目录初始化：

```bash
mkdir demo && cd demo
graft init
```

创建 candidate：

```bash
printf 'hello
' > hello.txt
graft create --from graft:empty --expect ValidPatch --message 'first candidate'
```

验证并接纳：

```bash
graft validate candidate:<digest> --expect ValidPatch
graft admit candidate:<digest> --require ValidPatch
```

查看与搜索 admitted patch：

```bash
graft show patch:<digest> --evidence --change
graft search --property ValidPatch
graft search --has-evidence ValidPatch
```

把 patch 显式设为 cwd view：

```bash
graft materialize patch:<digest>
graft status
graft diff
graft discard
```

## Workspace layout

```text
.graft/
  config.toml
  store/
    public/{blob,tree,change,patch,evidence_refs,relation,promotion}/
    private/{candidate,evidence_refs,relation}/
    derived/evidence/
  state/{cwd,aliases/,index.sqlite}
  run/{daemon.sock,daemon.pid,trials/,worktrees/,tmp/}

graft.toml
properties/*.toml
graft.lock
```

`graft.lock` 是 derived anchor：`[properties]` 固定 property content IDs，`[repos.<id>]` 固定外部 repo treeish 解析结果。它不属于 cwd snapshot。

## ID 形式

```text
tree:<digest>
change:<digest>
property:<digest>
evidence:<digest>
candidate:<digest>
patch:<digest>
relation:<digest>
promotion:<digest>
scratch:<digest>
manifest:<digest>
```

`blob` 使用 raw bytes blake3，不带 typed prefix。旧 `gr_/grc_/ev_/ch_/gt_` 输入会以 `[E_LEGACY_ID]` 失败。

## Properties

Property 定义位于 `properties/<Alias>.toml`，文件名就是 alias，文件内容是 verifier spec。例如：

```toml
kind = 'builtin'
check = 'valid_patch'
```

常用命令：

```bash
graft property lock
graft property check
graft property list
graft property show ValidPatch
```

## Daemon

写命令默认自动启动 per-workspace daemon：

```text
.graft/run/daemon.sock
.graft/run/daemon.pid
```

`graftd` 串行执行 wire op；启动时会清理 `.graft/run/{trials,worktrees,tmp}`。可在 `.graft/config.toml` 调整 idle timeout：

```toml
[daemon]
idle_timeout_minutes = 30
```

手动检查/停止：

```bash
graftd status --socket .graft/run/daemon.sock
graftd stop   --socket .graft/run/daemon.sock
```

## Sync / clone

Graft remote 是 Git 仓库，但只使用固定 storage refs：

```text
refs/graft/facts
refs/graft/blobs
refs/graft/manifests
```

同步 public store：

```bash
graft sync /path/to/storage.git
graft sync /path/to/storage.git --fetch-only
graft sync /path/to/storage.git --push-only
graft incoming
graft verify-pending
```

`evidence_refs` 会 sync；`store/derived/evidence/` 不 sync，fresh clone 需要 `graft verify-pending` 本地重建。

Clone 不 materialize cwd：

```bash
graft clone /path/to/storage.git ./clone
cd clone
graft incoming
graft materialize patch:<digest>
```

## Promote

`graft promote` 是唯一会写外部 Git repo 的路径。推荐在 `graft.toml` 配置 target：

```toml
[promotion]
required_properties = ['ValidPatch']

[promote_targets.docs]
path = '../external-git-repo'
branch = 'graft-out'
required_properties = ['ValidPatch']
```

执行：

```bash
graft promote patch:<digest> --to docs --yes
```

## Scratch

`scratch` 是 daemon-backed 临时状态图：

```bash
graft scratch open --base patch:<digest>
graft scratch read scratch:<digest> path/to/file --mode hashlines
graft scratch write scratch:<digest> new.txt --content $'hello
'
graft scratch edit scratch:<digest> file.txt --edits '[...]'
graft scratch promote scratch:<digest> --expect ValidPatch --message 'from scratch'
```

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --doc --workspace
```

Smoke tests 在 `tests/*.sh`。
