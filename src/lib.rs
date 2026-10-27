//! `ikigai-json`: JSON and CSV as resources for ikigai.
//!
//! ```text
//! source urn:json:query src=urn:file:people.csv q='.[] | select(.age | tonumber > 30) | .name'
//! ```
//!
//! A standalone **module crate**: a host links it in and mounts [`space`]. It depends on the
//! published `ikigai-core` kernel, **jaq** (a jq implementation in Rust, MIT) and the `csv`
//! crate, uses no OS or platform APIs, and compiles to `wasm32-unknown-unknown`, so the book's
//! in-page kernel can link it.
//!
//! ## Three resources
//!
//! | IRI | args | answer |
//! |-----|------|--------|
//! | `urn:json:query` | `src` (an IRI, required), `q` (a jq program, required), `as` | the results, as JSON |
//! | `urn:json:from-csv` | `content` (CSV, piped, required), `header` (default `true`), `as` | `application/json` |
//! | `urn:json:to-csv` | `content` (JSON, piped, required), `as` | `text/csv` |
//!
//! `urn:json:query` reads `src` **through the kernel, under the caller's capability** (it adds
//! no authority of its own), so its answer is golden-threaded from `src`: cacheable exactly when
//! that read is, and recomputed when it changes. A `src` that is not JSON is converted through
//! whatever transreptor the kernel can plan to `application/json`, which is how a query reads
//! CSV with no CSV query language: [`csv_to_json`] is registered as the `text/csv` →
//! `application/json` transreptor.
//!
//! ## How results become one answer
//!
//! A jq program yields a STREAM of values. The answer is:
//!
//! - `as=application/json` (the default): **exactly one result is that value; zero or several
//!   are a JSON array of them**. So `.name` answers `"Ada"`, and `.[] | .name` answers
//!   `["Ada","Grace"]`. A program that must always answer an array collects itself: `[.[] |
//!   .name]` answers `["Ada"]` for one person, where the bare stream would answer `"Ada"`.
//! - `as=application/x-ndjson`: one result per line, nothing merged. The unambiguous form.
//! - `as=text/plain`: **exactly one SCALAR result**, as text: a string's own characters (no
//!   quotes, no escapes), a number, boolean or null as its JSON spelling. Anything else (no
//!   result, several, an array or object) is refused with `InvalidArgument` naming `as`. This is
//!   what a template splices.
//!
//! ## The sandbox
//!
//! The program comes from a request, so it runs in [`sandbox`]: no environment, clock, input,
//! logging, halting or regular expressions; bounded program size, input size and nesting, work
//! (fuel), wall-clock time, value depth, string, integer and output size; and its own large
//! stack. A bound that is reached refuses the request and names the bound.

pub mod csv_json;
pub mod sandbox;
mod scan;
mod streaming;

pub use csv_json::{csv_to_json, json_to_csv, CsvRefusal};
pub use scan::{nesting, MAX_JSON_NESTING};

use ikigai_core::{
    space_iri, ArgRef, ArgSpec, AsyncFnEndpoint, Description, EndpointSpace, Error, Exact,
    FnEndpoint, Invocation, Iri, ReprType, Representation, Request, Result, Verb,
};

/// The media type of a JSON answer.
pub const JSON: &str = "application/json";
/// The media type of a newline-delimited stream of JSON results.
pub const NDJSON: &str = "application/x-ndjson";
/// The media type of a single scalar result as text.
pub const TEXT: &str = "text/plain";
/// The media type of CSV (RFC 4180, registered by RFC 7111).
pub const CSV: &str = "text/csv";

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_ANY_URI: &str = "http://www.w3.org/2001/XMLSchema#anyURI";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

/// The name [`space`] claims: `urn:iki:space:json`.
pub const SPACE_ID: &str = "urn:iki:space:json";

// --- arguments ---------------------------------------------------------------------------

fn invalid(name: &str, detail: impl Into<String>) -> Error {
    Error::InvalidArgument {
        name: name.to_string(),
        detail: detail.into(),
    }
}

/// The `src` argument: a reference, or an IRI written inline.
fn src_iri(inv: &Invocation<'_>) -> Result<Iri> {
    match inv.request.args.get("src") {
        Some(ArgRef::Reference(iri)) => Ok(iri.clone()),
        Some(_) => {
            let text = inv.inline_str("src")?.trim();
            Iri::parse(text).map_err(|e| invalid("src", format!("not an IRI: {e}")))
        }
        None => Err(Error::MissingArgument("src".to_string())),
    }
}

