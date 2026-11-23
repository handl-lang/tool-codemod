# Explanation

## Why this crate exists

Codemods are declared as data (P-002): a `codemod.yaml` per package names
the artifacts it touches (`targets`) and the rewrite rules that apply
(`rules`). Keeping the document model in its own crate lets the plugin
layer (`handl-plugin-codemod`), the ext shim (`ext-codemod`), and pure-handl
rule packs all share one typed, validated spec — and lets the rewrite
engine evolve without touching spec parsing.

## Why zero kernel deps

The spec model deliberately does not depend on `handl-language` or
`hnl-tool-codegen`: `language` stays a `String` resolved per target by
the plugin layer (ADR-0044 — a new language is a data change), and `match`
stays an opaque `serde_json::Value` carrying the `TransformCondition`
serde shape. The engine binds the real types when it lands; the spec
format does not change.

## Where it sits

`tool-*` family, spec/model layer of the desired-state pipeline
(PL-004). The rewrite engine, `plugin-codemod`, and `ext-codemod` build on
this crate in later tasks.
