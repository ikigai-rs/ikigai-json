//! The iterative pre-scan that bounds JSON nesting BEFORE any parser sees the bytes.
//!
//! A stack overflow aborts the whole host, on any thread, and a size bound does not bound
//! recursion: a few kilobytes of `[` nest thousands deep. So every JSON this crate parses from
//! a request (the `src` of a query, the string `fromjson` reads, the array `urn:json:to-csv`
//! converts) is scanned here first, in one pass and without recursion.

/// The deepest JSON a request may hand this crate, counting objects and arrays together outside
/// strings: **256**.
///
/// Real documents are shallow (the W3C JSON-LD suite's deepest is 10, a typical API response
/// under 20). 256 leaves room for anything a person or a service writes and is far below what a
/// parser's recursion can survive on the evaluation thread.
pub const MAX_JSON_NESTING: usize = 256;

/// The nesting depth of `bytes` read as JSON, or the first depth past `limit` (so the scan
/// stops there).
///
/// One pass, no recursion. A string opens at a `"` outside one and closes at the next `"` that
/// a `\` does not escape, so a bracket inside a string is text. It counts the whole input, past
/// the point where a parser would stop at an error, which is the safe direction: it can
/// over-count, never hide a level the parser would read.
///
/// ```
/// use ikigai_json::nesting;
///
/// assert_eq!(nesting(b"1", 10), 0);
/// assert_eq!(nesting(br#"{"a":[1,{"b":[]}]}"#, 10), 4);
/// // Brackets inside a string are text, and an escaped quote does not end the string.
/// assert_eq!(nesting(br#"["[[[\"[[["]"#, 10), 1);
/// // The scan stops one past the limit rather than counting the rest.
/// assert_eq!(nesting("[".repeat(1000).as_bytes(), 10), 11);
/// ```
pub fn nesting(bytes: &[u8], limit: usize) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > deepest {
                    deepest = depth;
                    if deepest > limit {
                        return deepest;
                    }
                }
            }
            // A closer with nothing open is a syntax error the parser reports.
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    deepest
}
