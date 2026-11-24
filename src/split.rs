//! Top-level statement splitting for the raw-parse pipeline.
//!
//! The v0.1 Pratt parser parses a whole token stream into one arena (a
//! synthetic `block` wraps multiple top-level expressions), but the
//! codemod engine works **per statement** — the `ext-transform`
//! precedent: split the source at bracket-depth zero, raw-parse each
//! chunk, offset spans back into the file, rewrite, then splice
//! unchanged gaps verbatim through `PreserveEmitter`.
//!
//! [`StatementSplitter`] is the seam: the engine is generic over
//! `Language`, and statement boundaries are a *surface* convention, so
//! the splitter is injectable. [`HandlStatementSplitter`] is the
//! default — a port of the shared `plugin-ext-diag::split_statements`
//! implementation (the same convention the bootstrap typechecker,
//! linter, and test runner use) for the canonical language's surface
//! (`--` comments, `"…"` strings, newline/`;` separators), plus the
//! `.hnl` module *declaration lines* (`cap`/`op`/`handl` contract
//! declarations and their indented `guarantee:`/`requires:`/
//! `termination:` continuations) that the module loader strips before
//! parsing — see [`mask_declaration_lines`]. A language
//! whose surface differs supplies its own splitter via
//! [`CodemodEngine::with_statement_splitter`](crate::CodemodEngine::with_statement_splitter).

use std::borrow::Cow;

/// A top-level statement chunk: its trimmed text and byte offset in the
/// original source.
#[derive(Clone, Debug)]
pub struct StatementChunk {
    /// The trimmed statement text.
    pub text: String,
    /// Byte offset of the trimmed text's first byte in the original
    /// source. The chunk's end offset is `offset + text.len()`.
    pub offset: usize,
}

impl StatementChunk {
    /// Exclusive end byte offset in the original source.
    pub fn end(&self) -> usize {
        self.offset + self.text.len()
    }
}

/// Splits source text into top-level statement chunks.
///
/// Implementations are shared across the (currently sequential) plan
/// pipeline; `Send + Sync` keeps the parallel-map phase unblocked.
pub trait StatementSplitter: Send + Sync {
    /// Split `src` into top-level statement chunks, in source order.
    fn split(&self, src: &str) -> Vec<StatementChunk>;
}

/// The canonical-language statement splitter — the
/// `plugin-ext-diag::split_statements` algorithm, vendored here so this
/// engine keeps **zero host deps** (engine→host-plugin deps are never
/// legal; the function is small surface-lexing logic, not policy).
///
/// Splits at bracket-depth zero, respecting `"…"` string literals and
/// `--` comments. A `--` comment at a statement boundary is dropped;
/// one inside a statement stays in the chunk text (the lexer strips
/// it). Newlines are whitespace to the grammar, so a depth-0 `\n` is
/// only a *candidate* boundary — not a split when the pending chunk
/// ends mid-expression (last significant char is an operator) or the
/// following line cannot begin a statement. `;` is always a hard
/// separator.
#[derive(Debug, Clone, Copy, Default)]
pub struct HandlStatementSplitter;

impl StatementSplitter for HandlStatementSplitter {
    fn split(&self, src: &str) -> Vec<StatementChunk> {
        // `.hnl` module sources carry contract-declaration lines
        // (`cap "…"`, `op "…" : Sig`, `handl …`, plus indented
        // `guarantee:`/`requires:`/`termination:` continuations) that
        // are *not* grammar statements — the module loader strips them
        // before parsing (`handl-runtime`
        // `module_prep::strip_declaration_lines`; the contract dialect
        // is parsed separately by `plugin-ext-refinement`). Mask them
        // to whitespace — byte-length preserving, so every chunk offset
        // still indexes the original source for preserve-mode splices —
        // run the boundary scan on the mask, then re-slice each chunk's
        // text from `src`. A masked declaration line is indistinguishable
        // from a blank line to the splitter, so it can never become a
        // chunk: it lands in the inter-statement gaps `assemble` copies
        // verbatim.
        let masked = mask_declaration_lines(src);
        let mut chunks = split_statements(&masked);
        if matches!(masked, Cow::Borrowed(_)) {
            return chunks; // no declaration lines — text already verbatim
        }
        for chunk in &mut chunks {
            // The mask only steered boundary detection; chunk text must
            // be the real statement bytes. `chunk.text.len()` is the
            // masked trimmed span, which is byte-identical in length to
            // the original span (masking is 1:1 per byte), so the same
            // `[offset, offset + len)` window slices `src`.
            let end = chunk.offset + chunk.text.len();
            chunk.text = src[chunk.offset..end].to_string();
        }
        chunks
    }
}

