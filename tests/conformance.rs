//! The module recipe as one test: `ikigai-conformance` walks every endpoint the test host binds
//! (this module's three, and the files beside them) and reports every violation at once.
//!
//! - `json-from-csv` and `json-to-csv` are `pure` and `cacheable`: functions of their inline
//!   `content`, reading nothing else.
//! - `json-query` is `cacheable` over the threaded file it reads: its answer takes its expiry
//!   and golden thread from `src` (`tests/endpoints.rs` cuts it). It is not pure: it reads
//!   `src`.
//! - `space()` is self-named `urn:iki:space:json`: it takes no parameters and reads nothing
//!   while it is built.

mod common;

use common::{Host, PEOPLE_IRI};
use ikigai_conformance::{Check, Fixture, Suite};
use ikigai_core::Verb;

const ENDPOINTS: [&str; 3] = ["json-query", "json-from-csv", "json-to-csv"];

const WATCHER_CUT: &str = "test stand-in for an ikigai-fs file under a watcher: the watcher's \
     cut is the test's kernel.cut, which the_answer_is_golden_threaded_from_src exercises";

#[test]
fn conforms() {
    let host = Host::new();
    let report = Suite::new()
        .fixture(
            Fixture::new("json-query", Verb::Source)
                .arg("src", PEOPLE_IRI)
                .arg("q", ".[0].name"),
        )
        .fixture(Fixture::new("json-from-csv", Verb::Source).arg("content", "a,b\n1,2\n"))
        .fixture(Fixture::new("json-to-csv", Verb::Source).arg("content", r#"[{"a":1}]"#))
        .cacheable("json-query")
        .opt_out_check(
            "json-from-csv", Check::Cacheable,
            "incremental JSON has unknown length; core's bounded cache declines it without collecting",
        )
        .pure("json-to-csv")
        .cacheable("json-to-csv")
        // The host's files: stand-ins for ikigai-fs, so held to its standard there.
        .opt_out_check("test-people", Check::Cacheable, WATCHER_CUT)
        .opt_out_check("test-csv", Check::Cacheable, WATCHER_CUT)
        .opt_out_check("test-html", Check::Cacheable, WATCHER_CUT)
        .pure("test-secret")
        .self_named_space("json", ikigai_json::space)
        .run_blocking(&host.kernel);
    println!("{report}");
    assert!(report.is_clean(), "{report}");
    assert_eq!(
        ikigai_core::space_iri("json").as_str(),
        ikigai_json::SPACE_ID
    );
    for id in ENDPOINTS {
        assert!(
            report.to_string().contains(id),
            "the walk saw `{id}`: {report}"
        );
    }
}
