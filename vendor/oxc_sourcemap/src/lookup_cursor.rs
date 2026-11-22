use crate::{
    Token,
    mappings::{LineStart, Mappings},
};

/// Lookups through a cursor mostly move forward along a line, so a lookup scans this many tokens
/// from the previous result before it falls back to binary search.
const FORWARD_SCAN: usize = 8;

/// The number of decoded lines that a cursor keeps. Line `n` goes to slot `n % CACHED_LINES`.
const CACHED_LINES: usize = 8;

/// A stateful cursor for many token lookups in one [`SourceMap`](crate::SourceMap).
///
/// Created by [`SourceMap::lookup_cursor`](crate::SourceMap::lookup_cursor). It does not decode
/// the whole map: it keeps where each generated line starts in the map's [`Mappings`], decodes a
/// line when a lookup needs it, and keeps the last few decoded lines. It remembers the line and
/// token index of its previous result. When the next query is on the same line and every
/// token before that index is before the queried column, the search starts there: a short forward
/// scan, then a binary search over the rest of the line. Otherwise it searches the whole line.
/// This makes runs of mostly-forward queries, such as tracing every token of one map through
/// another when composing sourcemaps, cheaper than independent lookups.
///
/// The previous result only changes where the search starts, never its answer:
/// [`lookup_token`](Self::lookup_token) and [`lookup_token_approx`](Self::lookup_token_approx)
/// return exactly what [`SourceMap::lookup_token`] and [`SourceMap::lookup_token_approx`]
/// return for the same map, in any query order, as long as each line's tokens are sorted by
/// generated column.
///
/// Like those lookups, the cursor assumes that order. Maps with unsorted segments on a line are
/// not supported: the cursor's results for them are unspecified and can differ from the
/// stateless lookups.
///
/// ```
/// use oxc_sourcemap::SourceMapBuilder;
///
/// let mut builder = SourceMapBuilder::default();
/// let source = builder.add_source_and_content("a.js", "");
/// builder.add_token(0, 2, 0, 0, Some(source), None);
/// builder.add_token(0, 6, 0, 4, Some(source), None);
/// let map = builder.into_sourcemap();
///
/// let mut cursor = map.lookup_cursor();
/// // Clamps to the line's first token.
/// assert_eq!(cursor.lookup_token_approx(0, 0).map(|t| t.get_dst_col()), Some(2));
/// // The last token before the column.
/// assert_eq!(cursor.lookup_token_approx(0, 5).map(|t| t.get_dst_col()), Some(2));
/// assert_eq!(cursor.lookup_token_approx(0, 9).map(|t| t.get_dst_col()), Some(6));
/// // The exact lookup does not clamp.
/// assert_eq!(cursor.lookup_token(0, 0), None);
/// ```
#[derive(Debug, Clone)]
pub struct LookupCursor<'a> {
    mappings: &'a Mappings,
    /// See [`Mappings::line_starts`].
    lines: Vec<LineStart>,
    /// Decoded lines: the line number (`u32::MAX` when empty) and its tokens.
    cache: [(u32, Vec<Token>); CACHED_LINES],
    /// The line and token index of the previous result.
    prev: Option<(u32, usize)>,
}

impl<'a> LookupCursor<'a> {
    pub(crate) fn new(mappings: &'a Mappings) -> Self {
        Self {
            mappings,
            lines: mappings.line_starts(),
            cache: std::array::from_fn(|_| (u32::MAX, Vec::new())),
            prev: None,
        }
    }

    /// The tokens of generated line `line`, or `None` past the last line with a token.
    #[inline]
    fn line(&mut self, line: u32) -> Option<&[Token]> {
        let slot = &mut self.cache[line as usize % CACHED_LINES];
        if slot.0 != line {
            let start = self.lines.get(line as usize)?;
            let end = self.lines.get(line as usize + 1)?.index();
            self.mappings.decode_line(start, end, &mut slot.1);
            slot.0 = line;
        }
        Some(&slot.1)
    }