/// The `as` argument, one of `allowed`, defaulting to the first.
fn wanted<'a>(inv: &Invocation<'_>, allowed: &[&'a str]) -> Result<&'a str> {
    let Ok(raw) = inv.inline_str("as") else {
        return Ok(allowed[0]);
    };
    let bare = raw.split(';').next().unwrap_or("").trim();
    allowed
        .iter()
        .find(|a| a.eq_ignore_ascii_case(bare))
        .copied()
        .ok_or_else(|| {
            invalid(
                "as",
                format!("this resource answers {}, not `{bare}`", allowed.join(", ")),
            )
        })
}

fn is_json(media: &str) -> bool {
    let bare = media
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    bare == JSON || bare == "text/json" || bare.ends_with("+json")
}

/// Read `src` through the kernel as JSON: as served if it is, else through a planned
/// transreption to `application/json`.
async fn read_json(inv: &Invocation<'_>, src: &Iri) -> Result<Vec<u8>> {
    let answer = inv.source(src).await?;
    let served = answer.repr_type.media_type.clone();
    if is_json(&served) {
        return Ok(answer.bytes);
    }
    let chain = inv.select_transreptor(&served, JSON).ok_or_else(|| {
        invalid(
            "src",
            format!("`{src}` is `{served}`, and nothing this kernel mounts converts it to {JSON}"),
        )
    })?;
    let mut current = answer;
    for step in chain {
        let iri = Iri::parse(&step.endpoint).map_err(|e| {
            Error::Endpoint(format!("bad transreptor IRI `{}`: {e}", step.endpoint))
        })?;
        let request = Request::new(Verb::Source, iri)
            .with_arg("content", ArgRef::Inline(current.bytes))
            .with_arg("as", ArgRef::Inline(step.to.into_bytes()));
        current = inv.issue(request).await?;
    }
    Ok(current.bytes)
}

fn refusal(r: sandbox::Refusal) -> Error {
    use sandbox::Refusal::*;
    match r {
        Program(_) | Runtime(_) => invalid("q", r.to_string()),
        Input(_) => invalid("src", r.to_string()),
        Bound {
            bound: "fuel" | "stack",
            ..
        } => Error::OverBudget {
            detail: r.to_string(),
        },
        Bound { .. } => invalid("q", r.to_string()),
        Timeout(detail) => Error::OverBudget { detail },
    }
}

/// Merge a run's results into one representation, by the rule in the crate docs.
fn answer(outputs: Vec<sandbox::Output>, media: &str) -> Result<Representation> {
    let bytes = match media {
        TEXT => match outputs.as_slice() {
            [one] => match &one.scalar_text {
                Some(text) => text.clone().into_bytes(),
                None => {
                    return Err(invalid(
                        "as",
                        "as=text/plain answers a single scalar, and the result is an array or \
                         object; leave `as` out for JSON",
                    ))
                }
            },
            _ => {
                return Err(invalid(
                    "as",
                    format!(
                        "as=text/plain answers exactly one scalar result, and the program \
                         yielded {}; leave `as` out for JSON",
                        outputs.len()
                    ),
                ))
            }
        },
        NDJSON => {
            let mut s = String::new();
            for o in &outputs {
                s.push_str(&o.json);
                s.push('\n');
            }
            s.into_bytes()
        }
        _ => match outputs.as_slice() {
            [one] => one.json.clone().into_bytes(),
            many => {
                let parts: Vec<&str> = many.iter().map(|o| o.json.as_str()).collect();
                format!("[{}]", parts.join(",")).into_bytes()
            }
        },
    };
    let repr_type = if media == TEXT {
        ReprType::new(TEXT).with_param("charset", "utf-8")
    } else {
        ReprType::new(media)
    };
    Ok(Representation::new(repr_type, bytes).cacheable())
}

// --- endpoints ------------------------------------------------------------------------------

/// `urn:json:query`: run the jq program `q` over the JSON resource `src`.
fn query_bytes() -> AsyncFnEndpoint {
    AsyncFnEndpoint::new("json-query", |inv| {
        Box::pin(async move {
            let program = inv.inline_str("q")?.to_string();
            let media = wanted(inv, &[JSON, NDJSON, TEXT])?;
            let src = src_iri(inv)?;
            let input = read_json(inv, &src).await?;
            let outputs =
                sandbox::run(&program, &input, &sandbox::Limits::default()).map_err(refusal)?;
            answer(outputs, media)
        })
    })
    .with_description(
        Description::new("json-query")
            .title("jq query")
            .summary(
                "Runs the jq program `q` (jaq, sandboxed) over the JSON resource `src`, read \
                 under the caller's capability; a non-JSON `src` is converted through the \
                 kernel's transreptors (CSV reads as an array of objects). One result answers \
                 that value, zero or several a JSON array; as=application/x-ndjson answers one \
                 per line, as=text/plain a single scalar as bare text. No environment, clock, \
                 input or regular expressions; work, time, depth and size are bounded.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .input(
                ArgSpec::new("src")
                    .summary("the JSON resource to query (any IRI the caller may read)")
                    .class(XSD_ANY_URI),
            )
            .input(
                ArgSpec::new("q")
                    .summary("the jq program, e.g. `.[] | select(.age > 30) | .name`")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("as")
                    .summary("the answer's media type")
                    .class(XSD_STRING)
                    .one_of([JSON, NDJSON, TEXT])
                    .default_value(JSON),
            )
            .output(JSON)
            .output(NDJSON)
            .output("text/plain;charset=utf-8"),
    )
}

/// `urn:json:from-csv`: read the piped CSV as JSON (see [`csv_to_json`]). Also the registered
/// `text/csv` → `application/json` transreptor.
fn from_csv_bytes() -> FnEndpoint {
    FnEndpoint::new("json-from-csv", |inv: &Invocation<'_>| {
        wanted(inv, &[JSON])?;
        let header = match inv.inline_str("header").map(str::trim) {
            Err(_) | Ok("true") => true,
            Ok("false") => false,
            Ok(other) => {
                return Err(invalid(
                    "header",
                    format!("expected true or false, got {other:?}"),
                ))
            }
        };
        let json = csv_to_json(inv.inline_arg("content")?, header)
            .map_err(|r| invalid("content", r.detail))?;
        Ok(Representation::new(ReprType::new(JSON), json.into_bytes()).cacheable())
    })
    .with_description(
        Description::new("json-from-csv")
            .title("CSV as JSON")
            .summary(
                "Reads the piped RFC 4180 CSV as JSON: an array of objects keyed by the header \
                 row, or with header=false an array of arrays. Every value is a string (CSV has \
                 no types; this does not guess). Ragged records, a repeated column name and \
                 non-UTF-8 bytes are refused, as is anything past the size, row, column and \
                 field bounds. Also the registered text/csv to application/json transreptor.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .input(
                ArgSpec::new("content")
                    .summary("the CSV, filled from the pipe")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("header")
                    .summary("whether the first record names the columns")
                    .class(XSD_BOOLEAN)
                    .one_of(["true", "false"])
                    .default_value("true"),
            )
            .input(
                ArgSpec::new("as")
                    .summary("the answer's media type (a transreption step sets it)")
                    .class(XSD_STRING)
                    .one_of([JSON])
                    .default_value(JSON),
            )
            .output(JSON)
            .transreptor([CSV], [JSON]),
    )
}

