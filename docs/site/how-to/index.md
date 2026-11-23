# How-to guides

Task recipes. Each assumes sibling checkouts of this repo and of the platform
repos it patches.

## Load a codemod spec

```rust
let spec = hnl_tool_codemod::CodemodSpec::load("codemod.yaml")?;
```

`load`/`parse` run validation; use `from_yaml` + `validate` separately when
you want to inspect a broken spec.

## Author a codemod.yaml

Declare `targets` (language name as a string — resolved to `Language` data
by the plugin layer — plus glob and `unparse: preserve|full|none`) and
`rules` (`id`, `scope: file|program`, `requires` engine names,
`on_engine_error: skip|fail`, opaque `match` in the `TransformCondition`
vocabulary, `rewrite` as `handl:<module>.<fn>`, `native:<name>`, or
`{ delete: true }`). See `tests/codemod_spec.rs` for a full example.