/// Whether `line` is a `.hnl` module declaration line the loader strips
/// before parsing — a byte-faithful port of the `keep` predicate in
/// `handl-runtime`'s `module_prep::strip_declaration_lines_mapped`
/// (vendored like `split_statements` itself: this engine keeps zero
/// host deps, and these are *surface* keywords of the canonical module
/// syntax — the same category as the `--`/`;`/`from`/`import`
/// conventions already hardcoded here, not AST kind strings).
///
/// Quirks mirrored deliberately: `cap`/`op`/`handl` match after leading
/// whitespace with a keyword boundary (`op"…"` counts, `opcode` does
/// not); `guarantee`/`requires`/`termination` match as *prefixes* on
/// indented lines only.
fn is_declaration_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if line.starts_with(' ') || line.starts_with('\t') {
        for kw in ["guarantee", "requires", "termination"] {
            if trimmed.starts_with(kw) {
                return true;
            }
        }
    }
    for kw in ["cap", "op", "handl"] {
        if let Some(rest) = trimmed.strip_prefix(kw)
            && (rest.is_empty()
                || rest.starts_with(' ')
                || rest.starts_with('\t')
                || rest.starts_with('"'))
        {
            return true;
        }
    }
    false
}

/// Replace every declaration line's content with spaces, preserving
/// `\n` and total byte length — the offset-preserving equivalent of the
/// loader's `strip_declaration_lines`. Returns [`Cow::Borrowed`] when
/// the source has no declaration lines (the common case).
///
/// Each *char* becomes `len_utf8()` spaces so the mask stays valid
/// UTF-8 at identical byte length; only `\n` survives inside a masked
/// line.
fn mask_declaration_lines(src: &str) -> Cow<'_, str> {
    if !src.lines().any(is_declaration_line) {
        return Cow::Borrowed(src);
    }
    let mut masked = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // Test the line the way `str::lines` yields it (no `\n`, no
        // trailing `\r`) so the fast-path scan and this loop agree.
        let test = body.strip_suffix('\r').unwrap_or(body);
        if is_declaration_line(test) {
            for ch in body.chars() {
                for _ in 0..ch.len_utf8() {
                    masked.push(' ');
                }
            }
            masked.push_str(newline);
        } else {
            masked.push_str(line);
        }
    }
    Cow::Owned(masked)
}

/// Split `src` into top-level statement chunks (canonical surface).
pub fn split_statements(src: &str) -> Vec<StatementChunk> {
    let bytes = src.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    // Last significant byte of the pending chunk — every code byte
    // outside strings and comments.
    let mut last_sig: Option<u8> = None;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // String literal — skip to the closing quote (respecting
        // escapes).
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => {
                        i += 2;
                        continue;
                    }
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            last_sig = Some(b'"');
            continue;
        }

        // Comment: `--` (or `---`) to end of line. Only advance `start`
        // past the comment when it sits at a statement boundary (the
        // pending chunk is whitespace-only) — advancing it mid-statement
        // would discard the statement text preceding the comment and
        // leave a dangling tail (e.g. a bare `}`) behind.
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let comment_start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if src[start..comment_start].trim().is_empty() {
                start = i;
                last_sig = None;
            }
            continue;
        }

        // Bracket nesting.
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }

        // Statement separators at depth zero.
        if depth == 0
            && (b == b'\n' || b == b';')
            && (b == b';' || is_newline_boundary(src, start, last_sig, i))
        {
            push_chunk(src, &mut chunks, start, i);
            i += 1;
            start = i;
            last_sig = None;
            continue;
        }

        if !b.is_ascii_whitespace() {
            last_sig = Some(b);
        }
        i += 1;
    }

    // Trailing chunk.
    push_chunk(src, &mut chunks, start, src.len());
    chunks
}

/// Is the `\n` at byte `i` a real statement boundary? False when the
/// pending statement is mid-expression (ends on an operator char) or
/// the next line cannot begin a statement.
fn is_newline_boundary(src: &str, start: usize, last_sig: Option<u8>, i: usize) -> bool {
    let bytes = src.as_bytes();
    // No Handl statement ends with an operator or punctuation char —
    // postfix forms end in an identifier or a closer.
    const CANNOT_END: &[u8] = b"=,:;+-*/%<>!?&|~.@";
    if let Some(b) = last_sig
        && CANNOT_END.contains(&b)
    {
        // `from "<spec>" import *` / `import *` legitimately END in
        // `*` — the star is the splice marker, not a pending binary
        // operator. Without this the following statement merges
        // into the import's trailing tokens.
        if !(b == b'*' && is_import_chunk(&src[start..i])) {
            return false;
        }
    }
    // Scan forward past whitespace and `--` comments to the next code
    // byte.
    let mut j = i + 1;
    loop {
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if j + 1 < bytes.len() && bytes[j] == b'-' && bytes[j + 1] == b'-' {
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            continue;
        }
        break;
    }
    match bytes.get(j) {
        // No Handl statement begins with these; unary `-`/`+`/`!` and
        // open brackets are excluded since they legitimately start
        // expressions.
        Some(b) => !b"=,)]}.:;|&<>~*/%?".contains(b),
        None => true,
    }
}

