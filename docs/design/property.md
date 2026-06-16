# §2.5–§2.6 约束语言与证据

## 2.5 约束语言：`constraints.roto` → `ConstraintDef` → `Constraint` → `Plan`

Graft 使用单个工作区约束源文件：

```text
constraints.roto
```

Graft 使用宿主提供的 Roto 运行时对 `constraints.roto` 做类型检查。如下形态的顶层函数定义一个命名约束：

```roto
fn cargo_tests_pass(app: Application) -> Constraint {
    primitive(
        observe_run(call(["cargo", "test", "--workspace"], app.target())),
        exit_code_is(0),
        "workspace tests pass",
    )
}
```

目标实现模型分为三层：

```text
ConstraintDef { name, description, body: Constraint }

Constraint =
  | Top
  | Bottom
  | Primitive { plan: PlanId }
  | Both { left: Constraint, right: Constraint }
  | Either { left: Constraint, right: Constraint }

Plan { observation: ObservationPlan, assertion }
PlanId = blake3(canonical(observation, assertion))
```

Lean 内核把 primitive leaf 抽象命名为 `ConstraintPrimitive`；实现层用 `PlanId` 承载该语义。

关键分离规则：

- `ConstraintDef.name` 是用户可见的顶层函数名，也是 lock/config key；它不属于 primitive 身份。
- `ConstraintDef.description` 是展示和帮助元数据；它不属于 primitive 身份。
- `Constraint::Primitive` 直接指向内容寻址的 `PlanId`。
- `PlanId` 只由规范化的 observation/assertion 对派生。
- `ObservationPlan` 是实现层的观测方案，用于说明要观测什么；它不是 Lean 内核中的语义 `Observation` 对象。
- 逻辑组合只由 `Constraint::{Both,Either}` 表示；`all_of` / `either_any` 是基于二元节点的 n 元折叠助手，不是额外的检查级身份层。
- 当前模型没有带外优先级或依赖字段。必需策略通过在准入或推广位置组合约束来表达。

### 2.5.1 约束计划身份

Invariant 2.5.1 (ConstraintPlanIdentity)

```text
PlanId = blake3(canonical(observation, assertion))
```

名称、描述、锁键、源码位置、策略位置和展示文本都不进入 `PlanId`。重命名顶层 Roto 函数只改变人和配置引用约束的方式；只要 observation/assertion 正文不变，primitive 身份就不变。

这为 Graft 提供两个稳定轴：

1. **人和配置轴**：`ConstraintDef.name` 与 `graft.lock [constraints.<name>]` 让用户按名称引用约束。
2. **证据和准入轴**：证据引用验证器实际观测并断言的 `PlanId` 叶子。

### 2.5.2 Roto 宿主接口

Roto 对外接口刻意保持精简：

```roto
fn name(app: Application) -> Constraint

primitive(observation, assertion, description) -> Constraint
both(left, right) -> Constraint
either(left, right) -> Constraint
all_of(list) -> Constraint
either_any(list) -> Constraint
observe_run(run) -> Observation
call(argv, tree) -> RunPlan
```

Roto 接口为兼容保留历史类型名 `Observation`；Rust 实现类型为 `ObservationPlan`，表示构造 `Plan` 的方案，而不是 Lean 内核中的语义对象。

代表性示例：

```roto
fn changed_docs(app: Application) -> Constraint {
    primitive(
        app.changed_paths(["docs/**"]),
        any_match,
        "docs changed",
    )
}

fn safe_patch(app: Application) -> Constraint {
    both(
        changed_docs(app),
        primitive(
            observe_run(call(["cargo", "test", "--workspace"], app.target())),
            exit_code_is(0),
            "tests pass",
        ),
    )
}
```

顶层 Roto 函数可以调用另一个返回 `Constraint` 的顶层函数；组合仍显式体现在返回的 `Constraint` 抽象语法树中。

### 2.5.3 Constraint lattice

`Constraint` 的形式语义以 [`formal/kernel.lean`](../../formal/kernel.lean) 为准。
运行时可以短路、缓存或批量执行 verifier，但不能改变 constraint 树的语义。

### 2.5.4 稳定组合语义

`Stable` 的形式定义以 [`formal/kernel.lean`](../../formal/kernel.lean) 为准。证据复用是运行时策略：必须能通过公开 Compose 关系、父证据和当前约束策略重新推出；否则必须显式失败并要求重新验证。

### 2.5.5 实现收敛待办

这些事项暂不改变 Lean 内核；先作为实现/生命周期层收敛项追踪：

- **证据 subject 分层**：`satisfies(app, c)` 仍只表达端点语义；candidate/patch subject 绑定属于外层认证和证据记录。若后续形式化，应新增 `certified(subject, c, evidence)` 桥接，而不是放宽只看端点的 `satisfies`。
- **`PreviousFailure`**：当前 Roto/validation 接口依赖本地验证历史。它应保持为验证器上下文或诊断能力，或在进入纯 `PlanId` 语义前 elaboration 成固定 state/file ref；不要把历史 selector 直接提升进内核 primitive。
- **分支感知验证**：policy 对 `Either` 已按 constraint 树判断；validation 可优化为按分支按需执行，避免展平后多跑无用 primitive。
- **稳定证据复用**：运行时尚未实现由 `Stable`/`Entails` 驱动的组合证据派生；落地前需要稳定性声明、漂移检查和基于 relation 的重推规则。
- **稳定 ID**：`PlanId` 等公开身份后续应明确规范序列化契约，并评估 digest 长度加固。

## 2.6 证据

证据是运行时为一个具体 subject 和一个 primitive plan 生成的记录：

```text
EvidenceRecord {
  id,
  subject,   // candidate/patch/application scope
  plan,      // PlanId
  verifier,
  result,    // passed / failed / error / not_applicable
  ...
}
```

`graft-validate` 提供 plan/evidence 辅助函数和约束满足性检查；`graft-runtime` 负责感知存储的 plan 执行和命令物化。运行时可以按 `(argv, materialized tree id)` 记忆化相同运行观测，使一次命令执行服务于多个断言；但每条证据记录仍指向它支持的 primitive `PlanId`。

### 2.6.1 证据内容寻址

证据身份由规范化的 subject/plan/verifier/result 载荷内容寻址。墙钟耗时、沙箱路径、原始日志或主机特定路径等本地诊断细节，除非显式声明为相关输出，否则不属于稳定身份。

### 2.6.2 观测可复现性

Graft 不把证据视为手写证明。本地验证器可以在相同执行契约下重跑同一 plan，并比较规范输出，从而重建证据。

这就是观测可复现性：

```text
(subject, plan, verifier, execution_contract) + canonical result
  -> same EvidenceId
```

准入检查要求 candidate/patch 约束以及额外准入/推广策略约束所需的每个 primitive leaf 都有通过证据。缺失或失败的证据会作为结构化约束失败上报，并由运行时或说明系统渲染为面向用户的诊断。

### 2.6.3 证据引用与重建

公开补丁记录以 owner-indexed 记录存储 evidence refs。新 clone 中证据正文可能暂时缺失，直到本地重建或获取。若 refs 提到的 evidence id 没有可用正文，admit/promote 必须显式失败，并要求用户为缺失约束运行验证，而不能静默接受该 ref。

### 2.6.4 Promotion effect 不是证据

Promotion 是显式外部副作用边界。`graft patch promote --yes` 可以写 Git 引用、本地文件或远端目标，并产生 `PromotionRecord`；但这不证明约束，也不修改任何 `EvidenceRecord`。
