# Graft workspace templates

Starter layouts for new Graft workspaces. Each subdirectory is a complete
workspace skeleton; copy or render it into a new directory as the initial
content.

| Template | Purpose |
| --- | --- |
| [`default/`](./default/) | Minimal layout `graft init` produces — empty `constraints.roto`, no admission/promotion gates. |

These templates use the Roto constraint language conventions
(top-level `fn name(app: Application) -> Constraint`, no
`constraint_registry()`, no PascalCase aliases). The default template is
empty by design; add constraint functions to `constraints.roto` as workspace
policy grows.
