# tool-codemod

The declarative codemod spec model: `codemod.yaml` targets and rules,
parsed and validated — data only, no rewrite engine yet.

## What it is

The **spec layer** of the codemod engine (proposal P-002, plan PL-004
Phase 2): typed structs for the `codemod.yaml` document, YAML entry points
(`CodemodSpec::load` / `parse` / `from_yaml`), and `validate()` —
structured issues, never panics. Matching, planning, and the
plan/apply/revert lifecycle over `ArtifactStore` arrive in later tasks.

## Start here

- **Reference** — [public API](/tool-codemod/reference/)
- **How-to** — [task recipes](/tool-codemod/how-to/)
- **Explanation** — [design and rationale](/tool-codemod/explanation/)

No tutorials: this repo is a building block used from Rust. End-to-end
walkthroughs live with the product that composes it — see the
[Handl platform](/handl/).
