/-
  Graft 形式化内核。
  形式语义以本文件为准；设计文档只保留边界说明。
-/

/- 1. 语法对象 -/

axiom State : Type

/--
Action 的原子语法单位。
要求：
* 具有可内容寻址、可序列化的语法身份（由实现层定义）；
* 在内核中只通过 `semAtom` 解释为 `State → Option State`；
* 若某操作可由更基础原子无损降级，应作为别名或宏留在内核外。
-/
axiom ActionAtom : Type

/-- 自由 action：`1 + ActionAtom * X`。 -/
inductive Action where
  | empty : Action
  | step : ActionAtom → Action → Action

/--
约束 primitive；实现层通常由内容寻址 `PlanId` 承载。
primitive 是关于 application 端点的外部谓词。
-/
axiom ConstraintPrimitive : Type
axiom ConstraintPrimitive.holds : ConstraintPrimitive → State → State → Prop

/-- 约束表达式：命题代数 + plan 引用。 -/
inductive Constraint where
  | top       : Constraint
  | bottom    : Constraint
  | primitive : ConstraintPrimitive → Constraint
  | both      : Constraint → Constraint → Constraint
  | either    : Constraint → Constraint → Constraint

infixr:35 " ⊓ " => Constraint.both
infixr:30 " ⊔ " => Constraint.either

/-- n 元 meet / join 只是二元节点的折叠，不是新语义层。 -/
def allOf (constraints : List Constraint) : Constraint :=
  constraints.foldr Constraint.both Constraint.top

def anyOf (constraints : List Constraint) : Constraint :=
  constraints.foldr Constraint.either Constraint.bottom

/- 2. Action 语义 -/

/-- 原子的不可见语义；`none` 表示在该 state 上不可应用。 -/
axiom semAtom : ActionAtom → State → Option State

/-- 顺序解释 action；失败以 `none` 表示。 -/
noncomputable def sem : Action → State → Option State
  | Action.empty, s => some s
  | Action.step atom rest, s => (semAtom atom s).bind (sem rest)

def idAction : Action := Action.empty

/-- action append。 -/
def composeAction : Action → Action → Action
  | Action.empty, b => b
  | Action.step atom rest, b => Action.step atom (composeAction rest b)

def composeActions (actions : List Action) : Action :=
  actions.foldr composeAction idAction

theorem compose_assoc (a b c : Action) :
    composeAction (composeAction a b) c = composeAction a (composeAction b c) := by
  induction a with
  | empty => rfl
  | step atom rest ih => simp [composeAction, ih]

theorem compose_id_left (a : Action) : composeAction idAction a = a := by
  rfl

theorem compose_id_right (a : Action) : composeAction a idAction = a := by
  induction a with
  | empty => rfl
  | step atom rest ih => simp [composeAction, ih]

theorem sem_id (s : State) : sem idAction s = some s := by
  rfl

theorem sem_seq (a1 a2 : Action) (s : State) :
    sem (composeAction a1 a2) s = (sem a1 s).bind (sem a2) := by
  induction a1 generalizing s with
  | empty => rfl
  | step atom rest ih =>
      simp [composeAction, sem]
      cases semAtom atom s with
      | none => rfl
      | some s' => exact ih s'

theorem sem_compose_actions_nil (s : State) : sem (composeActions []) s = some s := by
  rfl

theorem sem_compose_actions_cons (a : Action) (rest : List Action) (s : State) :
    sem (composeActions (a :: rest)) s = (sem a s).bind (sem (composeActions rest)) := by
  rw [composeActions]
  simp [List.foldr]
  rw [sem_seq]
  rfl

/- 3. Application -/

/-- 绑定 base、action 和可应用性证据的一次具体应用。 -/
structure Application where
  base   : State
  action : Action
  valid  : (sem action base).isSome

def applicable (base : State) (a : Action) : Prop :=
  (sem a base).isSome

noncomputable def Application.target (app : Application) : State :=
  (sem app.action app.base).get app.valid

def composable (a b : Application) : Prop :=
  a.target = b.base

theorem compose_applicable
    (app1 app2 : Application) (link : composable app1 app2) :
    applicable app1.base (composeAction app1.action app2.action) := by
  have h1 : sem app1.action app1.base = some app1.target :=
    (Option.some_get app1.valid).symm
  show (sem (composeAction app1.action app2.action) app1.base).isSome
  rw [sem_seq, h1]
  show (sem app2.action app1.target).isSome
  rw [link]
  exact app2.valid

