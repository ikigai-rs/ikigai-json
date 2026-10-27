mod common;

use common::{Host, PEOPLE_IRI};
use futures::executor::block_on;
use ikigai_core::Capability;

#[test]
fn ndjson_body_is_one_shot() {
    let host = Host::new();
    let request = host.request(
        "urn:json:query",
        &[
            ("src", PEOPLE_IRI),
            ("q", ".[]"),
            ("as", ikigai_json::NDJSON),
        ],
    );
    let answer = block_on(host.kernel.issue_body(request, &Capability::root())).unwrap();
    assert!(
        answer.bytes.is_one_shot(),
        "jq results must not be collected before returning the body"
    );
}

#[test]
fn csv_declares_incremental() {
    use ikigai_core::Endpoint;
    assert!(
        ikigai_json::from_csv()
            .describe()
            .transreption()
            .unwrap()
            .incremental
    );
}

use async_trait::async_trait;
use futures::{stream, StreamExt};
use ikigai_core::{
    ArgRef, Bindings, Body, ByteStream, Bytes, Endpoint, Error, Exact, Invocation, Iri,
    IssueContext, Kernel, Replay, ReprType, Representation, Request, Result, Verb,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
struct Input {
    chunks: Vec<Result<Bytes>>,
    repeats: usize,
    polled: Arc<AtomicUsize>,
}

impl Replay for Input {
    fn open(&self) -> ByteStream {
        let chunks = self.chunks.clone();
        let polled = self.polled.clone();
        let repeats = self.repeats;
        Box::pin(stream::unfold(0, move |index| {
            let item = if index < chunks.len() * repeats {
                polled.fetch_add(1, Ordering::SeqCst);
                Some(chunks[index % chunks.len()].clone())
            } else {
                None
            };
            async move { item.map(|item| (item, index + 1)) }
        }))
    }
}

#[async_trait]
impl Endpoint for Input {
    async fn invoke(&self, _: &Invocation<'_>) -> Result<Representation> {
        panic!("the body input must never be collected through invoke")
    }
    async fn invoke_body(&self, _: &Invocation<'_>) -> Result<Representation<Body>> {
        Ok(Representation::from_body(
            ReprType::new("text/csv"),
            Body::replay(self.clone()),
        ))
    }
}

fn input(chunks: Vec<Result<Bytes>>, repeats: usize) -> (Kernel, Arc<AtomicUsize>) {
    let polled = Arc::new(AtomicUsize::new(0));
    let source = Input {
        chunks,
        repeats,
        polled: polled.clone(),
    };
    let kernel = Kernel::new(Arc::new(
        ikigai_json::space().bind(Exact::new("urn:input"), source),
    ));
    (kernel, polled)
}

fn csv_request(header: bool) -> Request {
    Request::new(Verb::Source, Iri::parse("urn:json:from-csv").unwrap())
        .with_arg(
            "content",
            ArgRef::Reference(Iri::parse("urn:input").unwrap()),
        )
        .with_arg("header", ArgRef::Inline(header.to_string().into_bytes()))
}

#[test]
fn csv_body_matches_bytes_at_every_chunk_boundary() {
    for csv in [
        "",
        "a",
        "\u{feff}name,age\r\nAda,36\r\n\"Hopper, Grace\",85\r\n",
        "a,b\nx,\"quoted\nline\"\n",
        "a,b\n\"a\"\"b\",é\n",
        "a,b\n,\n",
    ] {
        for header in [true, false] {
            let expected = ikigai_json::csv_to_json(csv.as_bytes(), header).unwrap();
            for split in 0..=csv.len() {
                let (kernel, _) = input(
                    vec![
                        Ok(Bytes::copy_from_slice(&csv.as_bytes()[..split])),
                        Ok(Bytes::new()),
                        Ok(Bytes::copy_from_slice(&csv.as_bytes()[split..])),
                    ],
                    1,
                );
                let answer =
                    block_on(kernel.issue_body(csv_request(header), &Capability::root())).unwrap();
                let bytes = block_on(answer.bytes.collect(1024 * 1024)).unwrap();
                assert_eq!(
                    bytes.as_ref(),
                    expected.as_bytes(),
                    "split={split} header={header} csv={csv:?}"
                );
            }
        }
    }
}

#[test]
fn csv_early_stop_does_not_poll_the_remaining_input() {
    let (kernel, polled) = input(vec![Ok(Bytes::from_static(b"a,b\n"))], 100_000);
    let answer = block_on(kernel.issue_body(csv_request(false), &Capability::root())).unwrap();
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    let mut body = answer.bytes.into_stream();
    assert_eq!(block_on(body.next()).unwrap().unwrap(), "[");
    assert_eq!(block_on(body.next()).unwrap().unwrap(), "[\"a\",\"b\"]");
    drop(body);
    assert_eq!(polled.load(Ordering::SeqCst), 1);
}

#[test]
fn large_csv_is_processed_with_a_budget_smaller_than_the_input_and_output() {
    let (kernel, polled) = input(vec![Ok(Bytes::from_static(b"a,b\n"))], 100_000);
    let answer = block_on(kernel.issue_body_with_context(
        csv_request(false),
        &Capability::root(),
        IssueContext::new().with_memory_limit(64),
    ))
    .unwrap();
    let mut body = answer.bytes.into_stream();
    let mut bytes = 0;
    while let Some(chunk) = block_on(body.next()) {
        bytes += chunk.unwrap().len();
    }
    assert_eq!(bytes, 1_000_001);
    assert_eq!(polled.load(Ordering::SeqCst), 100_000);
}

#[test]
fn csv_refuses_oversized_rows_before_polling_the_whole_source() {
    let (kernel, polled) = input(vec![Ok(Bytes::from_static(b"xxxxxxxx"))], 100_000);
    let answer = block_on(kernel.issue_body_with_context(
        csv_request(false),
        &Capability::root(),
        IssueContext::new().with_memory_limit(32),
    ))
    .unwrap();
    assert!(matches!(
        block_on(answer.bytes.collect(1024)),
        Err(Error::OverBudget { .. })
    ));
    assert!(polled.load(Ordering::SeqCst) <= 5);
}

#[test]
fn csv_partial_failures_end_in_an_error_without_a_closing_bracket() {
    for chunks in [
        vec![
            Ok(Bytes::from_static(b"a,b\n1,2\n")),
            Ok(Bytes::from_static(b"3\n")),
        ],
        vec![
            Ok(Bytes::from_static(b"a,b\n1,2\n")),
            Err(Error::Endpoint("source failed".into())),
        ],
        vec![
            Ok(Bytes::from_static(b"a,b\n1,2\n")),
            Ok(Bytes::from_static(b"\xff,4\n")),
        ],
    ] {
        let (kernel, _) = input(chunks, 1);
        let answer = block_on(kernel.issue_body(csv_request(true), &Capability::root())).unwrap();
        let mut body = answer.bytes.into_stream();
        assert_eq!(block_on(body.next()).unwrap().unwrap(), "[");
        assert_eq!(
            block_on(body.next()).unwrap().unwrap(),
            "{\"a\":\"1\",\"b\":\"2\"}"
        );
        assert!(block_on(body.next()).unwrap().is_err());
        assert!(block_on(body.next()).is_none());
    }
}

#[test]
fn ndjson_body_matches_the_original_invoke() {
    let host = Host::new();
    for q in ["empty", ".[]", "range(1000)", "\"a\\nb\"", "."] {
        let request = host.request(
            "urn:json:query",
            &[("src", PEOPLE_IRI), ("q", q), ("as", ikigai_json::NDJSON)],
        );
        let bindings = Bindings::new();
        let cap = Capability::root();
        let inv = Invocation::with_issuer(&request, &bindings, &cap, &host.kernel);
        let bytes = block_on(ikigai_json::query().invoke(&inv)).unwrap();
        let body = block_on(host.kernel.issue_body(request, &Capability::root())).unwrap();
        assert_eq!(
            block_on(body.bytes.collect(1024 * 1024)).unwrap().as_ref(),
            bytes.bytes
        );
    }
}

#[test]
fn ndjson_runtime_error_arrives_after_the_first_result() {
    let host = Host::new();
    let request = host.request(
        "urn:json:query",
        &[
            ("src", PEOPLE_IRI),
            ("q", "0, error(\"late\")"),
            ("as", ikigai_json::NDJSON),
        ],
    );
    let answer = block_on(host.kernel.issue_body(request, &Capability::root())).unwrap();
    let mut body = answer.bytes.into_stream();
    assert_eq!(block_on(body.next()).unwrap().unwrap(), "0\n");
    assert!(matches!(
        block_on(body.next()).unwrap(),
        Err(Error::InvalidArgument { .. })
    ));
    assert!(block_on(body.next()).is_none());
}

#[test]
fn ndjson_refuses_a_value_larger_than_the_run_budget() {
    let host = Host::new();
    let request = host.request(
        "urn:json:query",
        &[
            ("src", PEOPLE_IRI),
            ("q", "0, (\"x\" * 1000)"),
            ("as", ikigai_json::NDJSON),
        ],
    );
    let answer = block_on(host.kernel.issue_body_with_context(
        request,
        &Capability::root(),
        IssueContext::new().with_memory_limit(128),
    ))
    .unwrap();
    let mut body = answer.bytes.into_stream();
    assert_eq!(block_on(body.next()).unwrap().unwrap(), "0\n");
    assert!(matches!(
        block_on(body.next()).unwrap(),
        Err(Error::OverBudget { .. })
    ));
    assert!(block_on(body.next()).is_none());
}

#[test]
fn bytes_invoke_keeps_the_existing_csv_refusal() {
    let request = Request::new(Verb::Source, Iri::parse("urn:json:from-csv").unwrap())
        .with_arg("content", ArgRef::Inline(b"a,b\n1\n".to_vec()));
    let bindings = Bindings::new();
    let cap = Capability::root();
    let inv = Invocation::detached(&request, &bindings, &cap);
    assert!(matches!(
        block_on(ikigai_json::from_csv().invoke(&inv)),
        Err(Error::InvalidArgument { .. })
    ));
}

#[test]
fn jq_results_larger_than_the_pool_are_streamed_as_small_values() {
    let host = Host::new();
    let request = host.request(
        "urn:json:query",
        &[
            ("src", PEOPLE_IRI),
            ("q", "range(10000)"),
            ("as", ikigai_json::NDJSON),
        ],
    );
    let answer = block_on(host.kernel.issue_body_with_context(
        request,
        &Capability::root(),
        IssueContext::new().with_memory_limit(128),
    ))
    .unwrap();
    let mut body = answer.bytes.into_stream();
    let mut seen = 0;
    while let Some(chunk) = block_on(body.next()) {
        assert_eq!(chunk.unwrap(), format!("{seen}\n"));
        seen += 1;
    }
    assert_eq!(seen, 10000);
}

#[test]
fn ndjson_csv_source_is_transrepted_without_an_unbounded_intermediate() {
    let (kernel, polled) = input(vec![Ok(Bytes::from_static(b"a,b\n"))], 100_000);
    let request = Request::new(Verb::Source, Iri::parse("urn:json:query").unwrap())
        .with_arg("src", ArgRef::Reference(Iri::parse("urn:input").unwrap()))
        .with_arg("q", ArgRef::Inline(b".[]".to_vec()))
        .with_arg(
            "as",
            ArgRef::Inline(ikigai_json::NDJSON.as_bytes().to_vec()),
        );
    let answer = block_on(kernel.issue_body_with_context(
        request,
        &Capability::root(),
        IssueContext::new().with_memory_limit(128),
    ));
    assert!(matches!(answer, Err(Error::OverBudget { .. })));
    assert!(polled.load(Ordering::SeqCst) < 20);
}

#[test]
fn csv_total_input_bound_is_an_error_after_partial_output() {
    let (kernel, polled) = input(vec![Ok(Bytes::from(vec![b'\n'; 4096]))], 4097);
    let answer = block_on(kernel.issue_body(csv_request(false), &Capability::root())).unwrap();
    assert!(matches!(
        block_on(answer.bytes.collect(1024)),
        Err(Error::OverBudget { .. })
    ));
    assert_eq!(polled.load(Ordering::SeqCst), 4097);
}

#[test]
fn csv_escaping_cannot_expand_a_row_past_the_budget() {
    let (kernel, _) = input(vec![Ok(Bytes::from_static(b"\x00\x00\x00\x00\n"))], 1);
    let answer = block_on(kernel.issue_body_with_context(
        csv_request(false),
        &Capability::root(),
        IssueContext::new().with_memory_limit(16),
    ))
    .unwrap();
    assert!(matches!(
        block_on(answer.bytes.collect(1024)),
        Err(Error::OverBudget { .. })
    ));
}
