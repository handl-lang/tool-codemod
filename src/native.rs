//! Native-rewrite dispatch for codemod rules.
//!
//! Codemod `rewrite:` names resolve through the same
//! [`NativeRewrite`](hnl_engine_transform::native::NativeRewrite)
//! seam transform rules use — the engine holds no `handl:`/`native:`
//! knowledge itself. Two layers are chained:
//!
//! 1. **internal** — [`DELETE_NATIVE`], the `{ delete: true }`
//!    rewrite. It emits a tombstone node
//!    ([`TOMBSTONE_KIND`]) that [`crate::compact::strip_tombstones`]
//!    removes from the output arena afterwards (the transform
//!    vocabulary replaces nodes; it cannot drop a child — deletion is
//!    a codemod-level post-pass).
//! 2. **user** — the [`NativeRewrite`] the caller wires
//!    ([`CodemodEngine::with_rewrite_resolver`](crate::CodemodEngine::with_rewrite_resolver)).
//!    The plugin layer supplies the `handl:<mod>.<fn>` `.hnl` resolver
//!    and registered `native:` rewrites — same layering as lint
//!    natives (`NativeLintResolver` / `HandlNativeLint`).

use std::sync::Arc;

use handl_language::{AstNode, AstRef};
use hnl_engine_transform::native::{
    ChainedRewrite, NativeRewrite, NativeRewriteRegistry, RewriteCtx,
};

/// The internal native name `{ delete: true }` compiles to.
pub const DELETE_NATIVE: &str = "codemod.delete";

/// The tombstone node kind delete rewrites produce. Internal
/// vocabulary (like `__fresh_*` names) — never a language kind, never
/// emitted: [`crate::compact::strip_tombstones`] removes every node of
/// this kind before unparsing.
pub const TOMBSTONE_KIND: &str = "__codemod_delete__";

/// Build the internal layer: just the delete tombstone.
fn internal_registry() -> NativeRewriteRegistry {
    let mut reg = NativeRewriteRegistry::new();
    reg.register(
        DELETE_NATIVE,
        Box::new(|ctx: &mut RewriteCtx<'_>| {
            // The tombstone inherits the matched node's span so edit
            // records and diagnostics keep source positions; its payload
            // is irrelevant — the node never survives compaction.
            let mut node = AstNode::new(TOMBSTONE_KIND);
            node.span = ctx.node().span;
            let idx = ctx.out().push(node).index();
            Ok(AstRef::new(idx))
        }),
    );
    reg
}

/// The [`NativeRewrite`] the codemod engine wires into
/// `RewriteEngine`: internal delete tombstone first, then the
/// caller-supplied resolver (when any). `ChainedRewrite` routes by
/// `provides`, so `codemod.delete` always hits the internal layer and
/// every other name reaches the user resolver — which reports its own
/// "unknown native" error when nothing claims it.
pub(crate) fn codemod_natives(user: Option<Arc<dyn NativeRewrite>>) -> Arc<dyn NativeRewrite> {
    let internal: Arc<dyn NativeRewrite> = Arc::new(internal_registry());
    match user {
        Some(user) => Arc::new(ChainedRewrite::new(vec![internal, user])),
        None => internal,
    }
}
