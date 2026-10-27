# Changelog

## 0.3.0 (unreleased)

- Add a one-shot NDJSON body for jq results on native targets, with backpressure,
  cancellation, per-value byte bounds, and terminal error items. jq still reads a
  bounded input document whole and retains its total output and sandbox bounds.
- Add incremental CSV-to-JSON bodies using csv-core, the reader underlying csv.
  Keep only the header, current row, and current input chunk. Preserve replayability
  when the input can be read again; unknown output length can prevent cache admission.
- Keep the existing `Endpoint::invoke` implementations and their bounds. `query()`
  and `from_csv()` now return `impl Endpoint` instead of concrete function endpoints.
- Require ikigai-core 0.2.1 and declare the failure classes propagated by body reads.
- On wasm, jq NDJSON remains a bounded collected one-shot body; CSV is incremental.
  Conformance 0.12.0 is not yet available on crates.io, so tests use 0.11.0 plus
  explicit body equivalence, early-stop, large-input, and budget checks.
