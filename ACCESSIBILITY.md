# Accessibility

What `ikigai-json` emits that people read, the rules that output follows, how it is tested, and
how to report a barrier. This crate writes JSON, NDJSON, CSV and bare text answers and the text of
its errors; it writes no HTML, SVG or terminal output of its own. It is not a conformance claim.
Adapted from the [ACCESSIBILITY.md template](https://github.com/mgifford/ACCESSIBILITY.md) (MIT).

| Field | Value |
|---|---|
| Target | WCAG 2.2 Level AA, for any page that presents this output; this repository writes text only |
| Status | Evaluation in progress. No conformance claim. |
| Owner | the maintainers of this repository |
| Report a barrier | [a GitHub issue](https://github.com/ikigai-rs/ikigai-json/issues/new?labels=accessibility) with the `accessibility` label; anything security-sensitive through [private vulnerability reporting](https://github.com/ikigai-rs/ikigai-json/security/advisories/new) |
| Last reviewed | 2026-10-31 |

## What it emits that people read

- **Answers**: `urn:json:query`'s results as JSON, NDJSON (one result per line) or, with
  `as=text/plain`, one scalar as bare text that a page template can splice into its own markup
  (`src/lib.rs`); `urn:json:from-csv`'s JSON and `urn:json:to-csv`'s CSV (`src/csv_json.rs`,
  `src/streaming.rs`). They are data that people also read.
- **Error and refusal messages**: the jq sandbox's refusals (`Refusal`'s `Display` in
  `src/sandbox.rs`: `the jq program is not runnable here: …`, `the input is not usable: …`,
  `the jq program raised an error: …`, `the <bound> bound was reached: …`), and the CSV
  converters' and endpoints' `InvalidArgument` details in `src/csv_json.rs`, `src/streaming.rs`
  and `src/lib.rs`, which a host shows to whoever made the request.
- `examples/probe.rs` prints one program's results, or `REFUSED` and the refusal, to the
  terminal; it is a developer's probe, not a shipped binary.

Not this repository's: the page a template splices a `text/plain` answer into (its escaping and
markup are the template's), and the page, error page or terminal a host shows any of this in.

## The rules that output follows

Followed by reading the source:

- A refusal is an `InvalidArgument` naming the argument it is about (`q`, `src`, `as`, `header`,
  `content`) or an `OverBudget`, with a sentence that says what happened: the bound and its
  number, the record number and field count for a CSV record of the wrong length, the column a
  header names twice, the builtin that is not available here and why. Several also say what to
  do: "use header=false for arrays", "leave `as` out for JSON", "narrow the selection or the
  input".
- Quoted input is clipped (a jq runtime error to 1,024 bytes, a nested CSV value to 40
  characters, the text at a parse error to 24), so an error message stays readable whatever the input was.
- Nothing is written in color; every answer is plain text.

## How it is tested

- `tests/endpoints.rs`: `refusals_are_typed_and_name_the_argument`,
  `a_src_nothing_converts_is_refused_naming_src`, `dollar_j_splices_a_single_scalar_as_bare_text`,
  `csv_endpoints_round_trip_and_refuse_what_they_cannot_hold` and `csv_edge_cases` check that
  refusals name the argument and carry the words that explain them (the record number, `twice`,
  the bound's name).
- `tests/sandbox.rs` checks that an excluded builtin is refused with its reason (`environment`,
  `not available`), and `a_syntax_error_says_where` that a parse error says what was expected.
- **No automated accessibility checker, browser or assistive technology has been used on this
  output.**

## Known limitations

Found by reading the source on 2026-10-31, not by testing:

- On wasm a bound that is reached aborts the in-page kernel (wasm cannot unwind), so the person
  gets no refusal message at all, only a kernel that stopped (documented in the README).
- The wall-clock refusal (`the jq program ran past its … ms budget`) does not say what to do,
  unlike the fuel refusal beside it.
- A jq runtime error is jaq's own message after `the jq program raised an error:`; it is not
  reworded and may not name the part of the program that raised it.
- `the NDJSON worker panicked` and `starting NDJSON worker: …` name no input and say nothing
  about what to do.

## Contributing

Read this file before changing any output listed above: an answer's shape or an error message.
Keep an error a sentence that names the argument and says what to do. Say in the pull request what
was tested, with what, and what was not.

## Reporting a barrier

[Open an issue](https://github.com/ikigai-rs/ikigai-json/issues/new?labels=accessibility) with the
`accessibility` label (the link sets it when GitHub lets you add labels; otherwise a maintainer
adds it): what you were doing, what happened, and your terminal or browser and any assistive
technology, if you are comfortable sharing them. Anything security-sensitive goes through
[private vulnerability reporting](https://github.com/ikigai-rs/ikigai-json/security/advisories/new).
**We acknowledge a report within 7 days**, and promise nothing further.

## Change history

| Date | Change |
|---|---|
| 2026-10-31 | First version. |
