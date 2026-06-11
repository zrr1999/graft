/-
  Graft formal kernel
  ===================

  This file is the single source of truth for the Graft formal model. It is
  referenced from the split design docs under `docs/design/` (model.md §2.3/§2.4,
  property.md §2.5, admission.md §2.7) rather than duplicated there; `docs/design.md`
  is the index.

  Structure (read top to bottom):

    1. 核心模型   Core model      —— sorts、解释函数、基本对象（uninterpreted 签名）
    2. 核心公理   Core axioms     —— 约束这些符号的定律，即全部可信基
    3. 核心定义   Core definitions —— 由模型/公理派生的 def 与 theorem
    4. TODO       —— 已讨论但尚未落地的扩展

  Layering discipline:

  * `sem` is a *denotational* interpreter: it is the monoid homomorphism from the
    free Action monoid `(Action, composeAction, idAction)` into the Option-Kleisli
    monoid `(State → Option State, _ >=> _, pure)`. `sem_id` / `sem_seq` are exactly
    its two homomorphism laws; `.bind` is Kleisli composition in the `Option` monad.
    Consequence: semantic associativity is free, which is what makes the Rust
    `Action::Sequence` n-ary flattening sound.
  * Partiality lives in `Option`: `sem a base = none` means "not applicable" (stale
    hashline anchor / delete-missing / conflict). No `Part`/`PFun`, no Mathlib.
  * Trusted base = the *signature* axioms (uninterpreted `State` / `Action` /
    `Constraint` / `sem` / `idAction` / `composeAction` / `satisfies` / lattice ops)
    plus exactly the *law* axioms `sem_id`, `sem_seq`, and the `satisfies_*` family.
    Everything in §3 (`target`, `composable`, `composeApplication`, `targetCompose`,
    `certify…`) is derived; in particular `targetCompose` is a theorem, not an axiom.
  * Because `sem` is opaque, any *data* extracted through it cannot reduce, so
    `Application.target` / `composeApplication` / `certifyComposed` are `noncomputable`.
    Propositions (`applicable`, `composable`, the theorems) need no such marker, and
    `certify` stays computable because it only repackages already-given values.
  * `Constraint` is admission/promotion expression state, not property source.
    A `Constraint` primitive (the lattice leaf, Rust `Constraint::Primitive`) is a
    resolved reference to a `properties.roto` body.
  * `Graft` is an admitted, proof-carrying `Application`; `patch:<id>` is its
    persisted lifecycle wrapper.

  Note: there is no Lean toolchain wired in this repo, so this file is a
  documentation-level model. The proofs are written to be faithful (core `Option`
  lemmas only) but are not machine-checked.

  Section map:

  | Kernel fragment                                                       | design doc                   |
  | --------------------------------------------------------------------- | ---------------------------- |
  | `State`, `Action`, `sem`, `idAction`, `composeAction`, `sem_id/seq`   | design/model.md §2.3         |
  | `Application`, `applicable`, `target`, `composable`, `composeApplication`, `targetCompose` | design/model.md §2.4 |
  | `Constraint`, `satisfies`, `top`, `bottom`, `both`, `either`, `satisfies_*` | design/property.md §2.5 |
  | `Stable`, `stable_top`, `stable_bottom`, `stable_both`, `certifyComposedStable` | design/property.md §2.5 |
  | `Graft`, `certify`, `certifyComposed`                                 | design/admission.md §2.7     |
-/


/- ════════════════════════════════════════════════════════════════════════
   1. 核心模型 (Core model)
   sorts、解释函数符号、基本对象。这里只声明“有什么”，不声明“满足什么”。
   ════════════════════════════════════════════════════════════════════════ -/

axiom State : Type
axiom Action : Type
axiom Constraint : Type

/-- 指称语义：把 action 语法解释为 base→target 的偏转换。
    `none` 表示在该 base 上不可应用（= conflict / stale anchor）。 -/
