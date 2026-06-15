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

  * `sem` is a *denotational* interpreter and a genuine *monoid homomorphism*: the
    source `(Action, composeAction, idAction)` is a monoid (laws `compose_assoc` /
    `compose_id_left` / `compose_id_right`), mapped by `sem` into the Option-Kleisli
    monoid `(State → Option State, _ >=> _, pure)`; `sem_id` / `sem_seq` are the two
    homomorphism laws (`.bind` is Kleisli composition in the `Option` monad).
    Rust's `Action` is the *free* monoid on primitive ops (`Action::Sequence` =
    concatenation); the kernel abstracts it as an opaque monoid by axiomatizing the
    monoid laws — it does not model the generators, so what we have here is "a
    monoid", not syntactically "the free monoid", which is all the homomorphism
    framing needs. Consequence: re-association / n-ary flattening is *syntactic*
    `Action` equality (`compose_assoc`), so the Rust `Action::Sequence` flattening is
    sound *and* constraint-preserving — equal `Action` + equal base ⇒ equal
    `Application` (up to the subsingleton `valid`) ⇒ identical `satisfies`.
  * Partiality lives in `Option`: `sem a base = none` means "not applicable" (stale
    hashline anchor / delete-missing / conflict). No `Part`/`PFun`, no Mathlib.
  * Trusted base = the *signature* axioms (uninterpreted `State` / `Action` /
    `PlanId` / `sem` / `idAction` / `composeAction` / primitive `holds`) plus the
    `Constraint` expression syntax, and the *law* axioms: the source-monoid laws
    `compose_assoc` / `compose_id_left` / `compose_id_right` and the homomorphism laws
    `sem_id` / `sem_seq`. The old `satisfies_*` facts are now derived theorems, not
    axioms: `satisfies` is a recursive definition over `Constraint`, with primitive
    leaves delegated to `holds` over endpoint `Observation`. The monoid laws are a
    *deliberate* enlargement: they are not derivable (since `sem` is not injective,
    semantic equality cannot be pulled back to syntactic equality), so making `Action`
    a monoid costs three axioms — bought for syntactic re-association / n-ary
    flattening soundness. Everything in §3 (`target`, `Application.observation`,
    `satisfies`, `composable`, `composeApplication`, `targetCompose`, `certify…`) is
    derived; in particular `targetCompose` is a theorem, not an axiom.
  * Because `sem` is opaque, any *data* extracted through it cannot reduce, so
    `Application.target` / `composeApplication` / `certifyComposed` are `noncomputable`.
    Propositions (`applicable`, `composable`, the theorems) need no such marker, and
    `certify` stays computable because it only repackages already-given values.
  * `Constraint` is admission/promotion expression state, not constraint source.
    A `Constraint` primitive (the lattice leaf, Rust `Constraint::Primitive`) is a
    resolved reference to a content-addressed `PlanId` from a `Plan { observation, assertion }`.
  * `Graft` is an admitted, proof-carrying `Application`; `patch:<id>` is its
    persisted lifecycle wrapper.

  This file is machine-checked by the pinned Lean toolchain; run `just check`
  (or directly `lean formal/kernel.lean`). It intentionally avoids Mathlib.

  Section map:

  | Kernel fragment                                                       | design doc                   |
  | --------------------------------------------------------------------- | ---------------------------- |
  | `State`, `Action`, `sem`, `idAction`, `composeAction`, `compose_assoc/id_left/id_right`, `sem_id/seq` | design/model.md §2.3 |
  | `Application`, `applicable`, `target`, `composable`, `composeApplication`, `targetCompose` | design/model.md §2.4 |
  | `PlanId`, `Observation`, `holds`, `Constraint`, `satisfies`, `top`, `bottom`, `primitive`, `both`, `either`, `satisfies_*` | design/property.md §2.5 |
  | `Stable`, `Entails`, `stable_*`, `entails_*`, `certifyComposedStable`, `certifyComposedShared` | design/property.md §2.5 |
  | `Graft`, `certify`, `certifyComposed`                                 | design/admission.md §2.7     |
-/


/- ════════════════════════════════════════════════════════════════════════
   1. 核心模型 (Core model)
   sorts、解释函数符号、基本对象。这里只声明“有什么”，不声明“满足什么”。
   ════════════════════════════════════════════════════════════════════════ -/

axiom State : Type
axiom Action : Type

/-- Content-addressed plan identity. The kernel models only the resolved referent;
    naming/documentation (`ConstraintDef`) and hash construction live above it. -/
axiom PlanId : Type

/-- What a primitive plan may observe. It sees the endpoints of an application, never
    the `Action` syntax that produced the target. Target-only checks can ignore `base`;
    diff-aware checks may use both endpoints. -/