/// Does `chunk` start a `from`/`import` declaration? Used to exempt
/// the splice marker `import *` from the trailing-operator heuristic —
/// a `*` ending an import statement is complete, not mid-expression.
fn is_import_chunk(chunk: &str) -> bool {
    let t = chunk.trim_start();
    for kw in ["from", "import"] {
        if let Some(rest) = t.strip_prefix(kw)
            && (rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t'))
        {
            return true;
        }
    }
    false
}

/// Push a trimmed chunk from `src[start..end]` if it is non-empty and
/// not a comment-only chunk (a `--` comment at a statement boundary
/// that was not elided during the scan).
fn push_chunk(src: &str, chunks: &mut Vec<StatementChunk>, start: usize, end: usize) {
    let slice = &src[start..end];
    let lead = slice.len() - slice.trim_start().len();
    let trimmed = slice.trim();
    if trimmed.is_empty() || trimmed.starts_with("--") {
        return;
    }
    chunks.push(StatementChunk {
        text: trimmed.to_string(),
        offset: start + lead,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<(usize, String)> {
        split_statements(src)
            .into_iter()
            .map(|c| (c.offset, c.text))
            .collect()
    }

    #[test]
    fn splits_on_newlines_and_semicolons() {
        let src = "x := 1\ny := 2;\nz := 3";
        let chunks = texts(src);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].1, "x := 1");
        assert_eq!(chunks[1].1, "y := 2");
        assert_eq!(chunks[2].1, "z := 3");
    }

    #[test]
    fn keeps_bracketed_newlines_together() {
        let src = "f(\n  a,\n  b\n)\nnext := 1";
        let chunks = texts(src);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].1.contains("f("));
        assert_eq!(chunks[1].1, "next := 1");
    }

    #[test]
    fn drops_boundary_comments_keeps_trailing() {
        let src = "-- file header\nx := 1 -- trailing\ny := 2";
        let chunks = texts(src);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].1.contains("-- trailing"));
    }

    #[test]
    fn does_not_split_mid_expression() {
        // `x = a +` cannot end a statement — the newline is not a
        // boundary.
        let src = "x := a +\n  b\ny := 2";
        let chunks = texts(src);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].1.contains("a +"));
    }

    // -- declaration-line masking (module_prep parity) -----------------

    fn handl_texts(src: &str) -> Vec<(usize, String)> {
        HandlStatementSplitter
            .split(src)
            .into_iter()
            .map(|c| (c.offset, c.text))
            .collect()
    }

    #[test]
    fn declaration_lines_are_not_chunks_but_keep_offsets() {
        // The lib-otlp contract-declaration shape: `cap` lines, `op`
        // signature lines, and indented `guarantee:` continuations.
        let src = concat!(
            "cap \"otlp.configure\"\n",
            "op \"otlp.configure\" : Value -> Value\n",
            "  guarantee: True\n",
            "\n",
            "from \"otlp_c\" import * as otlp_c\n",
            "\n",
            "do {\n",
            "  { configure: (url) ~> url }\n",
            "}\n",
        );
        let chunks = handl_texts(src);
        assert_eq!(chunks.len(), 2, "chunks: {chunks:?}");
        assert!(chunks[0].1.starts_with("from \"otlp_c\" import"));
        assert!(chunks[1].1.starts_with("do {"));
        // Offsets index the *original* source: the import's offset is
        // past every declaration line.
        assert_eq!(
            &src[chunks[0].0..chunks[0].0 + chunks[0].1.len()],
            chunks[0].1
        );
        assert!(chunks[0].0 > src.find("guarantee").unwrap());
    }

    #[test]
    fn declaration_keywords_need_a_boundary() {
        // `opcode`/`capital` are ordinary identifiers, not declarations.
        let src = "opcode := 1\ncapital := 2\nhandlheld := 3";
        let chunks = handl_texts(src);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn unindented_guarantee_is_not_a_declaration() {
        // Only *indented* guarantee/requires/termination lines are
        // continuations — mirroring strip_declaration_lines.
        let src = "guarantee: True\nx := 1";
        let chunks = handl_texts(src);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn declaration_mask_preserves_byte_offsets_for_splices() {
        // Every chunk offset must be a char boundary in the original
        // and the masked span must be byte-identical in length — the
        // property preserve-mode splicing relies on.
        let src = "op \"ns.f\" : (Value * String) -> Value\n  requires: True\nx := 1\n";
        let masked = mask_declaration_lines(src);
        assert_eq!(masked.len(), src.len());
        for chunk in HandlStatementSplitter.split(src) {
            assert!(src.is_char_boundary(chunk.offset));
            assert!(src.is_char_boundary(chunk.offset + chunk.text.len()));
            assert_eq!(
                &src[chunk.offset..chunk.offset + chunk.text.len()],
                chunk.text
            );
        }
    }
}