axiom sem : Action → State → Option State

axiom idAction : Action
axiom composeAction : Action → Action → Action

/-- 一个 `Application` 把 base、action 与“在该 base 上可应用”的证据捆在一起。
    可应用性内联为 `(sem action base).isSome`，因此 `Application` 只依赖 `sem`，
    且完全由 `(base, action)` 加这一事实决定（`valid` 是 subsingleton）。 -/
structure Application where
  base   : State
  action : Action
  valid  : (sem action base).isSome

/-- 约束在一个具体 application 上是否被满足。opaque：无 functoriality 公理。 -/
axiom satisfies : Application → Constraint → Prop

axiom top    : Constraint
axiom bottom : Constraint
axiom both   : Constraint → Constraint → Constraint
axiom either : Constraint → Constraint → Constraint

infixr:35 " ⊓ " => both
infixr:30 " ⊔ " => either


/- ════════════════════════════════════════════════════════════════════════
   2. 核心公理 (Core axioms)
   全部可信基就这些；其余皆为定理。
   ════════════════════════════════════════════════════════════════════════ -/

/-- 同态保单位：`idAction` 在任意 state 上都成功且不改变它。 -/
axiom sem_id (s : State) : sem idAction s = some s

/-- 同态保乘积：`composeAction a1 a2` = 先 `a1` 后 `a2`，即 Kleisli 合成。 -/
axiom sem_seq (a1 a2 : Action) (s : State) :
    sem (composeAction a1 a2) s = (sem a1 s).bind (sem a2)

axiom satisfies_top (app : Application) :
    satisfies app top = True

axiom satisfies_bottom (app : Application) :
    satisfies app bottom = False

axiom satisfies_both (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊓ c2) = (satisfies app c1 ∧ satisfies app c2)

axiom satisfies_either (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊔ c2) = (satisfies app c1 ∨ satisfies app c2)


/- ════════════════════════════════════════════════════════════════════════
   3. 核心定义 (Core definitions)
   派生的 def 与 theorem，零额外可信基。
   ════════════════════════════════════════════════════════════════════════ -/

/-- 可应用性谓词（与 `Application.valid` 同义），用于谈论 conflict = ¬applicable。 -/
def applicable (base : State) (a : Action) : Prop :=
  (sem a base).isSome

/-- application 的输出端状态（派生量，不存储）。命名对标 `List.length`：
    `app.base` 与 `app.target` 调用点对称，外部看不出一个是字段、一个是函数。 -/
noncomputable def Application.target (app : Application) : State :=
  (sem app.action app.base).get app.valid

/-- 有向组合关系：`composable a b` 表示 a 的终点正是 b 的起点。**不对称**。 -/
def composable (a b : Application) : Prop :=
  a.target = b.base

/-- 顺序组合后仍可应用：由 `sem_seq` 把 `none` 排除在 `composable` 的衔接点之外。 -/
theorem composeApplicable
    (app1 app2 : Application) (link : composable app1 app2) :
    applicable app1.base (composeAction app1.action app2.action) := by
  have h1 : sem app1.action app1.base = some app1.target :=
    (Option.some_get app1.valid).symm
  show (sem (composeAction app1.action app2.action) app1.base).isSome
  rw [sem_seq, h1]
  -- `(some app1.target).bind (sem app2.action)` 定义即 `sem app2.action app1.target`
  show (sem app2.action app1.target).isSome
  rw [link]
  exact app2.valid

/-- 顺序组合：base 取 app1 的 base，action 取 `composeAction`，证据来自上面的引理。 -/
noncomputable def composeApplication
    (app1 app2 : Application) (link : composable app1 app2) : Application where
  base   := app1.base
  action := composeAction app1.action app2.action
  valid  := composeApplicable app1 app2 link

/-- 这是 theorem 不是 axiom：复合体的终点就是 app2 的终点。
    先证两个底层 option 相等（`sem_seq` + bind 定义化化简 + `link`），
    再用 `Option.get` 的证明无关性收尾。 -/
