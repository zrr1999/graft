# Default workspace template

The minimal layout `graft init` produces:

```text
graft.toml          # workspace config (admission, promotion, sync)
graft.lock          # property/repo resolution lock (commit this)
properties.roto     # property source (empty by default)
.gitignore          # ignores only local Graft state
```

This template is what a fresh workspace looks like before any policy is
declared. It is gated only by Graft's core integrity invariant
(`base + change == target`).

## Adding a property

1. Add a function to `properties.roto`:

   ```roto
   fn empty_change(app: Application) -> Property {
       property(
           [
               app.changed_paths().any_match(["**"]).failure(),
           ],
           "the change touches no paths",
           Severity.Blocking,
           [],
       )
   }
   ```

2. Reference it in `graft.toml`:

   ```toml
   [admission.required_properties]
   workspace = ["empty_change"]
   ```

3. Refresh the lock and re-run admission:

   ```sh
   graft property lock
   graft validate <candidate>
   ```

See `examples/properties/` for idiomatic single-pattern files and
`examples/full/` for a fully wired workspace.
