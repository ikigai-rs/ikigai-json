//! Body adapters. CSV uses the csv crate's incremental parser (csv-core), with
//! caller-owned, bounded field storage; jq keeps its evaluation stack on a worker.

use async_trait::async_trait;
use futures::{stream, StreamExt};
use ikigai_core::{
    AsyncFnEndpoint, Body, Bytes, Description, Endpoint, Error, FailureClass, FnEndpoint,
    Invocation, MemoryBudget, ReprType, Representation, Result,
};
use jaq_core::ValT as _;
use jaq_json::Val;

use crate::{csv_json::*, invalid, sandbox, wanted, JSON, NDJSON};

fn budget(inv: &Invocation<'_>) -> MemoryBudget {
    inv.memory_budget().cloned().unwrap_or_else(|| {
        ikigai_core::IssueContext::new()
            .with_memory_limit(ikigai_core::DEFAULT_STREAM_MEMORY)
            .memory_budget()
            .expect("configured pool")
            .clone()
    })
}

fn over(detail: impl Into<String>) -> Error {
    Error::OverBudget {
        detail: detail.into(),
    }
}

fn jq_refusal(error: sandbox::Refusal) -> Error {
    match error {
        sandbox::Refusal::Bound { .. } => over(error.to_string()),
        other => crate::refusal(other),
    }
}

pub(crate) struct Query(pub AsyncFnEndpoint);

#[async_trait]
impl Endpoint for Query {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.0.invoke(inv).await
    }

    async fn invoke_body(&self, inv: &Invocation<'_>) -> Result<Representation<Body>> {
        if wanted(inv, &[JSON, NDJSON, crate::TEXT])? != NDJSON {
            return self.0.invoke_body(inv).await;
        }
        let program = inv.inline_str("q")?.to_owned();
        let src = crate::src_iri(inv)?;
        // jq consumes a JSON document whole; it is the RESULT stream that is incremental.
        let pool = budget(inv);
        let source = inv.source_body(&src).await?;
        let source = if crate::is_json(&source.repr_type.media_type) {
            source
        } else {
            let chain = inv
                .select_transreptor(&source.repr_type.media_type, JSON)
                .ok_or_else(|| invalid("src", "no transreptor converts this source to JSON"))?;
            inv.transrept(source, &chain).await?
        };
        let limit = usize::try_from(pool.remaining()).unwrap_or(usize::MAX);
        let input = match source.bytes {
            Body::Bytes(bytes) => {
                Body::Bytes(bytes)
                    .collect(limit.min(sandbox::MAX_INPUT_BYTES))
                    .await?
            }
            streamed => {
                // Bound the input independently of a possibly larger run pool, then
                // let core account for every chunk retained by the collection.
                let bounded = streamed.into_stream().scan(0usize, |seen, item| {
                    let item = item.and_then(|chunk| {
                        *seen = seen.saturating_add(chunk.len());
                        if *seen > sandbox::MAX_INPUT_BYTES {
                            Err(over("the jq input exceeds MAX_INPUT_BYTES"))
                        } else {
                            Ok(chunk)
                        }
                    });
                    futures::future::ready(Some(item))
                });
                Body::once(bounded).collect_within(&pool).await?
            }
        };
        let limit = usize::try_from(pool.remaining()).unwrap_or(usize::MAX);
        let body = jq_body(program, input, limit)?;
        Ok(Representation::from_body(ReprType::new(NDJSON), body).cacheable())
    }

    fn name(&self) -> &str {
        self.0.name()
    }
    fn describe(&self) -> Description {
        // A source or referenced content can propagate any failure class.
        FailureClass::ALL
            .into_iter()
            .fold(self.0.describe(), Description::fails)
    }
}

#[cfg(not(target_family = "wasm"))]
fn jq_body(program: String, input: Bytes, limit: usize) -> Result<Body> {
    use futures::{channel::mpsc, SinkExt};
    let (mut sender, receiver) = mpsc::channel(0);
    // A zero-capacity channel admits one in-flight result. Dropping the consumer
    // disconnects the send and stops jaq before the next result is evaluated.
    std::thread::Builder::new()
        .name("json-ndjson".into())
        .spawn(move || {
            let limits = sandbox::Limits {
                max_output_bytes: sandbox::MAX_OUTPUT_BYTES,
                max_string_bytes: sandbox::MAX_STRING_BYTES.min(limit),
                ..sandbox::Limits::default()
            };
            let mut used = 0usize;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sandbox::run_each(
                    &program,
                    &input,
                    &limits,
                    limit.saturating_sub(1),
                    |output| {
                        let mut line = output.json;
                        line.push('\n');
                        used = used.saturating_add(line.len());
                        let item = if line.len() > limit || used > sandbox::MAX_OUTPUT_BYTES {
                            Err(over("the NDJSON output exceeds the byte budget"))
                        } else {
                            Ok(Bytes::from(line))
                        };
                        let failed = item.is_err();
                        futures::executor::block_on(sender.send(item)).is_ok() && !failed
                    },
                )
            }));
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => jq_refusal(error),
                Err(_) => Error::Endpoint("the NDJSON worker panicked".into()),
            };
            let _ = futures::executor::block_on(sender.send(Err(error)));
        })
        .map_err(|e| Error::Endpoint(format!("starting NDJSON worker: {e}")))?;
    Ok(Body::once(receiver))
}

