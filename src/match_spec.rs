//! The `match:` field of a [`CodemodRule`](crate::CodemodRule), bound to
//! engine types.
//!
//! The spec keeps `match` as an opaque [`serde_json::Value`] (this
//! crate's spec layer is kernel-free). This module is where the engine
//! decodes it into [`CodemodMatch`]:
//!
//! - `"call"` — shorthand for `{ kind: "call" }`: dispatch on one node
//!   kind, fire unconditionally.
//! - `{ kind: "call", when: <TransformCondition> }` — dispatch on one
//!   node kind, gated by the `handl-language`
//!   [`TransformCondition`](handl_language::TransformCondition)
//!   vocabulary (externally tagged: `KindAt`, `AnyChildKind`,
//!   `PayloadEq`, `ChildPayloadEq`, `ChildCount`, `AttrEq`, `SameAttr`,
//!   `And`, `Or`, `Not`).
//! - any other value — parsed as a bare `TransformCondition` and tried
//!   against **every** node kind the `Language` declares
//!   ([`CodemodMatch::Any`]); the rule set is expanded per kind at plan
//!   time so `RewriteEngine`'s kind dispatch still applies.
//!
//! `TransformCondition` has no node-kind test of its own (it is the
//! `when` vocabulary, ADR-0094 §3), so the kind lives in the `kind:`
//! field — the same split `LintCheck::Match { node_kind, when }` uses
//! (ADR-0097).

use handl_language::TransformCondition;
use serde_json::Value;

/// A decoded codemod `match:` — how a rule finds its nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum CodemodMatch {
    /// Dispatch on a single node kind, optionally gated by `when`.
    Node {
        /// The AST node kind to dispatch on (the `TransformDef`
        /// `node_kind`).
        kind: String,
        /// Optional `TransformCondition` gate.
        when: Option<TransformCondition>,
    },
    /// A bare `TransformCondition` tried against every node kind the
    /// `Language` declares — expanded to one `TransformDef` per kind.
    Any(TransformCondition),
}

/// Decode errors carry the rule id and the offending value for honest
/// diagnostics.
#[derive(Debug)]
pub struct MatchDecodeError {
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for MatchDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl CodemodMatch {
    /// Decode a `match:` spec value (`None` = unconditional — the rule
    /// fires on every node kind).
    ///
    /// # Errors
    ///
    /// Returns [`MatchDecodeError`] when the value is neither a kind
    /// spelling nor a valid `TransformCondition`.
    pub fn decode(value: Option<&Value>) -> Result<Option<Self>, MatchDecodeError> {
        let Some(v) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        // `"kind"` string shorthand.
        if let Some(s) = v.as_str() {
            if s.trim().is_empty() {
                return Err(MatchDecodeError {
                    message: "match kind must not be empty".into(),
                });
            }
            return Ok(Some(Self::Node {
                kind: s.to_string(),
                when: None,
            }));
        }
        // `{ kind: "...", when?: <TransformCondition> }`.
        if let Some(obj) = v.as_object()
            && let Some(kind) = obj.get("kind")
        {
            let kind = kind.as_str().ok_or_else(|| MatchDecodeError {
                message: format!("match.kind must be a string, got {kind}"),
            })?;
            if kind.trim().is_empty() {
                return Err(MatchDecodeError {
                    message: "match.kind must not be empty".into(),
                });
            }
            // Only `kind`/`when` are recognized — anything else is a
            // spec bug, reported rather than silently ignored.
            for key in obj.keys() {
                if key != "kind" && key != "when" {
                    return Err(MatchDecodeError {
                        message: format!("unknown match key {key:?} (expected kind|when)"),
                    });
                }
            }
            let when = match obj.get("when") {
                Some(w) if !w.is_null() => Some(
                    serde_json::from_value::<TransformCondition>(w.clone()).map_err(|e| {
                        MatchDecodeError {
                            message: format!("match.when is not a TransformCondition: {e}"),
                        }
                    })?,
                ),
                _ => None,
            };
            return Ok(Some(Self::Node {
                kind: kind.to_string(),
                when,
            }));
        }
        // Bare TransformCondition → every declared node kind.
        let cond = serde_json::from_value::<TransformCondition>(v.clone()).map_err(|e| {
            MatchDecodeError {
                message: format!("match is neither a kind nor a TransformCondition: {e}"),
            }
        })?;
        Ok(Some(Self::Any(cond)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_kind_string() {
        let m = CodemodMatch::decode(Some(&json!("call"))).unwrap().unwrap();
        assert_eq!(
            m,
            CodemodMatch::Node {
                kind: "call".into(),
                when: None
            }
        );
    }

    #[test]
    fn decodes_kinded_when() {
        let v = json!({"kind": "call", "when": {"ChildCount": 2}});
        let m = CodemodMatch::decode(Some(&v)).unwrap().unwrap();
        match m {
            CodemodMatch::Node { kind, when } => {
                assert_eq!(kind, "call");
                assert_eq!(when, Some(TransformCondition::ChildCount(2)));
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn decodes_bare_condition_as_any() {
        let v =
            json!({"And": [{"ChildCount": 1}, {"Not": {"KindAt": {"index": 0, "kind": "lit"}}}]});
        let m = CodemodMatch::decode(Some(&v)).unwrap().unwrap();
        assert!(matches!(m, CodemodMatch::Any(_)));
    }

    #[test]
    fn none_is_unconditional() {
        assert_eq!(CodemodMatch::decode(None).unwrap(), None);
        assert_eq!(CodemodMatch::decode(Some(&Value::Null)).unwrap(), None);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_conditions() {
        assert!(CodemodMatch::decode(Some(&json!({"kind": "x", "bogus": 1}))).is_err());
        assert!(CodemodMatch::decode(Some(&json!({"kind": 3}))).is_err());
        assert!(CodemodMatch::decode(Some(&json!({"NoSuchVariant": {}}))).is_err());
    }
}