theorem targetCompose
    (app1 app2 : Application) (link : composable app1 app2) :
    (composeApplication app1 app2 link).target = app2.target := by
  have e : sem (composeAction app1.action app2.action) app1.base
             = sem app2.action app2.base := by
    have h1 : sem app1.action app1.base = some app1.target :=
      (Option.some_get app1.valid).symm
    rw [sem_seq, h1]
    -- `(some app1.target).bind (sem app2.action)` 定义即 `sem app2.action app1.target`
    show sem app2.action app1.target = sem app2.action app2.base
    rw [link]
  simp only [Application.target, composeApplication, e]

/-- 被认证的 proof-carrying application。`patch:<id>` 是它的持久化外壳。 -/
structure Graft where
  application : Application
  constraint  : Constraint
  valid       : satisfies application constraint

/-- admit：对一个 application 配上它满足某约束的证据。 -/
def certify
    (app : Application) (c : Constraint) (proof : satisfies app c) : Graft where
  application := app
  constraint  := c
  valid       := proof

/-- 组合路径的认证：调用方需提供复合体满足 `c` 的*新*证据（约束不自动从分量传播，见 TODO）。 -/
noncomputable def certifyComposed
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (c : Constraint)
    (proof : satisfies (composeApplication g1.application g2.application link) c) :
    Graft :=
  certify (composeApplication g1.application g2.application link) c proof

/--
约束传递性 / 组合封闭性：`Stable c` 表示“C 在顺序组合下封闭”——
两端各自满足 C 且可组合，则复合体也满足 C。

这是关于*单个共享约束* C 的命题，不是“两个不同约束取 ⊓”：后者
`satisfies a c1 → satisfies b c2 → satisfies (compose) (c1 ⊓ c2)` 一般为假
（反例：c=“target 含 /tmp/foo”，a 创建、b 删除）。 -/
def Stable (c : Constraint) : Prop :=
  ∀ (a b : Application) (link : composable a b),
    satisfies a c → satisfies b c →
    satisfies (composeApplication a b link) c

/-- 格的上下界都平凡稳定：top 总被满足；bottom 的前提为假，空真成立。 -/
theorem stable_top : Stable top := by
  intro a b _ _ _
  rw [satisfies_top]; trivial

theorem stable_bottom : Stable bottom := by
  intro a b _ s1 _
  rw [satisfies_bottom] at s1; exact s1.elim

/-- `⊓` 对稳定性封闭：被标 `Stable` 的约束可自由用 `both` 组合，复合后整体仍稳定。
    注意 `⊔` **不**封闭（故无 `stable_either`）：复合体可能两支都不单独满足。 -/
theorem stable_both {c1 c2 : Constraint} (h1 : Stable c1) (h2 : Stable c2) :
    Stable (c1 ⊓ c2) := by
  intro a b link s1 s2
  rw [satisfies_both] at s1 s2 ⊢
  exact ⟨h1 a b link s1.1 s2.1, h2 a b link s1.2 s2.2⟩

/--
`Stable` 的回报：若 `g1` `g2` 携带*同一* stable 约束且可组合，则复合体的认证
*无需新证据义务*——直接由稳定性导出。这正是“传递性”在 admission 层的形式表达。

注意：这是 kernel 层 soundness，**不**等于 runtime 可跳过 evidence。runtime evidence 绑定具体
ApplicationId，而复合体是新 application / 新 id，仍按 §4.5 重跑。 -/
noncomputable def certifyComposedStable
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (hc : g2.constraint = g1.constraint)
    (hs : Stable g1.constraint) : Graft :=
  certify
    (composeApplication g1.application g2.application link)
    g1.constraint
    (hs g1.application g2.application link g1.valid (hc ▸ g2.valid))


