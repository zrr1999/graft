# 可验证补丁系统设计文档

本文档是 Graft 的唯一核心设计文档。README 负责快速上手；本文档负责说明模型、边界和权衡。

> 本文档描述当前目标模型：不设 `main` 视图，不引入一等冲突或产物对象，
> 采用三层存储、同步协议和 `graft.lock` 双锚。实现迁移以本文档为准。

---

## 1. 为什么需要 Graft

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
  -> 运行时生成的 Evidence
  -> 已接纳的 Patch
  -> 目标 Promotion
```

核心判断：

```text
Evidence ⊢ Constraint(Application)
Patch = 已接纳且携带证明的 Application
```

一个补丁不因为「智能体声称完成」而可信；只有当它是带有运行时证据且已接纳的应用时，才具备可信性。

### 1.3 与 Git 的关系

Graft 不替代 Git，也不把当前目录中的 Git 仓库当成工作区本体。

- 工作区是由 `$GRAFT_HOME` 或显式 `graft workspace init` 管理的用户级对象；当前目录只作为命令路由的附着键。
- 当前目录是否是 Git 仓库与 Graft 工作区概念正交；Graft 默认不写当前目录。
- 远端 Git 仓库是 Graft 的存储分区，不提供 `main` 视图；托管平台浏览体验由 Graft 另行提供，不属于当前设计范围。
- 显式 `graft patch promote` 可把某个补丁投影到任意目标：远端 Git 引用、本地 Git 引用，或本地非提交文件。这是唯一会把可信补丁输出到外部世界的路径。

一句话：

```text
Graft 管「变更为什么可信」，并把可信变更保存在 Graft workspace 中。
Git 在外部世界依然是发布渠道和可选目标，但 Graft 自身不依赖 Git 表达补丁图。
```

### 1.4 非目标

本文档不包含：

- 替代 Git 的协作流程或托管平台。
- 完整补丁理论（例如 pijul / darcs 风格的一等冲突、交换律和重排）。
- 一等冲突对象；`compose`、`migrate` 或 `revert` 不可解时直接失败。
- 一等产物对象；当前仅保存运行时生成的证据和已声明相关输出的摘要，完整产物模型另行设计。
- 持久化或跨主机归档验证器输出。
- 中央化评审门禁；准入表示本地认可，评审在补丁层分布式发生。
- `main`、`HEAD` 等默认视图概念。
- 任何形式的主机绑定状态；`PatchId` / `EvidenceId` 不携带主机名或时间戳。

---


## 文档地图

本设计文档已按模块拆分到 `docs/design/`；完整形式化内核见 [`formal/kernel.lean`](../formal/kernel.lean)。

| 模块 | 内容 | 文件 |
| --- | --- | --- |
| 对象模型 | §2.1–§2.4 State / Action / Application / Change | [design/model.md](./design/model.md) |
| 约束与证据 | §2.5–§2.6 constraints.roto / Evidence | [design/property.md](./design/property.md) |
| 准入与关系 | §2.7–§2.8 Candidate / Patch / admit, Relation / Promotion | [design/admission.md](./design/admission.md) |
| 生命周期 | §4–§5 scratch→candidate→patch→promote, materialize / run | [design/lifecycle.md](./design/lifecycle.md) |
| 运行时 | §6–§9 Sync / Clone / Daemon / GC | [design/runtime.md](./design/runtime.md) |
| 工作区 | §3 + §12 layout / discovery / registry / attach | [design/workspace.md](./design/workspace.md) |
| 参考 | §10–§11 Invariants / 错误码 / CLI 索引 | [design/reference.md](./design/reference.md) |
| 形式内核 | Lean 核心模型 / 公理 / 定义 / 待办 | [formal/kernel.lean](../formal/kernel.lean) |