structure Observation where
  base   : State
  target : State

/-- Opaque primitive judgment: a resolved plan holds for an endpoint observation.
    This is the only trusted hook for primitive constraints. -/
axiom holds : Observation → PlanId → Prop

/-- Constraint expression kernel: pure proposition algebra plus primitive Plan refs.
    Names/descriptions are outside the trusted kernel (`ConstraintDef`). -/
inductive Constraint where
  | top       : Constraint
  | bottom    : Constraint
  | primitive : PlanId → Constraint
  | both      : Constraint → Constraint → Constraint
  | either    : Constraint → Constraint → Constraint

/-- 指称语义：把 action 语法解释为 base→target 的偏转换。
    `none` 表示在该 base 上不可应用（= conflict / stale anchor）。 -/
axiom sem : Action → State → Option State

axiom idAction : Action
axiom composeAction : Action → Action → Action

/-- Global aliases keep the design prose close to the mathematical notation. -/
def top : Constraint := Constraint.top
def bottom : Constraint := Constraint.bottom
def primitive (plan : PlanId) : Constraint := Constraint.primitive plan
def both : Constraint → Constraint → Constraint := Constraint.both
def either : Constraint → Constraint → Constraint := Constraint.either

infixr:35 " ⊓ " => both
infixr:30 " ⊔ " => either

/-- 一个 `Application` 把 base、action 与“在该 base 上可应用”的证据捆在一起。
    可应用性内联为 `(sem action base).isSome`，因此 `Application` 只依赖 `sem`，
    且完全由 `(base, action)` 加这一事实决定（`valid` 是 subsingleton）。 -/
structure Application where
  base   : State
  action : Action
  valid  : (sem action base).isSome


/- ════════════════════════════════════════════════════════════════════════
   2. 核心公理 (Core axioms)
   全部可信基就这些；其余皆为定理。
   ════════════════════════════════════════════════════════════════════════ -/

/-- 源 monoid 结合律：`composeAction` 在*语法*层结合。axiom 而非 theorem——`sem` 非单射，
    语义结合（`sem_seq` + Kleisli 律免费给出）拉不回语法相等，故须显式公理化。
    回报：`Action::Sequence` n 元 flatten 在语法层即 `Action` 相等，从而对*任意*约束保持。 -/
axiom compose_assoc (a b c : Action) :
    composeAction (composeAction a b) c = composeAction a (composeAction b c)

/-- 源 monoid 左单位：`idAction` 是 `composeAction` 的左单位。 -/
axiom compose_id_left (a : Action) : composeAction idAction a = a

/-- 源 monoid 右单位：`idAction` 是 `composeAction` 的右单位。 -/
axiom compose_id_right (a : Action) : composeAction a idAction = a

/-- 同态保单位：`idAction` 在任意 state 上都成功且不改变它。 -/
axiom sem_id (s : State) : sem idAction s = some s

/-- 同态保乘积：`composeAction a1 a2` = 先 `a1` 后 `a2`，即 Kleisli 合成。 -/
axiom sem_seq (a1 a2 : Action) (s : State) :
    sem (composeAction a1 a2) s = (sem a1 s).bind (sem a2)

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

/-- Endpoint observation exposed to primitive constraints. This is the P1-B guardrail:
    `satisfies` below factors through `Observation`, so primitive plans cannot inspect
    `app.action` syntax. -/
noncomputable def Application.observation (app : Application) : Observation where
  base   := app.base
  target := app.target

/-- Constraint satisfaction is now a recursive definition over the expression syntax,
    with the only opaque primitive case delegated to `holds` over endpoint observation. -/
noncomputable def satisfies (app : Application) : Constraint → Prop
  | Constraint.top => True
  | Constraint.bottom => False
  | Constraint.primitive plan => holds app.observation plan
  | Constraint.both c1 c2 => satisfies app c1 ∧ satisfies app c2
  | Constraint.either c1 c2 => satisfies app c1 ∨ satisfies app c2

/-- `satisfies` depends on an application only through its endpoint observation. -/
theorem satisfies_observation_eq
    {x y : Application} (h : x.observation = y.observation) (c : Constraint) :
    satisfies x c ↔ satisfies y c := by
  induction c with
  | top => simp [satisfies]
  | bottom => simp [satisfies]
  | primitive plan => simp [satisfies, h]
  | both c1 c2 ih1 ih2 => simp [satisfies, ih1, ih2]
  | either c1 c2 ih1 ih2 => simp [satisfies, ih1, ih2]

/-- Derived lattice semantics: `top` is always satisfied. -/
theorem satisfies_top (app : Application) : satisfies app top := by
  trivial