/- ════════════════════════════════════════════════════════════════════════
   4. TODO（已讨论，尚未落地）
   ════════════════════════════════════════════════════════════════════════ -/

/-
  TODO: inverse semantics

  目前不建模 Action 级或 Application 级的逆。两者都预期是 kernel 之上的*加法*：

  * Action 层：`sem` 是偏函数，不期望群逆，不引入 `invertAction` / `sem_inv`。
  * Application 层：计划形如命题 `Invertible : Application → Prop`，由关系
      `reverts (app app_inv : Application) : Prop :=`
      `  ∃ link : composable app app_inv,`
      `    (composeApplication app app_inv link).target = app.base`
    支撑，对称性与终点唯一性可经 `targetCompose` 导出。

  Rust 的 `Change::reversed`（端点 op 对偶）是一个具体逆 witness，未来由桥接引理接回。
-/

/-
  TODO (residual): `Stable` 的剩余开放项

  机制已落地（见 §3：`Stable` / `stable_top` / `stable_bottom` / `stable_both` /
  `certifyComposedStable`）。仍待讨论：

  1. 具体 primitive 的稳定性：wellformed / consistent / resolved 尚未引入（见下个 TODO）。
     哪些 primitive 稳定是 *policy*，不宜硬编为 kernel axiom；拟由 graft.toml 手动标注
     （见设计讨论）。方向不变：wellformed / consistent 可标 Stable；resolved 仅顺序组合下可标。
  2. 共享子约束提取：`certifyComposedStable` 现要求两边携带*同一*约束。若两个 patch 携带
     `c ⊓ extra1` / `c ⊓ extra2`，想让复合体仅保持共享的 stable `c`，需加减弱引理
     `satisfies app (c1 ⊓ c2) → satisfies app c1`（由 `satisfies_both` 平凡得出）。是否支持？
  3. n 元组合：`composeApplication` 是二元，`graft patch compose` 可 n 元。n 元保持可由
     `stable_both` 归纳，但需 `composable` 的链式/结合引理，暂未证。
  4. 顺序 vs rebase 边界：`Stable` 现仅刻画顺序 `composeApplication`。Migrate(rebase) 把 action
     搬到新 base 时 `applicable` 可能失败，resolved 在 rebase 下不应标 Stable。
  5. runtime 对接：`Stable` 是 kernel soundness，不授权 evidence 复用（§4.5 仍重跑）。若将来
     要用 `Stable` 驱动 evidence-reuse 优化，需单独的 runtime 决策与证据等价性论证。
-/

/-
  TODO (deferred): 状态三分与是否引入否定（wellformed / consistent / resolved）

  待讨论引入下列约束 primitive（均 : Constraint）：
    wellformed  -- 是否是可被系统理解的状态
    consistent  -- 是否没有语义矛盾
    resolved    -- 是否没有未决冲突
  以及状态分类：
    NormalState   := wellformed ⊓ consistent ⊓ resolved        （只正面定义）
    ConflictState := wellformed ∧ consistent ∧ ¬ resolved      （可恢复）
    InvalidState  := ¬ wellformed                              （死路）

  暂定方向（未定案）：
  * 核心模型不引入 ¬。proof-carrying 下“坏状态 = 证据缺席”，由 runtime 报 typed error，
    不进 kernel；保持 Constraint 格“无补”就是“不引入 ¬”的形式版。
  * 只正面定义 NormalState 以保持核心简单；Conflict 若需消解，用带 witness 的正面对象
    + `resolve` 关系，而非 `¬ resolved`（否则丢掉冲突 witness）。
  * 可能的简化：若认精化链 resolved ⊆ consistent ⊆ wellformed，则 NormalState ≡ resolved。

  开放项：NormalState 形态（三 primitive vs 精化链）、Conflict 是“仅拒绝”还是“可消解”——
  待后续讨论后再落地，并与上面的 `Stable` 段对接（哪些 primitive 可标 Stable）。
-/
