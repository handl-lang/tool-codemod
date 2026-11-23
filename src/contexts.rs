//! `requires:` engine contexts (P-002 "engine-aware rules", T-083).
//!
//! A [`CodemodRule::requires`](crate::CodemodRule) entry names an engine
//! context the rule needs (`typecheck`, `scope`, …). The engine does
//! **not** know any engine — it asks an injected
//! [`EngineContextProvider`] for the named result, per artifact, lazily
//! (only when a rule that requires it is active), and hash-cached.
//!
//! ## Cache key
//!
//! `(artifact content hash, engine name, engine version)` — bumping an
//! engine's `engine_version()` invalidates its cached contexts; a
//! content change invalidates everything for that artifact.
//!
//! ## Honesty boundary (v1)
//!
//! The produced [`EngineContextReport`] is *gate* data in v1: it
//! decides whether the rule may run on the artifact (provider error →
//! [`EngineErrorPolicy`](crate::EngineErrorPolicy)). Exposing the
//! report *inside* match/rewrite evaluation is a later seam —
//! `TransformCondition` is a pure AST predicate and `RewriteCtx`
//! carries no context slot; when a consumer needs engine-informed
//! matching it lands as a `Native` rewrite reading the report through
//! the resolver layer, not as engine-core surgery.
//!
//! `hnl-engine-codemod` deliberately does **not** depend on
//! `hnl-engine-typecheck` (that crate pulls host deps — engine→host is
//! never legal). Typecheck wiring is a provider impl the plugin layer
//! supplies; absent a provider, `requires: [typecheck]` is an honest
//! "engine unavailable" governed by `on_engine_error`.

use std::collections::HashMap;
use std::sync::Arc;

use handl_language::Language;
use hnl_engine_codegen::{ArtifactRef, ContentHash};
use serde_json::Value;

/// Everything a provider needs to compute one artifact's context.
#[derive(Debug)]
pub struct EngineContextRequest<'a> {
    /// The artifact being codemodded.
    pub artifact: &'a ArtifactRef,
    /// Content hash of the artifact's current bytes (the cache-key
    /// half — providers may skip recompute on their own caches).
    pub content_hash: ContentHash,
    /// The artifact's UTF-8 source.
    pub source: &'a str,
    /// The `Language` the target parses under.
    pub lang: &'a Language,
}

/// A provider failure — reported verbatim in diagnostics; `on_engine_error`
/// decides skip-vs-fail.
#[derive(Debug, Clone)]
pub struct EngineContextError(pub String);

impl std::fmt::Display for EngineContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineContextError {}

/// One computed engine context for one artifact.
#[derive(Debug, Clone)]
pub struct EngineContextReport {
    /// The engine name (must match the `requires:` entry).
    pub engine: String,
    /// The engine version that produced the report (cache-key half).
    pub version: String,
    /// The engine's report — an opaque JSON value the rule layer may
    /// consume once the match/rewrite context seam lands.
    pub report: Value,
}

/// Computes named engine contexts for artifacts.
///
/// Implementations must be `Send + Sync` (the plan pipeline is
/// sequential in v1 but the trait is future-proofed for the parallel
/// map phase). `provide` may be called concurrently for *different*
/// artifacts once parallelism lands — providers should be stateless or
/// internally synchronized.
pub trait EngineContextProvider: Send + Sync {
    /// The `requires:` name this provider serves (e.g. `"typecheck"`).
    fn engine_name(&self) -> &str;
    /// Version stamped into cache keys and reports — bump on any
    /// semantic change to the engine's output.
    fn engine_version(&self) -> &str;
    /// Compute the context for one artifact.
    ///
    /// # Errors
    ///
    /// [`EngineContextError`] — the rule's `on_engine_error` policy
    /// decides whether this skips the rule or fails the run.
    fn provide(
        &self,
        req: &EngineContextRequest<'_>,
    ) -> Result<EngineContextReport, EngineContextError>;
}

/// Cache key: `(artifact hash, engine name, engine version)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct EngineCacheKey {
    pub hash: ContentHash,
    pub engine: String,
    pub version: String,
}

/// Per-run context cache — `HashMap` keyed by [`EngineCacheKey`],
/// shared across all artifacts of one `plan` call. Sequential in v1;
/// a `Mutex` wrapper is the documented upgrade path for the parallel
/// phase.
pub(crate) type EngineContextCache =
    HashMap<EngineCacheKey, Result<Arc<EngineContextReport>, String>>;

/// Look up (or compute + cache) the context for `engine` on one
/// artifact. Returns the cached-or-fresh result; `Err(String)` is the
/// provider's failure message.
pub(crate) fn context_for(
    providers: &HashMap<String, Arc<dyn EngineContextProvider>>,
    cache: &mut EngineContextCache,
    artifact: &ArtifactRef,
    content_hash: ContentHash,
    source: &str,
    lang: &Language,
    engine: &str,
) -> Result<Arc<EngineContextReport>, String> {
    let provider = providers
        .get(engine)
        .ok_or_else(|| format!("no EngineContextProvider registered for '{engine}'"))?;
    let version = provider.engine_version().to_string();
    let key = EngineCacheKey {
        hash: content_hash,
        engine: engine.to_string(),
        version,
    };
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let req = EngineContextRequest {
        artifact,
        content_hash,
        source,
        lang,
    };
    let result = provider.provide(&req).map(Arc::new).map_err(|e| e.0);
    cache.insert(key, result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Counting {
        calls: Mutex<usize>,
    }

    impl EngineContextProvider for Counting {
        fn engine_name(&self) -> &str {
            "typecheck"
        }
        fn engine_version(&self) -> &str {
            "1"
        }
        fn provide(
            &self,
            _req: &EngineContextRequest<'_>,
        ) -> Result<EngineContextReport, EngineContextError> {
            *self.calls.lock().unwrap() += 1;
            Ok(EngineContextReport {
                engine: "typecheck".into(),
                version: "1".into(),
                report: serde_json::json!({"ok": true}),
            })
        }
    }

    #[test]
    fn cache_dedups_by_hash_engine_version() {
        let provider = Arc::new(Counting {
            calls: Mutex::new(0),
        });
        let mut providers: HashMap<String, Arc<dyn EngineContextProvider>> = HashMap::new();
        providers.insert("typecheck".to_string(), provider.clone());
        let mut cache = EngineContextCache::new();
        let art = ArtifactRef::new("a.hnl");
        let hash = ContentHash::of(b"x = 1");
        let lang = Language::new();

        for _ in 0..3 {
            context_for(
                &providers,
                &mut cache,
                &art,
                hash,
                "x = 1",
                &lang,
                "typecheck",
            )
            .unwrap();
        }
        assert_eq!(
            *provider.calls.lock().unwrap(),
            1,
            "provider called once per key"
        );

        // A different artifact hash recomputes.
        let hash2 = ContentHash::of(b"y = 2");
        context_for(
            &providers,
            &mut cache,
            &art,
            hash2,
            "y = 2",
            &lang,
            "typecheck",
        )
        .unwrap();
        assert_eq!(*provider.calls.lock().unwrap(), 2);

        // A missing provider is an honest error, not a panic.
        assert!(context_for(&providers, &mut cache, &art, hash, "x = 1", &lang, "nope").is_err());
    }
}
