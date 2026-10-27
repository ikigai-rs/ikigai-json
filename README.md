# ikigai-json

JSON and CSV as **resources** for [ikigai](https://github.com/ikigai-rs): jq selection over any
resource, and CSV that reads as JSON. A view can be built from a spreadsheet export without
converting anything to RDF first:

```text
source urn:file:people.csv                                   # text/csv, as the file is
source urn:json:from-csv content=…                           # [{"name":"Ada","age":"36"},…]
source urn:json:query src=urn:file:people.csv q='[.[] | select(.age | tonumber > 50) | .name]'
                                                             # ["Grace"], CSV read through the transreptor
source urn:json:query src=urn:file:people.csv q='.[0].name' as=text/plain
                                                             # Ada, bare text a template can splice
```

The last line is what ikigai-fn's `$j{<src> <program>}` asks for in a page. If you want the graph
instead, the RDF route is still there (`ikigai-jsonld`, `ikigai-linkeddata`); this module is for
the views that do not need it.

## Resources

The space names itself `urn:iki:space:json` (`ikigai_json::SPACE_ID`).

| IRI | args | answer |
|-----|------|--------|
| `urn:json:query` | `src` (an IRI, required), `q` (a jq program, required), `as` | the results |
| `urn:json:from-csv` | `content` (CSV, piped, required), `header` (`true`/`false`, default `true`), `as` | `application/json` |
| `urn:json:to-csv` | `content` (JSON, piped, required), `as` | `text/csv` |

**`urn:json:query`** reads `src` through the kernel **under the caller's capability**. It adds no
authority of its own, so a caller who cannot read `src` cannot query it either. A `src` that is
not JSON goes through whatever transreptor chain the kernel can plan to `application/json`. That
is how jq reads CSV with no CSV query language. The collected JSON and text answers are cacheable when the read of
`src` is, and it hangs on `src`'s golden thread, so it recomputes when `src` changes.

A jq program yields a stream of results, and they become one answer like this:

| `as` | answer |
|------|--------|
| `application/json` (default) | **one result is that value; zero or several are a JSON array of them.** `.name` gives `"Ada"` and `.[].name` gives `["Ada","Grace"]`. A program that always needs an array collects its own results (`[.[].name]`). |
| `application/x-ndjson` | one result per line, nothing merged |
| `text/plain` | **exactly one scalar**, as text: a string's own characters (no quotes, no escaping), or a number, boolean or null in its JSON spelling. No result, several results, or an array or object is refused (`InvalidArgument` naming `as`) |

**`urn:json:from-csv`** reads RFC 4180 CSV through the `csv` crate. By default each record becomes
an object keyed by the header row, in column order; with `header=false` each becomes an array.
**Every value is a string.** CSV has no types, and guessing would change data silently (is `007` a
number?), so a query converts the values it needs to (`tonumber`). The converter refuses rather
than guesses on: records of different lengths, a header that names a column twice, bytes that are
not UTF-8, and anything past a bound. A leading byte-order mark is skipped. The endpoint is also
the registered `text/csv` → `application/json` transreptor.

**`urn:json:to-csv`** goes the other way, for an array of flat objects (the header is the union of
their keys, in the order first seen) or an array of arrays. It writes CRLF line endings and quotes
a field only when it has to. Null and a missing key become empty fields, and a nested value is
refused. It is the `application/json` → `text/csv` transreptor and is declared lossy, because `36`
and `"36"` write the same.

## Streaming bodies

`Endpoint::invoke` keeps the collected bytes behavior and existing bounds.
Through `Kernel::issue_body`, `as=application/x-ndjson` on native targets answers a
one-shot stream with one jq result per chunk, including its trailing newline.
Backpressure holds at most one queued result; dropping the reader stops evaluation.
The input JSON document is still read whole under the byte budget and input bound.
Each serialized result must fit the run's byte budget; the existing total output,
fuel, time, and value bounds also apply. Slow readers count toward the sandbox's
time budget. A failure after some results is an error item, never a clean end.
One-shot NDJSON answers are not cached.

The CSV body path is incremental on native and wasm targets. It uses `csv-core`,
the reader underlying the `csv` crate, to parse arbitrary input chunk boundaries
without collecting the CSV or the resulting array. It keeps the header, current
row, and current input chunk. The header and row must fit the run's byte budget,
and existing input, row, column, and field bounds still apply. Exceeding a bound
ends the body with `OverBudget`; malformed records end it with `InvalidArgument`.
An error never yields a closing bracket that would disguise a partial answer.
A replayable input produces a replayable output, but its length is unknown and
core's bounded cache declines it. A one-shot input produces a one-shot output.

On wasm, jq NDJSON still uses the existing bounded collected evaluation before
returning its one-shot body: the native evaluation worker requires threads. CSV
streaming does not require threads.

## The jq sandbox

`q` comes from a request and jq is a complete language, so the program does not run as written. It
is parsed into jaq's syntax tree, rewritten so that every construction, arithmetic operation,
comparison, string interpolation, `..` and fold step passes a guard, then compiled against a
curated library. It runs on a thread with its own 1 GiB stack (reserved address space, used only
as deep as the program actually recurses). Every pipe, call and binding is metered. A bound that is
reached ends the run with a refusal that names the bound. That refusal is an unwind, so `try`
cannot catch it.

### The bounds

| bound | default | refused as |
|-------|---------|------------|
| program size | 4 KiB | `InvalidArgument` `q` |
| input size | 16 MiB | `InvalidArgument` `src` |
| input nesting (iterative pre-scan, before parsing) | 256 | `InvalidArgument` `src` |
| work ("fuel": one unit per pipe, call, binding, collected element or walked value) | 20,000,000 | `OverBudget` |
| wall clock (native targets) | 5 s | `OverBudget` |
| recursion (stack used) | 64 MiB of the 1 GiB thread | `OverBudget` |
| value depth (checked when a value is built, compared, sorted or serialized) | 512 | `InvalidArgument` `q` |
| string built by `+`, `*`, interpolation or `tojson` | 8 MiB, checked **before** it is built | `InvalidArgument` `q` |
| integer built by arithmetic | 65,536 bits, checked before it is built | `InvalidArgument` `q` |
| array difference and `contains`/`indices` | charged by the product of the lengths, before running | `OverBudget` |
| path length (`getpath`, `setpath`, `delpaths`) | 512 | `InvalidArgument` `q` |
| total output | 8 MiB | `InvalidArgument` `q` |

The fuel budget runs out in about 0.6 s in a release build (measured with other builds sharing
the CPU), so in a release build fuel, which is deterministic, is the bound that stops a loop and
the wall clock is a backstop. A debug build takes about 3 s here and longer on a slow CI runner,
where the clock can stop it first. `tests/sandbox.rs` runs each hostile program in a child process: deep
recursion, values nested ten million deep and then serialized, compared, walked or put into an
error message, string, array and integer bombs, shared structure that would expand to 2^40 nodes,
and `fromjson` on a megabyte of `[`. Each of them has to be refused without taking the process
down. Without these guards, several of them abort the host.

### What is available

**Everything in jq that does not reach outside the document**: paths and iteration (`.a`, `.[]`,
`..`, slices, `?`), `|`, `,`, `//`, `as` bindings and destructuring, `if`, `try`/`catch`,
`reduce`, `foreach`, `label`/`break`, `def` (recursion included, under the stack bound),
assignment and update operators, string interpolation and the `@text @json @html @uri @sh @base64
@base64d` formats, and these builtins:

- **core**: `empty error not select tostring range/1-3 repeat recurse while until paths getpath
  setpath delpaths map map_values walk del first last nth limit skip join combinations to_entries
  from_entries with_entries isempty all any path path_value keys_unsorted`
- **values**: `null true false type length utf8bytelength keys has in inside contains indices index
  rindex add min max min_by max_by sort sort_by group_by unique unique_by reverse flatten pick
  transpose bsearch tojson fromjson tonumber toboolean tobytes values nulls booleans numbers
  strings arrays objects iterables scalars finites normals isboolean isnumber isstring isarray
  isobject`
- **strings**: `startswith endswith ltrimstr rtrimstr trim ltrim rtrim explode implode
  ascii_downcase ascii_upcase split/1 escape_sh` (and `/` splits a string)
- **math**: `floor round ceil abs sqrt pow log exp` and the rest of libm (`acos` … `fma`), plus
  `nan infinite isnan isinfinite isfinite isnormal`
- **time, in UTC only**: `todate fromdate todateiso8601 fromdateiso8601 gmtime mktime strftime
  strptime`

### What is not

| left out | why |
|----------|-----|
| `env`, `$ENV` | it reads the host's environment |
| `now`, `localtime`, `strflocaltime` | they read the clock or the host's time zone, so the answer would not be a function of the input |
| `input`, `inputs`, `input_filename` | a query reads exactly one document, its `src` |
| `debug`, `stderr` | they write to the host's log |
| `halt`, `halt_error` | halting is the host's decision |
| `import`, `include`, `$__loc__` | no module can be read |
| `test match capture scan split/2 splits sub gsub` | the cost of a regular-expression match cannot be bounded before it runs |

A program that names one of these is refused (`InvalidArgument` `q`) with the reason. The lists
live in `src/sandbox.rs` as `AVAILABLE`, `WRAPPED`, `EXCLUDED` and `DROPPED_DEFS`, and a test
fails as soon as jaq offers a native that has not been classified.

Two of jaq's own differences from jq are visible here. `null | .a = 1` raises an error instead of
creating `{"a":1}` (start from `{}`). Binary operators iterate the left operand on the outside:
`[(1,2) - (10,20)]` is `[-9,-19,-8,-18]`.

## Hosting it

```rust
use std::sync::Arc;
use ikigai_core::{Fallback, Kernel, Space};

let root = Fallback::new(vec![
    Arc::new(ikigai_json::space()) as Arc<dyn Space>,
    // … and whatever serves the documents people will query (ikigai-fs's urn:file:*)
]);
let kernel = Kernel::new(Arc::new(root));
```

There is nothing to configure, and the module declares no capabilities: reading `src` is the
caller's own read. It compiles to `wasm32-unknown-unknown`. On wasm there is no thread and no
clock, so the fuel, depth, size and stack-use bounds still hold, but the wall-clock budget does
not exist. A refusal there is an abort that traps the in-page kernel, because wasm cannot unwind.
The page belongs to one user and nothing else is reachable from it, so the bound holds; it just
ends the page's kernel instead of returning an error.

## License

MIT OR Apache-2.0, at your option.

This crate depends on [jaq](https://github.com/01mf02/jaq) (`jaq-core`, `jaq-std`, `jaq-json`),
copyright Michael Färber, under the MIT license, and on
[csv](https://github.com/BurntSushi/rust-csv), copyright Andrew Gallant, under MIT or Unlicense.
