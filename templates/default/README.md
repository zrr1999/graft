# 默认工作区模板

`graft init` 生成的最小布局：

```text
graft.toml          # 工作区配置（admission、promotion、sync）
graft.lock          # constraint/repo 解析锁（应提交）
constraints.roto     # 约束源文件（默认空）
.gitignore          # 只忽略本地 Graft 状态
```

该模板表示声明任何策略前的新工作区形态。它只受 Graft application core integrity 不变量约束：`apply(action, base, proof) == target` 且 `replay(base, change.ops) == target`。

## 添加约束

1. 在 `constraints.roto` 中添加函数：

   ```roto
   fn empty_change(app: Application) -> Constraint {
       primitive(app.changed_paths(["**"]), no_match, "the change touches no paths")
   }
   ```

2. 在 `graft.toml` 中引用它：

   ```toml
   admission.required = ["empty_change"]
   ```

3. 刷新 lock 并重新运行 admission：

   ```sh
   graft constraint lock
   graft patch validate <candidate>
   ```

`examples/constraints/` 提供可复用的单模式约束示例。
