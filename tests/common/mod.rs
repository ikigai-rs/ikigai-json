//! A test host: this module's space plus files to query, behind IRIs, the way a host mounts
//! `ikigai-fs` beside it.

#![allow(dead_code)] // each test file uses part of this

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Description, Error, Exact, FnEndpoint, Invocation, Iri, Kernel, ReprType,
    Representation, Request, Verb,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

pub const PEOPLE_IRI: &str = "urn:file:people.json";
pub const CSV_IRI: &str = "urn:file:people.csv";
pub const LIVE_IRI: &str = "urn:file:live.json";
pub const HTML_IRI: &str = "urn:file:page.html";
pub const SECRET_IRI: &str = "urn:file:secret.json";
pub const SECRET_SCOPE: &str = "urn:cap:test:secret";

pub const PEOPLE: &str = r#"[{"name":"Ada","age":36},{"name":"Grace","age":85}]"#;
pub const PEOPLE_CSV: &str = "name,age\r\nAda,36\r\nGrace,85\r\n";

pub struct Host {
    pub kernel: Kernel,
    /// The JSON behind [`PEOPLE_IRI`], editable in place.
    pub people: Arc<RwLock<String>>,
    /// How many times [`PEOPLE_IRI`] has been read.
    pub reads: Arc<AtomicUsize>,
}

/// A file resource: bytes behind an IRI. `threaded` names its own IRI as the golden thread
/// (cut from outside, as a watcher cuts it); otherwise it is live and uncacheable.
fn file(
    id: &'static str,
    media: &'static str,
    body: Arc<RwLock<String>>,
    threaded: bool,
    reads: Arc<AtomicUsize>,
) -> FnEndpoint {
    FnEndpoint::new(id, move |inv: &Invocation<'_>| {
        reads.fetch_add(1, Ordering::SeqCst);
        let bytes = body.read().expect("lock").as_bytes().to_vec();
        let repr = Representation::new(ReprType::new(media), bytes);
        Ok(if threaded {
            repr.cacheable().depends_on(inv.request.target.as_str())
        } else {
            repr
        })
    })
    .with_description(
        Description::new(id)
            .title("Test file")
            .summary("A file served as a kernel resource for the tests.")
            .verb(Verb::Source)
            .output(media),
    )
}

fn secret() -> FnEndpoint {
    FnEndpoint::new("test-secret", |inv: &Invocation<'_>| {
        if !inv.capability.allows(SECRET_SCOPE) {
            return Err(Error::Denied(format!("reading this needs {SECRET_SCOPE}")));
        }
        Ok(Representation::new(
            ReprType::new("application/json"),
            br#"{"pin":1234}"#.to_vec(),
        )
        .cacheable())
    })
    .with_description(
        Description::new("test-secret")
            .title("Test secret")
            .summary("A JSON resource only a holder of the test scope may read.")
            .verb(Verb::Source)
            .requires(SECRET_SCOPE)
            .output("application/json"),
    )
}

impl Host {
    pub fn new() -> Self {
        let people = Arc::new(RwLock::new(PEOPLE.to_string()));
        let reads = Arc::new(AtomicUsize::new(0));
        let none = || Arc::new(AtomicUsize::new(0));
        let text = |s: &str| Arc::new(RwLock::new(s.to_string()));
        let space = ikigai_json::space()
            .bind(
                Exact::new(PEOPLE_IRI),
                file(
                    "test-people",
                    "application/json",
                    Arc::clone(&people),
                    true,
                    Arc::clone(&reads),
                ),
            )
            .bind(
                Exact::new(CSV_IRI),
                file("test-csv", "text/csv", text(PEOPLE_CSV), true, none()),
            )
            .bind(
                Exact::new(LIVE_IRI),
                file("test-live", "application/json", text(PEOPLE), false, none()),
            )
            .bind(
                Exact::new(HTML_IRI),
                file("test-html", "text/html", text("<p>hi</p>"), true, none()),
            )
            .bind(Exact::new(SECRET_IRI), secret());
        Host {
            kernel: Kernel::new(Arc::new(space)),
            people,
            reads,
        }
    }

    pub fn request(&self, target: &str, args: &[(&str, &str)]) -> Request {
        let mut request = Request::new(Verb::Source, Iri::parse(target).unwrap());
        for &(name, value) in args {
            request = request.with_arg(name, ArgRef::Inline(value.as_bytes().to_vec()));
        }
        request
    }

    pub fn issue_as(
        &self,
        target: &str,
        args: &[(&str, &str)],
        cap: &Capability,
    ) -> ikigai_core::Result<Representation> {
        block_on(self.kernel.issue(self.request(target, args), cap))
    }

    pub fn issue(
        &self,
        target: &str,
        args: &[(&str, &str)],
    ) -> ikigai_core::Result<Representation> {
        self.issue_as(target, args, &Capability::root())
    }

    /// A query's answer as text, under root.
    pub fn query(&self, src: &str, q: &str) -> ikigai_core::Result<String> {
        self.issue("urn:json:query", &[("src", src), ("q", q)])
            .map(|r| String::from_utf8(r.bytes).unwrap())
    }
}
