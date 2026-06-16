# Graft 工作区模板

这里提供新 Graft 工作区的起始布局。每个子目录都是完整工作区骨架，可复制或渲染到新目录作为初始内容。

| 模板 | 用途 |
| --- | --- |
| [`default/`](./default/) | `graft init` 生成的最小布局：空 `constraints.roto`，没有 admission/promotion 门禁。 |

这些模板遵循 Roto 约束语言约定：顶层 `fn name(app: Application) -> Constraint`，没有 `constraint_registry()`，也没有 PascalCase alias。默认模板刻意保持为空；工作区策略扩展时，再向 `constraints.roto` 添加约束函数。
