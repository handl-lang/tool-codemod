# Reference

## Public API

| item | what it is |
|---|---|
| `CodemodSpec` | the `codemod.yaml` document: `name`, `version`, `targets`, `rules` |
| `CodemodSpec::load` / `parse` | read + parse + validate (returns `SpecError`) |
| `CodemodSpec::from_yaml` / `validate` | parse-only and validate-only halves |
| `CodemodTarget` | `language` (string), `glob`, `unparse` |
| `UnparseMode` | `preserve` \| `full` \| `none` (default `none` = plan-only) |
| `CodemodRule` | `id`, `scope`, `requires`, `on_engine_error`, `match`, `rewrite` |
| `RuleScope` | `file` (default) \| `program` |
| `EngineErrorPolicy` | `skip` (default) \| `fail` |
| `CodemodRewrite` | `handl:`/`native:` string or `{ delete: true }` |
| `SpecIssue` / `SpecError` | structured validation findings and load errors |

## Crate

| crate | role |
|---|---|
| `hnl-tool-codemod` | spec/model layer — data only |

## Build and test

```bash
cargo build
cargo test
cargo xtask gate   # fmt → clippy -D warnings → test → doc
```

Dependencies are crates.io-only (`serde`, `serde_json`, `serde_yaml`) — no
sibling checkouts needed.
