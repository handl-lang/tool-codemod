//! Byte-splice edits → `Change`s (PL-004 T-091).
//!
//! Some desired-state producers do not rewrite ASTs — they emit *byte
//! edits*: lint `--fix` diagnostics carry `LintFixOut { start, end,
//! replacement }` ranges computed by natives and module analysis, and
//! `source.fmt` rewrites the whole file. Those edits are legitimate
//! desired-state: the post-splice content is exactly what a `Put`
//! should write. [`CodemodEngine::plan_byte_edits`](crate::CodemodEngine::plan_byte_edits)
//! is the entry point — it reads the artifact, applies the edits, and
//! emits the same [`CodemodPlan`]/[`Change`] types the AST pipeline
//! produces, so `hnl-tool-codegen`'s apply/revert lifecycle journals,
//! drift-checks, and reverts the write with no special casing.
//!
//! This is deliberately *not* a spec rule kind: byte edits are computed
//! per artifact by an upstream producer (a lint pass, a formatter) —
//! they cannot be expressed as a static `match:`/`rewrite:` pair, and
//! routing them through parse→rewrite→unparse would be a lie about the
//! mechanism.

use serde::{Deserialize, Serialize};

/// One byte-range splice against an artifact's UTF-8 source.
///
/// The codemod-engine spelling of a lint fix: `hnl_tool_lint`'s
/// `LintFixOut` carries `start`/`end`/`replacement`; `rule` is the
/// producing rule's id (a lint rule name, `"fmt"`, …) kept for
/// provenance — it lands verbatim on [`CodemodEdit::rule_id`](crate::CodemodEdit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteEdit {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
    /// Replacement text for `source[start..end]`.
    pub replacement: String,
    /// Provenance — the rule/pass that produced the edit.
    pub rule: String,
}

impl ByteEdit {
    /// A splice produced by `rule`.
    pub fn new(
        rule: impl Into<String>,
        start: usize,
        end: usize,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            start,
            end,
            replacement: replacement.into(),
            rule: rule.into(),
        }
    }
}

/// Why a byte-edit batch was refused.
///
/// Mirrors `handl_plugin_ext_diag::FixError`'s overlap/bounds checks,
/// plus a char-boundary guard `replace_range` would otherwise panic on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpliceError {
    /// Two edits overlap: `first` does not end at or before `second`'s
    /// start (edits are compared in ascending start order).
    Overlapping {
        /// First edit's start.
        first_start: usize,
        /// First edit's end.
        first_end: usize,
        /// Second edit's start.
        second_start: usize,
        /// Second edit's end.
        second_end: usize,
    },
    /// An edit's range is inverted or past the end of the source.
    OutOfBounds {
        /// Edit start.
        start: usize,
        /// Edit end.
        end: usize,
        /// Source length in bytes.
        len: usize,
    },
    /// An edit boundary splits a UTF-8 codepoint — `str::replace_range`
    /// would panic; refused instead.
    NotCharBoundary {
        /// The offending offset.
        offset: usize,
    },
}

impl std::fmt::Display for SpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overlapping {
                first_start,
                first_end,
                second_start,
                second_end,
            } => write!(
                f,
                "overlapping edits: {first_start}..{first_end} overlaps \
                 {second_start}..{second_end}"
            ),
            Self::OutOfBounds { start, end, len } => {
                write!(
                    f,
                    "edit {start}..{end} out of bounds for source of length {len}"
                )
            }
            Self::NotCharBoundary { offset } => {
                write!(f, "edit boundary {offset} is not a UTF-8 char boundary")
            }
        }
    }
}

impl std::error::Error for SpliceError {}

/// Apply `edits` to `source`: validate bounds/overlap/char boundaries,
/// then splice in descending start order so earlier offsets stay valid.
///
/// Adjacent edits (`a.end == b.start`) are allowed — they do not
/// overlap. Input order does not matter; the result is deterministic.
///
/// # Errors
///
/// [`SpliceError::Overlapping`], [`SpliceError::OutOfBounds`], or
/// [`SpliceError::NotCharBoundary`] — the batch is refused whole,
/// never partially applied.
pub fn apply_byte_edits(source: &str, edits: &[ByteEdit]) -> Result<String, SpliceError> {
    if edits.is_empty() {
        return Ok(source.to_string());
    }

    // Validate in ascending start order (deterministic regardless of
    // input order).
    let mut ordered: Vec<&ByteEdit> = edits.iter().collect();
    ordered.sort_by_key(|e| (e.start, e.end, e.rule.as_str()));

    let len = source.len();
    for w in ordered.windows(2) {
        let a = &w[0];
        let b = &w[1];
        if a.end > b.start {
            return Err(SpliceError::Overlapping {
                first_start: a.start,
                first_end: a.end,
                second_start: b.start,
                second_end: b.end,
            });
        }
    }
    for e in &ordered {
        if e.start > e.end || e.end > len {
            return Err(SpliceError::OutOfBounds {
                start: e.start,
                end: e.end,
                len,
            });
        }
        for &offset in &[e.start, e.end] {
            if !source.is_char_boundary(offset) {
                return Err(SpliceError::NotCharBoundary { offset });
            }
        }
    }

    // Apply in descending start order so earlier offsets stay valid.
    let mut result = source.to_string();
    for e in ordered.iter().rev() {
        result.replace_range(e.start..e.end, &e.replacement);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_non_overlapping_edits_in_any_order() {
        let src = "alpha beta gamma";
        let edits = vec![
            ByteEdit::new("r2", 11, 16, "G"),
            ByteEdit::new("r1", 0, 5, "A"),
            ByteEdit::new("r3", 6, 10, "B"),
        ];
        assert_eq!(apply_byte_edits(src, &edits).unwrap(), "A B G");
    }

    #[test]
    fn adjacent_edits_are_allowed() {
        let src = "abcdef";
        let edits = vec![ByteEdit::new("r", 0, 3, "X"), ByteEdit::new("r", 3, 6, "Y")];
        assert_eq!(apply_byte_edits(src, &edits).unwrap(), "XY");
    }

    #[test]
    fn empty_replacement_deletes_the_range() {
        let edits = vec![ByteEdit::new("r", 2, 5, "")];
        assert_eq!(apply_byte_edits("abcdef", &edits).unwrap(), "abf");
    }

    #[test]
    fn overlapping_edits_are_refused() {
        let edits = vec![
            ByteEdit::new("r1", 0, 4, "x"),
            ByteEdit::new("r2", 3, 6, "y"),
        ];
        assert!(matches!(
            apply_byte_edits("abcdef", &edits),
            Err(SpliceError::Overlapping { .. })
        ));
    }

    #[test]
    fn out_of_bounds_and_inverted_ranges_are_refused() {
        assert!(matches!(
            apply_byte_edits("abc", &[ByteEdit::new("r", 1, 9, "x")]),
            Err(SpliceError::OutOfBounds { .. })
        ));
        assert!(matches!(
            apply_byte_edits("abc", &[ByteEdit::new("r", 3, 1, "x")]),
            Err(SpliceError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn non_char_boundary_is_refused_not_panicked() {
        // "é" is two bytes; splitting it must error, not panic.
        let edits = vec![ByteEdit::new("r", 0, 1, "x")];
        assert!(matches!(
            apply_byte_edits("éx", &edits),
            Err(SpliceError::NotCharBoundary { .. })
        ));
    }

    #[test]
    fn empty_edit_list_is_identity() {
        assert_eq!(apply_byte_edits("abc", &[]).unwrap(), "abc");
    }
}