/-- Derived lattice semantics: `bottom` is never satisfied. -/
theorem satisfies_bottom (app : Application) : ¬ satisfies app bottom := by
  intro h
  exact h

/-- Derived lattice semantics: `both` is conjunction. -/
theorem satisfies_both (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊓ c2) = (satisfies app c1 ∧ satisfies app c2) := by
  rfl

/-- Derived lattice semantics: `either` is disjunction. -/
theorem satisfies_either (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊔ c2) = (satisfies app c1 ∨ satisfies app c2) := by
  rfl

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
（反例：c=“target 含 /tmp/foo”，a 创建、b 删除）。

**边界**：`Stable` 仅刻画顺序 `composeApplication` 下的封闭，**不**蕴含 rebase /
migrate / commutation / merge 下的稳定——后者会换 base 或重排 action，`applicable`
可能失败；resolved 类约束在 rebase 下不应标 Stable（见 §4 TODO）。 -/
def Stable (c : Constraint) : Prop :=
  ∀ (a b : Application) (link : composable a b),
    satisfies a c → satisfies b c →
    satisfies (composeApplication a b link) c

/-- 格的上下界都平凡稳定：top 总被满足；bottom 的前提为假，空真成立。 -/
theorem stable_top : Stable top := by
  intro a b link _ _
  exact satisfies_top (composeApplication a b link)

theorem stable_bottom : Stable bottom := by
  intro a b _ s1 _
  exact absurd s1 (satisfies_bottom a)

/-- `⊓` 对稳定性封闭：被标 `Stable` 的约束可自由用 `both` 组合，复合后整体仍稳定。
    注意 `⊔` **不**封闭（故无 `stable_either`）：复合体可能两支都不单独满足。 -/
theorem stable_both {c1 c2 : Constraint} (h1 : Stable c1) (h2 : Stable c2) :
    Stable (c1 ⊓ c2) := by
  intro a b link s1 s2
  rw [satisfies_both] at s1 s2 ⊢
  exact ⟨h1 a b link s1.1 s2.1, h2 a b link s1.2 s2.2⟩

/-- `d ⊨ c`：满足较强约束 `d` 即满足较弱约束 `c`。
    `Entails` 是 `Constraint` 上的*预序*（自反 + 传递），**不是偏序**：互相 entails 只
    给语义等价 `∀ app, satisfies app c ↔ satisfies app d`，而非值相等 `c = d`——kernel
    无 extensionality 公理，`c ⊓ c` 与 `c` 即互相 entails 却非同一 `Constraint` 值。 -/
def Entails (d c : Constraint) : Prop :=
  ∀ app : Application, satisfies app d → satisfies app c

/-- 预序：自反。 -/
theorem entails_refl (c : Constraint) : Entails c c :=
  fun _ h => h

/-- 预序：传递。 -/
theorem entails_trans {c d e : Constraint}
    (h1 : Entails c d) (h2 : Entails d e) : Entails c e :=
  fun app h => h2 app (h1 app h)

/-- `top` 是最大元（最弱约束）：任何约束都 entails 它。 -/
theorem entails_top (c : Constraint) : Entails c top :=
  fun app _ => satisfies_top app

/-- `bottom` 是最小元（最强约束）：它 entails 任何约束（前提为假，空真成立）。 -/
theorem bottom_entails (c : Constraint) : Entails bottom c :=
  fun app h => absurd h (satisfies_bottom app)

/-- `⊓` 是 meet（下确界）—— 左右投影：携带 `c1 ⊓ c2` 也携带其中任一分量。 -/
theorem entails_both_left {c1 c2 : Constraint} : Entails (c1 ⊓ c2) c1 := by
  intro app h
  rw [satisfies_both] at h
  exact h.1

theorem entails_both_right {c1 c2 : Constraint} : Entails (c1 ⊓ c2) c2 := by
  intro app h
  rw [satisfies_both] at h
  exact h.2

/-- `⊓` 是*最大*下界 —— 引入：同时蕴含 `c1`、`c2` 者蕴含 `c1 ⊓ c2`。 -/
theorem entails_both_intro {e c1 c2 : Constraint}
    (h1 : Entails e c1) (h2 : Entails e c2) : Entails e (c1 ⊓ c2) := by
  intro app h
  rw [satisfies_both]
  exact ⟨h1 app h, h2 app h⟩

/-- `⊔` 是 join（上确界）—— 左右注入：任一分量都 entails `c1 ⊔ c2`。 -/
theorem entails_either_left {c1 c2 : Constraint} : Entails c1 (c1 ⊔ c2) := by
  intro app h
  rw [satisfies_either]
  exact Or.inl h

