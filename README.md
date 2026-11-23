# engine-codemod

`hnl-engine-codemod` — the declarative codemod engine's **spec/model layer**
(proposal P-002, plan PL-004 Phase 2).

This crate defines the `codemod.yaml` document: `targets[]`
(`language`/`glob`/`unparse`), `rules[]` (`id`, `scope`, `requires`,
`on_engine_error`, `match`, `rewrite`), plus parsing, defaults, and a
structured validation pass. It is **data only** — matching, planning, and
apply/revert over `ArtifactStore` land in later tasks.

Zero kernel deps by design: `language` stays a `String` (resolved to
`Language` data by the plugin layer, ADR-0044) and `match` stays an opaque
`serde_json::Value` carrying the `TransformCondition` vocabulary shape until
the engine binds the real `handl-language` type.

## Development

```bash
cargo xtask gate    # fmt → clippy → test → doc  (alias: cargo run --manifest-path xtask/Cargo.toml -- gate)
cargo test
```

Deps are crates.io-only (`serde`, `serde_json`, `serde_yaml`) — a plain
clone builds as-is.

## License

MIT
