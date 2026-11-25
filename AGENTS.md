# Agent guide — tool-codemod

`hnl-tool-codemod` — the declarative codemod engine's spec/model
layer (proposal P-002, plan PL-004 Phase 2). Defines the `codemod.yaml`
document: `targets[]` (`language`/`glob`/`unparse`), `rules[]` (`id`,
`scope`, `requires`, `on_engine_error`, `match`, `rewrite`), plus
parsing, defaults, and planning. `publish = false` (git-install only).
Rust 2024.

## Layout

| path | role |
| --- | --- |
| `src/lib.rs` | crate docs + public API |
| `src/spec.rs`, `src/rules.rs` | `codemod.yaml` document model |
| `src/match_spec.rs` | rule `match` patterns |
| `src/engine.rs`, `src/plan.rs` | rule evaluation + rewrite planning |
| `src/splice.rs`, `src/split.rs`, `src/compact.rs`, `src/contexts.rs`, `src/native.rs` | source splicing, target splitting, native-rule glue |
| `tests/` | `codemod_engine`, `codemod_spec`, `codemod_splice` |
| `docs/` | spec reference |
| `xtask/`, `workflows/gate.yaml`, `bupha.toml`, `.cargo/` | vendored repo gate + CI wiring |

## Contract

- Deps resolve from git — `handl-language` → `handl-lang/kernel`,
  `hnl-tool-codegen`/`hnl-tool-fmt`/`hnl-tool-lex-pattern`/
  `hnl-tool-parse` → sibling `tool-*` repos, all `branch = "main"`.
- This crate is the *model* layer; execution rides on the `tool-*`
  engines it depends on — keep the `codemod.yaml` schema in `spec.rs`
  the single source of truth.

## Verification

```bash
cargo xtask gate   # fmt --check → clippy -D warnings → test → doc
```


<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->
