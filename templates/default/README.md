# Default workspace template

The minimal layout `graft init` produces:

```text
graft.toml          # workspace config (admission, promotion, sync)
graft.lock          # constraint/repo resolution lock (commit this)
constraints.roto     # constraint source (empty by default)
.gitignore          # ignores only local Graft state
```

This template is what a fresh workspace looks like before any policy is
declared. It is gated only by Graft's application core integrity invariant
(`apply(action, base, proof) == target` and `replay(base, change.ops) == target`).

## Adding a constraint

1. Add a function to `constraints.roto`:

   ```roto
   fn empty_change(app: Application) -> Constraint {
       primitive(app.changed_paths(["**"]), no_match, "the change touches no paths")
   }
   ```

2. Reference it in `graft.toml`:

   ```toml
   admission.required = ["empty_change"]
   ```

3. Refresh the lock and re-run admission:

   ```sh
   graft constraint lock
   graft patch validate <candidate>
   ```

See `examples/constraints/` for idiomatic single-pattern files and
`examples/full/` for a fully wired workspace.