#[cfg(target_family = "wasm")]
fn jq_body(program: String, input: Bytes, limit: usize) -> Result<Body> {
    // wasm has no worker threads. Keep the existing sandbox's bounded evaluation;
    // CSV remains incremental on this target as well.
    let limits = sandbox::Limits {
        max_output_bytes: sandbox::MAX_OUTPUT_BYTES.min(limit),
        ..sandbox::Limits::default()
    };
    let outputs = sandbox::run(&program, &input, &limits).map_err(jq_refusal)?;
    let answer = crate::answer(outputs, NDJSON)?;
    if answer.bytes.len() > limit {
        return Err(over("the NDJSON output exceeds the byte budget"));
    }
    Ok(Body::once(stream::iter([Ok(Bytes::from(answer.bytes))])))
}

pub(crate) struct FromCsv(pub FnEndpoint);

#[async_trait]
impl Endpoint for FromCsv {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.0.invoke(inv).await
    }

    async fn invoke_body(&self, inv: &Invocation<'_>) -> Result<Representation<Body>> {
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
        let input = inv.content_body("content").await?;
        let pool = budget(inv);
        if pool.remaining() < 2 {
            return Err(over("the byte budget cannot hold JSON array framing"));
        }
        let body = match input {
            Body::Once(input) => csv_stream(Body::Once(input), header, pool),
            Body::Bytes(bytes) => Body::replay(CsvReplay {
                input: ReplayInput::Bytes(bytes),
                header,
                pool,
            }),
            Body::Replay(replay) => Body::replay(CsvReplay {
                input: ReplayInput::Replay(replay),
                header,
                pool,
            }),
        };
        Ok(Representation::from_body(ReprType::new(JSON), body).cacheable())
    }

    fn name(&self) -> &str {
        self.0.name()
    }
    fn describe(&self) -> Description {
        FailureClass::ALL
            .into_iter()
            .fold(self.0.describe().incremental(), Description::fails)
    }
}

fn csv_stream(input: Body, header: bool, pool: MemoryBudget) -> Body {
    let state = CsvState::new(input, header, pool);
    Body::once(stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        match state.next().await {
            Ok(Some(chunk)) => Some((Ok(chunk), Some(state))),
            Ok(None) => None,
            Err(error) => Some((Err(error), None)),
        }
    }))
}

enum ReplayInput {
    Bytes(Bytes),
    Replay(std::sync::Arc<dyn ikigai_core::Replay>),
}

struct CsvReplay {
    input: ReplayInput,
    header: bool,
    pool: MemoryBudget,
}

impl ikigai_core::Replay for CsvReplay {
    fn open(&self) -> ikigai_core::ByteStream {
        let input = match &self.input {
            ReplayInput::Bytes(bytes) => Body::Bytes(bytes.clone()),
            ReplayInput::Replay(replay) => Body::Replay(replay.clone()),
        };
        csv_stream(input, self.header, self.pool.clone()).into_stream()
    }
}

struct CsvState {
    input: ikigai_core::ByteStream,
    chunk: Bytes,
    eof: bool,
    parser: csv_core::Reader,
    field: Vec<u8>,
    row: Vec<String>,
    names: Option<Vec<String>>,
    header: bool,
    width: Option<usize>,
    rows: usize,
    records: usize,
    input_bytes: usize,
    row_bytes: usize,
    pool: MemoryBudget,
    started: bool,
    ended: bool,
    first: bool,
    prefix: Vec<u8>,
    pending: Bytes,
}

impl CsvState {
    fn new(input: Body, header: bool, pool: MemoryBudget) -> Self {
        Self {
            input: input.into_stream(),
            chunk: Bytes::new(),
            eof: false,
            parser: csv_core::Reader::new(),
            field: Vec::new(),
            row: Vec::new(),
            names: None,
            header,
            width: None,
            rows: 0,
            records: 0,
            input_bytes: 0,
            row_bytes: 0,
            pool,
            started: false,
            ended: false,
            first: true,
            prefix: Vec::new(),
            pending: Bytes::new(),
        }
    }