theorem entails_either_right {c1 c2 : Constraint} : Entails c2 (c1 ⊔ c2) := by
  intro app h
  rw [satisfies_either]
  exact Or.inr h

/-- `⊔` 是*最小*上界 —— 消去：`c1`、`c2` 都蕴含 `e` 者，`c1 ⊔ c2` 蕴含 `e`。 -/
theorem entails_either_elim {c1 c2 e : Constraint}
    (h1 : Entails c1 e) (h2 : Entails c2 e) : Entails (c1 ⊔ c2) e := by
  intro app h
  rw [satisfies_either] at h
  exact h.elim (h1 app) (h2 app)

/--
共享子约束版本（一般情形；下面的 `certifyComposedStable` 是它的特例）：两边不必携带
完全相同的约束；只要它们各自蕴含同一个 stable `c`，复合体即可被认证为满足 `c`。

在 `Entails` 预序里 `c` 是 `g1.constraint`、`g2.constraint` 的*公共上界*（两者都
⊑ c，即 c 比两者都弱）；最紧者为 join `g1.constraint ⊔ g2.constraint`，但任何 stable
公共上界皆可用。

runtime 层是否复用已有 evidence 是 policy；kernel 只给出 soundness skeleton。
-/
noncomputable def certifyComposedShared
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (c : Constraint)
    (hs : Stable c)
    (e1 : Entails g1.constraint c)
    (e2 : Entails g2.constraint c) : Graft :=
  certify
    (composeApplication g1.application g2.application link)
    c
    (hs g1.application g2.application link
      (e1 g1.application g1.valid)
      (e2 g2.application g2.valid))

/--
`Stable` 的基本回报（`certifyComposedShared` 在 `c := g1.constraint` 处的特例）：若
`g1` `g2` 携带*同一* stable 约束且可组合，则复合体的认证*无需新证明义务*——直接由
稳定性导出。这正是“传递性”在 admission 层的形式表达。

它显式实现为 `certifyComposedShared`（`e1` 取自反，`e2` 取 `hc ▸`），因此不引入任何
新的可信内容；保留为“两端同约束”这一常见场景的便利签名。

runtime 层是否复用已有 evidence 是 policy；kernel 只给出 soundness skeleton。
-/
noncomputable def certifyComposedStable
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (hc : g2.constraint = g1.constraint)
    (hs : Stable g1.constraint) : Graft :=
  certifyComposedShared g1 g2 link g1.constraint hs
    (entails_refl g1.constraint)
    (fun _ h => hc ▸ h)


/- ════════════════════════════════════════════════════════════════════════
   4. TODO（已讨论的后续扩展；`Stable` 基础已落地于 §3）
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
  `Stable`: 基础已完工，后续扩展待落地

  基础机制（DONE，见 §3 与 design/property.md §2.5）：约束传递性已落地——
  `Stable` / `stable_top` / `stable_bottom` / `stable_both`；`Entails` 预序-格代数
  `entails_refl` / `entails_trans` / `entails_top` / `bottom_entails` /
  `entails_both_left` / `entails_both_right` / `entails_both_intro` /
  `entails_either_left` / `entails_either_right` / `entails_either_elim`；以及
  `certifyComposedShared` 与其特例 `certifyComposedStable`。
  配套已定决策（admission-time policy，不进可信基）：

  * stable 可由后续 constraint-source policy 在 `constraints.roto` 定义处声明（Roto-site），不是 `graft.toml` policy。
  * stable 与描述/展示元数据不进入 PlanId 哈希；PlanId 只由 canonical(observation, assertion) 决定。
  * 选择 evidence-reuse 语义 (ii)：复合体可通过 Compose 关系、父 evidence、当前 stable policy
    重推；stable 撤回或 constraint drift 时复用重推 fail-loud，使用 `[E_CONSTRAINT_DRIFT]`。

  后续扩展（TODO，均为基础之上的*加法*，不影响已完工的基础机制）：

  1. n 元组合：`composeApplication` 是二元，`graft patch compose` 可 n 元。n 元保持可由
     `stable_both` / `targetCompose` 归纳；action 层结合已由 `compose_assoc` 免费给出，
     仅剩 `composable` 的链式定义（`List.Chain composable`）待补；本轮只保留 TODO，不证明。
  2. 顺序 vs rebase 边界：`Stable` 现仅刻画顺序 `composeApplication`。Migrate(rebase) 把 action
     搬到新 base 时 `applicable` 可能失败，resolved 在 rebase 下不应标 Stable。
  3. 具体 primitive 的稳定性：wellformed / consistent / resolved 尚未引入（见下个 TODO），
     待其落地后再判定哪些可标 Stable——方向不变：wellformed / consistent 可标，resolved 仅顺序组合下可标。
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
