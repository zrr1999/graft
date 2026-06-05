# Graft workspace templates

Starter layouts for new Graft workspaces. Each subdirectory is a complete
workspace skeleton; copy or render it into a new directory as the initial
content.

| Template | Purpose |
| --- | --- |
| [`default/`](./default/) | Minimal layout `graft init` produces — empty `properties.roto`, no admission/promotion gates. |

These templates use the final Roto property language conventions
(top-level `fn name(app: Application) -> Property`, no
`property_registry()`, no PascalCase aliases). The default template is
empty by design; add property functions to `properties.roto` as workspace
policy grows.
