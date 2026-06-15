# 可验证补丁系统设计文档

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
State + Action + ApplicabilityProof
  -> Application
  -> constraint obligations
  -> runtime-generated Evidence
  -> admitted Patch
  -> target Promotion
```

核心判断：

```text
Evidence ⊢ Constraint(Application)
Patch = admitted, proof-carrying Application
```

一个 patch 不因为「agent 说完成了」而可信，而因为它是带有 runtime-generated evidence 的 admitted application。

### 1.3 与 Git 的关系

Graft 不替代 Git，也不把 cwd 里的 Git 仓库当成 workspace 本体。

- workspace 是 `$GRAFT_HOME` 或显式 `graft workspace init` 管理的用户级对象；cwd 只是命令路由的 attach key。
- cwd 是否是 Git 仓库与 Graft workspace 概念正交；Graft 默认不写 cwd。
- 远端 Git 仓库是 Graft 的存储分区，没有"main 视图"，托管平台浏览体验由 Graft 自己另外提供（不在本版 scope）。
- 显式 `graft patch promote` 可把某个 patch 投影到任意 target：远端 Git ref、本地 Git ref，或本地非提交文件。这是唯一会把可信 patch 输出到外部世界的路径。

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
- 一等的 artifact 对象（v1 只保存 runtime 生成的 evidence 与 declared relevant output 摘要；完整 artifact 归后续设计）。
- 持久化或跨 host 的 verifier 输出归档。
- 中央 review gate（admission = 本地认可，review 在 patch 层分布式发生）。
- main / HEAD 等"默认视图"概念。
- 任何形式的 host-bound state（PatchId / EvidenceId 不携带 hostname / timestamp）。

---


## 文档地图

本设计文档已按模块拆分到 `docs/design/`；完整形式化 kernel 见 [`formal/kernel.lean`](../formal/kernel.lean)（唯一源，结构为核心模型 + 核心公理 + 核心定义 + TODO）。

| 模块 | 内容 | 文件 |
| --- | --- | --- |
| 对象模型 | §2.1–§2.4 State / Action / Application / Change | [design/model.md](./design/model.md) |
| 约束与证据 | §2.5–§2.6 constraints.roto / Evidence | [design/property.md](./design/property.md) |
| 准入与关系 | §2.7–§2.8 Candidate / Patch / admit, Relation / Promotion | [design/admission.md](./design/admission.md) |
| 生命周期 | §4–§5 scratch→candidate→patch→promote, materialize / run | [design/lifecycle.md](./design/lifecycle.md) |
| 运行时 | §6–§9 Sync / Clone / Daemon / GC | [design/runtime.md](./design/runtime.md) |
| 工作区 | §3 + §12 layout / discovery / registry / attach | [design/workspace.md](./design/workspace.md) |
| 参考 | §10–§11 Invariants / 错误码 / CLI 索引 | [design/reference.md](./design/reference.md) |
| 形式 kernel | Lean 核心模型 / 公理 / 定义 / TODO | [formal/kernel.lean](../formal/kernel.lean) |