noncomputable def composeApplication
    (app1 app2 : Application) (link : composable app1 app2) : Application where
  base   := app1.base
  action := composeAction app1.action app2.action
  valid  := compose_applicable app1 app2 link

theorem target_compose
    (app1 app2 : Application) (link : composable app1 app2) :
    (composeApplication app1 app2 link).target = app2.target := by
  have e : sem (composeAction app1.action app2.action) app1.base
             = sem app2.action app2.base := by
    have h1 : sem app1.action app1.base = some app1.target :=
      (Option.some_get app1.valid).symm
    rw [sem_seq, h1]
    show sem app2.action app1.target = sem app2.action app2.base
    rw [link]
  simp only [Application.target, composeApplication, e]

/- 4. Constraint 语义 -/

noncomputable def satisfies (app : Application) : Constraint → Prop
  | Constraint.top => True
  | Constraint.bottom => False
  | Constraint.primitive p => p.holds app.base app.target
  | Constraint.both c1 c2 => satisfies app c1 ∧ satisfies app c2
  | Constraint.either c1 c2 => satisfies app c1 ∨ satisfies app c2

theorem satisfies_endpoints_eq
    {x y : Application} (hbase : x.base = y.base) (htarget : x.target = y.target)
    (c : Constraint) :
    satisfies x c ↔ satisfies y c := by
  induction c with
  | top => simp [satisfies]
  | bottom => simp [satisfies]
  | primitive p => simp [satisfies, hbase, htarget]
  | both c1 c2 ih1 ih2 => simp [satisfies, ih1, ih2]
  | either c1 c2 ih1 ih2 => simp [satisfies, ih1, ih2]

theorem satisfies_top (app : Application) : satisfies app Constraint.top := by
  trivial

theorem satisfies_bottom (app : Application) : ¬ satisfies app Constraint.bottom := by
  intro h
  exact h

theorem satisfies_both (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊓ c2) = (satisfies app c1 ∧ satisfies app c2) := by
  rfl

theorem satisfies_either (app : Application) (c1 c2 : Constraint) :
    satisfies app (c1 ⊔ c2) = (satisfies app c1 ∨ satisfies app c2) := by
  rfl

theorem satisfies_all_of (app : Application) :
    (constraints : List Constraint) →
    satisfies app (allOf constraints) ↔ ∀ c ∈ constraints, satisfies app c
  | [] => by
      constructor
      · intro _ c h
        cases h
      · intro _
        exact satisfies_top app
  | c :: rest => by
      change satisfies app (c ⊓ allOf rest) ↔ ∀ d ∈ c :: rest, satisfies app d
      constructor
      · intro h d hd
        rw [satisfies_both] at h
        simp at hd
        cases hd with
        | inl heq => simpa [heq] using h.1
        | inr hrest => exact (satisfies_all_of app rest).mp h.2 d hrest
      · intro h
        rw [satisfies_both]
        exact ⟨h c (by simp),
          (satisfies_all_of app rest).mpr (by
            intro d hd
            exact h d (by simp [hd]))⟩

theorem satisfies_any_of (app : Application) :
    (constraints : List Constraint) →
    satisfies app (anyOf constraints) ↔ ∃ c, c ∈ constraints ∧ satisfies app c
  | [] => by
      constructor
      · intro h
        exact False.elim ((satisfies_bottom app) h)
      · intro h
        rcases h with ⟨c, hc, _⟩
        cases hc
  | c :: rest => by
      change satisfies app (c ⊔ anyOf rest) ↔ ∃ d, d ∈ c :: rest ∧ satisfies app d
      constructor
      · intro h
        rw [satisfies_either] at h
        cases h with
        | inl hc => exact ⟨c, by simp, hc⟩
        | inr hrest =>
            rcases (satisfies_any_of app rest).mp hrest with ⟨d, hd, hs⟩
            exact ⟨d, by simp [hd], hs⟩
      · intro h
        rcases h with ⟨d, hd, hs⟩
        rw [satisfies_either]
        simp at hd
        cases hd with
        | inl heq => left; simpa [heq] using hs
        | inr hrest => right; exact (satisfies_any_of app rest).mpr ⟨d, hrest, hs⟩

