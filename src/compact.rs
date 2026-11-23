//! Tombstone compaction — the delete half of `{ delete: true }`.
//!
//! [`crate::native::TOMBSTONE_KIND`] nodes are produced by the internal
//! `codemod.delete` native during a rewrite pass. The transform
//! vocabulary *replaces* nodes; it has no primitive for dropping a
//! child from its parent, so deletion is a codemod-level post-pass:
//! [`strip_tombstones`] rebuilds the output arena without tombstones,
//! removing them from every `children` array (and remapping the rest).
//!
//! The result reports everything the unparse step needs:
//!
//! - `ast` / `root` — the compacted arena and its root (`root: None`
//!   means the whole statement subtree was deleted).
//! - `old_to_new` — pre-compaction output index → post-compaction
//!   index, so [`RewriteTrace`](hnl_engine_transform::output::RewriteTrace)
//!   `to` refs and the preserve-emitter `changed` set can be translated.
//! - `shrank_parents` — pre-compaction indices of kept nodes that lost
//!   tombstone children; they must be marked *changed* for
//!   `PreserveEmitter` (their text differs from any source span).

use handl_language::ast::ARENA_REF_PAYLOAD_KEYS;
use handl_language::{Ast, AstNode, AstRef, LangError};

use crate::native::TOMBSTONE_KIND;

/// The result of [`strip_tombstones`].
#[derive(Debug)]
pub(crate) struct Compacted {
    /// Arena with all tombstones removed.
    pub ast: Ast,
    /// Root in the compacted arena — `None` when the root itself was
    /// deleted (the whole statement goes away).
    pub root: Option<AstRef>,
    /// Pre-compaction index → post-compaction index (`None` for
    /// tombstones and out-of-range indices).
    pub old_to_new: Vec<Option<usize>>,
    /// Pre-compaction indices of kept nodes whose `children` arrays
    /// lost tombstone entries.
    pub shrank_parents: Vec<usize>,
}

/// Remove every [`TOMBSTONE_KIND`] node from `ast`, remapping
/// `children` arrays and named arena references
/// ([`ARENA_REF_PAYLOAD_KEYS`]).
///
/// # Errors
///
/// [`LangError::engine`] when a tombstone is referenced by a named
/// arena-ref payload key (e.g. a `function`'s `body`) — deleting a
/// node that structurally *is* another node's body is not a rewrite
/// the codemod vocabulary can express, so it fails honestly rather
/// than leaving a dangling index.
pub(crate) fn strip_tombstones(ast: &Ast, root: AstRef) -> Result<Compacted, LangError> {
    let n = ast.len();
    let is_tomb = |i: usize| {
        ast.get(AstRef::new(i))
            .map(|node| node.kind == TOMBSTONE_KIND)
            .unwrap_or(false)
    };

    // old index -> new index (tombstones and invalid entries -> None).
    let mut old_to_new: Vec<Option<usize>> = vec![None; n];
    let mut next = 0usize;
    for (i, slot) in old_to_new.iter_mut().enumerate().take(n) {
        if !is_tomb(i) {
            *slot = Some(next);
            next += 1;
        }
    }

    // Root deleted → the whole statement is gone; callers emit "".
    if is_tomb(root.index()) {
        return Ok(Compacted {
            ast: Ast::new(),
            root: None,
            old_to_new,
            shrank_parents: Vec::new(),
        });
    }

    let mut out = Ast::new();
    let mut shrank_parents = Vec::new();
    for i in 0..n {
        if is_tomb(i) {
            continue;
        }
        let node = ast.get(AstRef::new(i)).expect("index checked");
        let mut payload = node.payload.clone();
        let mut shrank = false;

        // `children`: drop tombstone entries, remap the rest.
        if let Some(children) = payload.get_mut("children").and_then(|c| c.as_array_mut()) {
            let mut kept = Vec::with_capacity(children.len());
            for v in children.iter() {
                match v.as_u64().map(|x| x as usize) {
                    Some(old) if is_tomb(old) => shrank = true,
                    Some(old) => match old_to_new.get(old).copied().flatten() {
                        Some(new) => kept.push(serde_json::Value::from(new)),
                        // Unresolvable child index — preserve verbatim
                        // (same policy as the transform remap).
                        None => kept.push(v.clone()),
                    },
                    None => kept.push(v.clone()),
                }
            }
            *children = kept;
        }

        // Named arena refs (`body`, …): remap like the transform
        // engine does; a tombstone target is an honest error.
        for key in ARENA_REF_PAYLOAD_KEYS {
            let Some(old) = payload
                .get(*key)
                .and_then(|v| v.as_u64())
                .map(|x| x as usize)
            else {
                continue;
            };
            if is_tomb(old) {
                return Err(LangError::engine(format!(
                    "codemod delete removed node {old}, which is referenced by payload key '{key}' — \
                     deleting an arena-referenced node is not expressible"
                )));
            }
            if let Some(Some(new)) = old_to_new.get(old)
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert((*key).to_string(), serde_json::Value::from(*new));
            }
        }

        if shrank {
            shrank_parents.push(i);
        }
        out.push(AstNode {
            kind: node.kind.clone(),
            span: node.span,
            payload,
        });
    }

    let new_root = old_to_new
        .get(root.index())
        .copied()
        .flatten()
        .map(AstRef::new)
        .ok_or_else(|| LangError::engine("codemod compaction lost the root node".to_string()))?;

    Ok(Compacted {
        ast: out,
        root: Some(new_root),
        old_to_new,
        shrank_parents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn n(kind: &str, children: &[usize]) -> AstNode {
        AstNode::new(kind).with_payload(json!({ "children": children }))
    }

    #[test]
    fn removes_tombstone_and_remaps() {
        // arena: 0=a, 1=b, 2=tomb, 3=root(children 0,1,2)
        let mut ast = Ast::new();
        ast.push(n("a", &[]));
        ast.push(n("b", &[]));
        let mut t = AstNode::new(TOMBSTONE_KIND);
        t.span = Some(handl_language::Span { start: 5, end: 9 });
        ast.push(t);
        ast.push(n("root", &[0, 1, 2]));

        let c = strip_tombstones(&ast, AstRef::new(3)).unwrap();
        assert_eq!(c.ast.len(), 3);
        assert_eq!(c.root, Some(AstRef::new(2)));
        let root = c.ast.get(AstRef::new(2)).unwrap();
        assert_eq!(
            root.payload.get("children").unwrap(),
            &json!([0, 1]),
            "tombstone child dropped, siblings remapped"
        );
        assert_eq!(c.old_to_new, vec![Some(0), Some(1), None, Some(2)]);
        assert_eq!(c.shrank_parents, vec![3]);
    }

    #[test]
    fn deleted_root_yields_none() {
        let mut ast = Ast::new();
        ast.push(AstNode::new(TOMBSTONE_KIND));
        let c = strip_tombstones(&ast, AstRef::new(0)).unwrap();
        assert_eq!(c.root, None);
        assert!(c.ast.is_empty());
    }

    #[test]
    fn arena_ref_to_tombstone_is_an_error() {
        let mut ast = Ast::new();
        ast.push(AstNode::new(TOMBSTONE_KIND));
        let mut parent = AstNode::new("function");
        parent.payload = json!({ "body": 0 });
        ast.push(parent);
        let err = strip_tombstones(&ast, AstRef::new(1)).unwrap_err();
        assert!(err.to_string().contains("arena-referenced"));
    }
}
