# Graft 约束语言示例

> 本目录提供 Roto 约束语言示例。加载和锁定代码会从 `constraints.roto`
> 中发现顶层 `fn name(app: Application) -> Constraint` 函数；primitive 叶子指向内容寻址的 `Plan`。

`examples/constraints/` 下的每个文件都是可独立阅读的模式示例，也是约束语言示例的唯一维护入口。

## 约定

- 命名约束是顶层函数 `fn name(app: Application) -> Constraint`。函数名就是暴露给 Graft 的名称；没有单独的 `EmptyChange` PascalCase alias，也没有 `constraint_registry()`。
- primitive leaf 由 `primitive(observation, assertion, description)` 构造。`Plan { observation, assertion }` 内容寻址；`description` 是展示文本，不参与身份计算。
- 使用 `both(left, right)` / `either(left, right)` 或 n 元 `all_of([...])` / `either_any([...])` 组合约束。没有单独的 `requires` 列表。
- 常见 assertion 包括 `any_match`、`all_match`、`no_match`、`exit_zero`、`exit_nonzero`、`outputs_same` 和 `outputs_differ`。
- 依赖运行时的值使用符号 plan 引用：`tree.file(path)` 构造 `FileRef`，`app.previous_failure(History.First/Last/Get(n))` 构造历史 `Application` 引用。缺失的文件或 witness 会在运行需要它们时求值为 `Unknown`。
- 沙箱默认无超时、允许网络、可读取输入 tree 外部的文件系统。确定性由约束作者负责。

## 索引

| 文件 | 模式 |
| --- | --- |
| `constraints/empty_change.roto` | 基于路径集合的结构谓词 |
| `constraints/only_touches_docs.roto` | 带白名单匹配的路径策略 |
| `constraints/no_generated_artifacts.roto` | 带拒绝列表匹配的路径策略 |
| `constraints/cargo_tests_pass.roto` | 针对 `app.target()` 的命令 oracle |
| `constraints/cargo_clippy_clean.roto` | clippy 命令 oracle |
| `constraints/precision_invariance.roto` | base/target 之间的 `same_output` 关系约束 |
| `constraints/training_alignment.roto` | 带 previous-failure witness 的可证伪命令 oracle |
| `constraints/safe_patch.roto` | 通过 `both` 表达的组合示例 |