/-- 语义蕴含：满足较强约束 `d` 即满足较弱约束 `c`。 -/
def Entails (d c : Constraint) : Prop :=
  ∀ app : Application, satisfies app d → satisfies app c

theorem entails_refl (c : Constraint) : Entails c c :=
  fun _ h => h

theorem entails_trans {c d e : Constraint}
    (h1 : Entails c d) (h2 : Entails d e) : Entails c e :=
  fun app h => h2 app (h1 app h)

theorem entails_top (c : Constraint) : Entails c Constraint.top :=
  fun app _ => satisfies_top app

theorem bottom_entails (c : Constraint) : Entails Constraint.bottom c :=
  fun app h => absurd h (satisfies_bottom app)

theorem entails_both_left {c1 c2 : Constraint} : Entails (c1 ⊓ c2) c1 := by
  intro app h
  rw [satisfies_both] at h
  exact h.1

theorem entails_both_right {c1 c2 : Constraint} : Entails (c1 ⊓ c2) c2 := by
  intro app h
  rw [satisfies_both] at h
  exact h.2

theorem entails_both_intro {e c1 c2 : Constraint}
    (h1 : Entails e c1) (h2 : Entails e c2) : Entails e (c1 ⊓ c2) := by
  intro app h
  rw [satisfies_both]
  exact ⟨h1 app h, h2 app h⟩

theorem entails_either_left {c1 c2 : Constraint} : Entails c1 (c1 ⊔ c2) := by
  intro app h
  rw [satisfies_either]
  exact Or.inl h

theorem entails_either_right {c1 c2 : Constraint} : Entails c2 (c1 ⊔ c2) := by
  intro app h
  rw [satisfies_either]
  exact Or.inr h

theorem entails_either_elim {c1 c2 e : Constraint}
    (h1 : Entails c1 e) (h2 : Entails c2 e) : Entails (c1 ⊔ c2) e := by
  intro app h
  rw [satisfies_either] at h
  exact h.elim (h1 app) (h2 app)

/- 5. 组合稳定性 -/

/-- 顺序组合下封闭的约束。 -/
def Stable (c : Constraint) : Prop :=
  ∀ (a b : Application) (link : composable a b),
    satisfies a c → satisfies b c →
    satisfies (composeApplication a b link) c

theorem stable_top : Stable Constraint.top := by
  intro a b link _ _
  exact satisfies_top (composeApplication a b link)

theorem stable_bottom : Stable Constraint.bottom := by
  intro a b _ s1 _
  exact absurd s1 (satisfies_bottom a)

theorem stable_both {c1 c2 : Constraint} (h1 : Stable c1) (h2 : Stable c2) :
    Stable (c1 ⊓ c2) := by
  intro a b link s1 s2
  rw [satisfies_both] at s1 s2 ⊢
  exact ⟨h1 a b link s1.1 s2.1, h2 a b link s1.2 s2.2⟩

theorem stable_all_of :
    (constraints : List Constraint) →
    (∀ c ∈ constraints, Stable c) → Stable (allOf constraints)
  | [], _ => by
      exact stable_top
  | c :: rest, hs => by
      apply stable_both
      · exact hs c (by simp)
      · apply stable_all_of rest
        intro d hd
        exact hs d (by simp [hd])

/- 6. 认证对象 -/

structure Graft where
  application : Application
  constraint  : Constraint
  valid       : satisfies application constraint

def certify
    (app : Application) (c : Constraint) (proof : satisfies app c) : Graft where
  application := app
  constraint  := c
  valid       := proof

noncomputable def certifyComposed
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (c : Constraint)
    (proof : satisfies (composeApplication g1.application g2.application link) c) :
    Graft :=
  certify (composeApplication g1.application g2.application link) c proof

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

noncomputable def certifyComposedStable
    (g1 g2 : Graft)
    (link : composable g1.application g2.application)
    (hc : g2.constraint = g1.constraint)
    (hs : Stable g1.constraint) : Graft :=
  certifyComposedShared g1 g2 link g1.constraint hs
    (entails_refl g1.constraint)
    (fun _ h => hc ▸ h)

/-
  7. 待办

  * 逆语义：优先在 Application 层建 `reverts` / `Invertible`，不把 `Action` 提升成群。
  * rebase 边界：`Stable` 只覆盖顺序 compose；migrate / rebase 需要单独关系。
  * 状态三分：若引入 wellformed / consistent / resolved，保持正面 primitive；暂不引入补和否定。
-/