    /// Lookup a token by line and column, like [`SourceMap::lookup_token`](crate::SourceMap::lookup_token).
    ///
    /// Returns the first token at `col` if there is one, else the last token before it. Returns
    /// `None` when `col` is before the line's first token, or the line is empty or missing.
    #[inline]
    pub fn lookup_token(&mut self, line: u32, col: u32) -> Option<Token> {
        let prev = self.prev;
        let (tokens, next) = Self::first_at_or_after(prev, self.line(line)?, line, col);
        let index = if tokens.get(next).is_some_and(|token| token.dst_col == col) {
            next
        } else {
            next.checked_sub(1)?
        };
        let token = *tokens.get(index)?;
        self.prev = Some((line, index));
        Some(token)
    }

    /// Lookup a token by line and column, like [`SourceMap::lookup_token_approx`](crate::SourceMap::lookup_token_approx).
    ///
    /// Returns the first token at `col` if there is one, else the last token before it, clamped
    /// to the line's first token. Returns `None` when the line is empty or missing.
    #[inline]
    pub fn lookup_token_approx(&mut self, line: u32, col: u32) -> Option<Token> {
        let prev = self.prev;
        let (tokens, next) = Self::first_at_or_after(prev, self.line(line)?, line, col);
        let index = if tokens.get(next).is_some_and(|token| token.dst_col == col) {
            next
        } else {
            next.saturating_sub(1)
        };
        // An empty line has no token.
        let token = *tokens.get(index)?;
        self.prev = Some((line, index));
        Some(token)
    }

    /// Returns `tokens`, the tokens of `line`, and the index of the first token at or after
    /// `col`. `prev` is the previous result.
    #[inline]
    fn first_at_or_after(
        prev: Option<(u32, usize)>,
        tokens: &[Token],
        line: u32,
        col: u32,
    ) -> (&[Token], usize) {
        // The search may start at the previous result only when every token before it is before
        // `col`, so a previous result never changes the answer.
        let mut next = match prev {
            Some((prev_line, index))
                if prev_line == line && (index == 0 || tokens[index - 1].dst_col < col) =>
            {
                index
            }
            _ => 0,
        };
        // Bound the scan so distant jumps cannot turn a lookup into a linear search.
        let end = tokens.len().min(next + FORWARD_SCAN);
        while next < end && tokens[next].dst_col < col {
            next += 1;
        }
        if next == end {
            next += tokens[next..].partition_point(|token| token.dst_col < col);
        }
        (tokens, next)
    }
}

#[cfg(test)]
mod lazy_tests {
    use crate::{SourceMap, Token};

    /// A small xorshift generator, so the test needs no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: u32) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % u64::from(bound)) as u32
        }
    }

    fn random_map(rng: &mut Rng) -> SourceMap<'static> {
        let mut tokens = Vec::new();
        let lines = 1 + rng.next(40);
        for line in 0..lines {
            // Some lines have no tokens.
            if rng.next(4) == 0 {
                continue;
            }
            let mut col = rng.next(5);
            for _ in 0..rng.next(30) {
                let token = match rng.next(5) {
                    0 => Token::unmapped(line, col),
                    1 => Token::new(line, col, rng.next(50), rng.next(80), Some(0), Some(rng.next(3))),
                    _ => Token::new(line, col, rng.next(50), rng.next(80), Some(rng.next(2)), None),
                };
                tokens.push(token);
                col += rng.next(6);
            }
        }
        SourceMap::new(
            None,
            vec!["a".into(), "b".into(), "c".into()],
            None,
            vec![Some("x.js".into()), Some("y.js".into())],
            vec![],
            tokens,
        )
    }

    #[test]
    fn lazy_cursor_matches_the_lookup_table() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..300 {
            let map = random_map(&mut rng);
            let table = map.generate_lookup_table();
            let mut cursor = map.lookup_cursor();
            for _ in 0..400 {
                // Mostly nearby lines, sometimes far jumps and lines past the end.
                let line = rng.next(45);
                let col = rng.next(120);
                assert_eq!(
                    cursor.lookup_token(line, col),
                    map.lookup_token(&table, line, col),
                    "lookup_token({line}, {col})"
                );
                assert_eq!(
                    cursor.lookup_token_approx(line, col),
                    map.lookup_token_approx(&table, line, col),
                    "lookup_token_approx({line}, {col})"
                );
            }
        }
    }
}