/// `urn:json:to-csv`: write the piped JSON array of flat records as CSV (see
/// [`json_to_csv`]). Also the registered `application/json` → `text/csv` transreptor, declared
/// a projection because types do not survive.
pub fn to_csv() -> FnEndpoint {
    FnEndpoint::new("json-to-csv", |inv: &Invocation<'_>| {
        wanted(inv, &[CSV])?;
        let csv =
            json_to_csv(inv.inline_arg("content")?).map_err(|r| invalid("content", r.detail))?;
        Ok(Representation::new(
            ReprType::new(CSV).with_param("charset", "utf-8"),
            csv.into_bytes(),
        )
        .cacheable())
    })
    .with_description(
        Description::new("json-to-csv")
            .title("JSON as CSV")
            .summary(
                "Writes the piped JSON array of flat objects (or of arrays) as RFC 4180 CSV \
                 with CRLF line ends. The header is the union of the objects' keys in the order \
                 first seen; null and a missing key are empty fields; a nested value is \
                 refused. Lossy: `36` and \"36\" write the same.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .input(
                ArgSpec::new("content")
                    .summary("the JSON, filled from the pipe")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("as")
                    .summary("the answer's media type (a transreption step sets it)")
                    .class(XSD_STRING)
                    .one_of([CSV])
                    .default_value(CSV),
            )
            .output("text/csv;charset=utf-8")
            .transreptor([JSON], [CSV])
            .lossy(),
    )
}

/// The module's space: `urn:json:query`, `urn:json:from-csv` and `urn:json:to-csv`, named
/// [`SPACE_ID`]. Configuration-free, so it names itself.
///
/// ```
/// # use std::sync::Arc;
/// # use futures::executor::block_on;
/// use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
///
/// let kernel = Kernel::new(Arc::new(ikigai_json::space()));
/// let request = Request::new(Verb::Source, Iri::parse("urn:json:from-csv").unwrap())
///     .with_arg("content", ArgRef::Inline(b"name\nAda\n".to_vec()));
/// let json = block_on(kernel.issue(request, &Capability::root())).unwrap();
/// assert_eq!(json.repr_type.media_type, "application/json");
/// assert_eq!(json.bytes, br#"[{"name":"Ada"}]"#);
/// ```
pub fn space() -> EndpointSpace {
    EndpointSpace::new()
        .bind(Exact::new("urn:json:query"), query())
        .bind(Exact::new("urn:json:from-csv"), from_csv())
        .bind(Exact::new("urn:json:to-csv"), to_csv())
        // LAST: a later `bind` drops the name.
        .named(space_iri("json"))
}

/// The jq endpoint, with a one-shot NDJSON body path beside the bounded bytes path.
pub fn query() -> impl ikigai_core::Endpoint {
    streaming::Query(query_bytes())
}

/// The CSV endpoint, with an incremental body path beside the bounded bytes path.
pub fn from_csv() -> impl ikigai_core::Endpoint {
    streaming::FromCsv(from_csv_bytes())
}
