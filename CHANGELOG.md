# Changelog

## 0.3.1 (unreleased)

- The jq sandbox's deadline reads the kernel's clock on every target, wasm included:
  the meter polls it at the existing 1024-fuel cadence, so a query past its time budget
  is refused on wasm too (ledger #2196). New `sandbox::run_with_clock`; a fixed or absent
  clock still leaves fuel enforcement in place.
- `json-from-csv` declares its `content` argument `reads_body`, since its body path
  streams it through `content_body`: under core 0.2.5 and later an undeclared reference
  argument is collected before the endpoint runs, so a large or streamed CSV failed
  `OverBudget` (ledger #2571). Requires ikigai-core 0.2.4. Conformance 0.12.0
  (`BODIES`, `BODY-INPUTS`) runs in the tests.

## 0.3.0

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
