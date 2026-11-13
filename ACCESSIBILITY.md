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
| Last reviewed | 2026-11-12 |

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

Recorded by source review on 2026-10-31; this historical review is Blocked below, not
a current test result:

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

Every named check has a result: Pass, Fail or Blocked, with evidence. A Pass covers only
that check, not overall accessibility. The current tests below ran on 2026-11-12 against
main at `7b780f7b3b3af9600dacbc21af08a5acc9e528ab`, before this documentation change,
using Cargo 1.99.1 and rustc 1.99.1. The command was
`cargo test --test endpoints --test sandbox`: 11 endpoint tests and 24 sandbox tests passed.
CI's `cargo test --workspace` repeats these suites; it does not run assistive technology or
render a host's output.

| Check | Result | Evidence |
|---|---|---|
| Endpoint refusal arguments and explanatory text | Pass | `tests/endpoints.rs`: `refusals_are_typed_and_name_the_argument`, `a_src_nothing_converts_is_refused_naming_src`, `csv_endpoints_round_trip_and_refuse_what_they_cannot_hold` and `csv_edge_cases`, run with the command and versions above. Checks argument names, the CSV record number, duplicate-header wording and named bounds; does not evaluate every message's clarity or recovery guidance. |
| Bare scalar text, JSON, NDJSON and CSV answer shapes | Pass | `tests/endpoints.rs`: `dollar_j_splices_a_single_scalar_as_bare_text`, `one_result_is_that_value_and_several_are_an_array`, `ndjson_is_one_result_per_line` and `csv_endpoints_round_trip_and_refuse_what_they_cannot_hold`, run with the command and versions above. Checks returned data, not host markup, escaping, reading order or presentation. |
| Excluded-builtin reasons and syntax-error wording | Pass | `tests/sandbox.rs`: `each_excluded_definition_is_refused_by_name` and `a_syntax_error_says_where`, run with the command and versions above. Checks `environment`, `not available` and `expected`; does not establish that a person can locate or recover from every error. |
| Source review of refusal guidance, clipping and plain-text output | Blocked | Historical source review dated 2026-10-31 described in “The rules that output follows” and “Known limitations”; its command and tool version were not recorded and CI does not repeat that review. The tests above cover only their stated assertions. |
| Wasm bound-refusal behavior | Blocked | The 2026-10-31 source review identified the abort limitation below; no current wasm runtime evaluation was performed. CI's wasm library lint compiles code but does not exercise refusal delivery in a browser. |
| Automated accessibility checker | Blocked | No axe or other accessibility checker was run on this output; no rendered host page or browser checker is configured in this repository's CI. Rust tests are not accessibility checkers. |
| Browser presentation, zoom, reflow and contrast | Blocked | No browser evaluation was performed. This crate supplies text data; a host page is needed to evaluate its presentation. |
| Manual keyboard | Blocked | No person performed a keyboard pass on the probe or a host presenting this output; no interactive evaluation session was available for this update. |
| Screen reader | Blocked | No screen reader session was available or used on this output for this update. |
| Screen magnifier | Blocked | No screen magnifier session was available or used on this output for this update. |
| Speech input | Blocked | No speech-input session was available or used on this output for this update. |
| Switch access | Blocked | No switch-access session was available or used on this output for this update. |
| Braille display | Blocked | No braille display was available or used on this output for this update. |

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
was tested in a `Check | Result | Evidence` table. Give every named check a Pass, Fail or
Blocked result, with the test or command and tool version, or why it could not run. Keep unrun
assistive technology checks and historical evaluations CI does not repeat as Blocked, with
the historical date where known.

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
| 2026-11-12 | Record current main test evidence and explicit Blocked checks; retain historical source-review limitations. |