    fn check(&self, bytes: usize) -> Result<()> {
        let names = self
            .names
            .as_ref()
            .map_or(0, |n| n.iter().map(|name| name.len() + 1).sum());
        if bytes.saturating_add(names) as u64 > self.pool.remaining() {
            Err(over("a CSV row and header exceed the run's byte budget"))
        } else {
            Ok(())
        }
    }

    async fn fill(&mut self) -> Result<()> {
        while self.chunk.is_empty() && !self.eof {
            match self.input.next().await {
                Some(Ok(chunk)) => {
                    self.input_bytes = self.input_bytes.saturating_add(chunk.len());
                    if self.input_bytes > MAX_CSV_BYTES {
                        return Err(over("CSV exceeds MAX_CSV_BYTES"));
                    }
                    self.chunk = chunk;
                }
                Some(Err(e)) => return Err(e),
                None => self.eof = true,
            }
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<Bytes>> {
        if !self.started {
            self.started = true;
            return Ok(Some(Bytes::from_static(b"[")));
        }
        if self.ended {
            return Ok(None);
        }
        // csv-core strips a BOM only when all three bytes arrive together.
        // Coalesce just the first three bytes, even across empty input chunks.
        if self.first {
            while self.prefix.len() < 3 {
                self.fill().await?;
                if self.chunk.is_empty() {
                    break;
                }
                let n = (3 - self.prefix.len()).min(self.chunk.len());
                self.prefix.extend_from_slice(&self.chunk.split_to(n));
            }
            self.pending = std::mem::take(&mut self.chunk);
            self.chunk = Bytes::from(std::mem::take(&mut self.prefix));
            self.first = false;
            if self.chunk.as_ref() == b"\xEF\xBB\xBF" {
                self.chunk = std::mem::take(&mut self.pending);
            }
        }
        loop {
            if self.chunk.is_empty() && !self.pending.is_empty() {
                self.chunk = std::mem::take(&mut self.pending);
            }
            self.fill().await?;
            let mut output = [0u8; 1024];
            let (status, consumed, written) = self.parser.read_field(&self.chunk, &mut output);
            self.chunk = self.chunk.slice(consumed..);
            self.check(
                self.row_bytes
                    .saturating_add(self.field.len())
                    .saturating_add(written),
            )?;
            if self.field.len() + written > MAX_CSV_FIELD_BYTES {
                return Err(over("CSV exceeds MAX_CSV_FIELD_BYTES"));
            }
            self.field.extend_from_slice(&output[..written]);
            match status {
                csv_core::ReadFieldResult::InputEmpty | csv_core::ReadFieldResult::OutputFull => {
                    continue
                }
                csv_core::ReadFieldResult::End => {
                    self.ended = true;
                    return Ok(Some(Bytes::from_static(b"]")));
                }
                csv_core::ReadFieldResult::Field { record_end } => {
                    let field = String::from_utf8(std::mem::take(&mut self.field))
                        .map_err(|_| invalid("content", "CSV record is not UTF-8"))?;
                    self.row_bytes += field.len() + 1;
                    self.row.push(field);
                    if self.row.len() > MAX_CSV_COLUMNS {
                        return Err(over("CSV exceeds MAX_CSV_COLUMNS"));
                    }
                    if !record_end {
                        continue;
                    }
                    self.records += 1;
                    let row = std::mem::take(&mut self.row);
                    self.row_bytes = 0;
                    if self.width.is_some_and(|w| w != row.len()) {
                        return Err(invalid(
                            "content",
                            format!("record {} has {} fields where the first has {}; RFC 4180 records all have the same number", self.records, row.len(), self.width.expect("known width")),
                        ));
                    }
                    self.width = Some(row.len());
                    if self.header && self.names.is_none() {
                        for (i, name) in row.iter().enumerate() {
                            if row[..i].contains(name) {
                                return Err(invalid("content", "the header names a column twice"));
                            }
                        }
                        self.names = Some(row);
                        continue;
                    }
                    if self.rows == MAX_CSV_ROWS {
                        return Err(over("CSV exceeds MAX_CSV_ROWS"));
                    }
                    let fields = row.into_iter().map(Val::utf8_str);
                    let value = match &self.names {
                        Some(names) => {
                            Val::from_map(names.iter().cloned().map(Val::utf8_str).zip(fields))
                                .expect("string keys")
                        }
                        None => fields.collect(),
                    };
                    let available = usize::try_from(self.pool.remaining()).unwrap_or(usize::MAX);
                    let mut json = sandbox::serialize(&value, available.saturating_sub(1))
                        .ok_or_else(|| {
                            over("a serialized CSV row exceeds the run's byte budget")
                        })?;
                    if self.rows > 0 {
                        json.insert(0, ',');
                    }
                    self.check(json.len())?;
                    self.rows += 1;
                    return Ok(Some(Bytes::from(json)));
                }
            }
        }
    }
}
