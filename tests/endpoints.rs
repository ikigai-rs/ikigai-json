//! The three resources through a kernel: what they answer, how results merge, how `src` is read
//! (under the caller's capability, through transreptors, golden-threaded), and how refusals are
//! typed.

mod common;

use common::*;
use ikigai_core::{Capability, Error};

fn invalid_arg(r: ikigai_core::Result<impl std::fmt::Debug>) -> (String, String) {
    match r {
        Err(Error::InvalidArgument { name, detail }) => (name, detail),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

// ---- answers --------------------------------------------------------------------------

#[test]
fn one_result_is_that_value_and_several_are_an_array() {
    let host = Host::new();
    assert_eq!(
        host.query(PEOPLE_IRI, ".[0]").unwrap(),
        r#"{"name":"Ada","age":36}"#
    );
    assert_eq!(host.query(PEOPLE_IRI, ".[0].name").unwrap(), r#""Ada""#);
    assert_eq!(
        host.query(PEOPLE_IRI, ".[].name").unwrap(),
        r#"["Ada","Grace"]"#
    );
    assert_eq!(host.query(PEOPLE_IRI, "empty").unwrap(), "[]");
    // A program that collects itself answers its own array, whatever its length.
    assert_eq!(
        host.query(PEOPLE_IRI, "[.[] | select(.age > 50) | .name]")
            .unwrap(),
        r#"["Grace"]"#
    );
    // ...where the bare stream of one is the one value.
    assert_eq!(
        host.query(PEOPLE_IRI, ".[] | select(.age > 50) | .name")
            .unwrap(),
        r#""Grace""#
    );
    let repr = host
        .issue("urn:json:query", &[("src", PEOPLE_IRI), ("q", ".")])
        .unwrap();
    assert_eq!(repr.repr_type.media_type, "application/json");
}

#[test]
fn ndjson_is_one_result_per_line() {
    let host = Host::new();
    let repr = host
        .issue(
            "urn:json:query",
            &[
                ("src", PEOPLE_IRI),
                ("q", ".[].name"),
                ("as", "application/x-ndjson"),
            ],
        )
        .unwrap();
    assert_eq!(repr.repr_type.media_type, "application/x-ndjson");
    assert_eq!(repr.bytes, b"\"Ada\"\n\"Grace\"\n");
}

/// ikigai-fn's `$j{<src> <program>}` desugars to `urn:json:query` with `as=text/plain` and
/// splices the answer into a page: a string must come back as its bare characters (`Ada`, never
/// `"Ada"`), and anything that is not exactly one scalar must be refused, not stringified.
#[test]
fn dollar_j_splices_a_single_scalar_as_bare_text() {
    let host = Host::new();
    let plain = |q: &str| {
        host.issue(
            "urn:json:query",
            &[("src", PEOPLE_IRI), ("q", q), ("as", "text/plain")],
        )
    };
    let ada = plain(".[0].name").unwrap();
    assert_eq!(ada.bytes, b"Ada", "a string splices bare, no JSON quotes");
    assert_eq!(ada.repr_type.media_type, "text/plain");
    assert_eq!(plain(".[0].age").unwrap().bytes, b"36");
    assert_eq!(plain(".[0].missing").unwrap().bytes, b"null");
    assert_eq!(plain(".[0].age > 1").unwrap().bytes, b"true");
    assert_eq!(
        plain("\"<b>&\"").unwrap().bytes,
        b"<b>&",
        "no escaping either: the splicer escapes"
    );

    for (q, why) in [
        (".[0]", "an object"),
        (".", "an array"),
        (".[].name", "two results"),
        ("empty", "no result"),
    ] {
        let (name, detail) = invalid_arg(plain(q));
        assert_eq!(name, "as", "{why}: {detail}");
        assert!(detail.contains("text/plain"), "{why}: {detail}");
    }
}

// ---- reading src -------------------------------------------------------------------------

#[test]
fn a_csv_src_is_read_through_the_transreptor() {
    let host = Host::new();
    assert_eq!(
        host.query(CSV_IRI, "[.[] | select(.age | tonumber > 50) | .name]")
            .unwrap(),
        r#"["Grace"]"#
    );
    // CSV values are strings: CSV has no types, and nothing guesses.
    assert_eq!(host.query(CSV_IRI, ".[0].age").unwrap(), r#""36""#);
}

#[test]
fn a_src_nothing_converts_is_refused_naming_src() {
    let host = Host::new();
    let (name, detail) = invalid_arg(host.query(HTML_IRI, "."));
    assert_eq!(name, "src");
    assert!(detail.contains("text/html"), "{detail}");
}

#[test]
fn src_is_read_under_the_callers_capability() {
    let host = Host::new();
    let q = [("src", SECRET_IRI), ("q", ".pin")];
    assert!(matches!(
        host.issue_as(
            "urn:json:query",
            &q,
            &Capability::scoped(Vec::<String>::new())
        ),
        Err(Error::Denied(_))
    ));
    let holder = Capability::scoped([SECRET_SCOPE]);
    assert_eq!(
        host.issue_as("urn:json:query", &q, &holder).unwrap().bytes,
        b"1234"
    );
}

#[test]
fn the_answer_is_golden_threaded_from_src() {
    let host = Host::new();
    let request = || host.request("urn:json:query", &[("src", PEOPLE_IRI), ("q", ".[0].name")]);
    assert_eq!(host.query(PEOPLE_IRI, ".[0].name").unwrap(), r#""Ada""#);
    assert!(
        host.kernel.is_cached(&request(), &Capability::root()),
        "cached while src is"
    );
    let reads = host.reads.load(std::sync::atomic::Ordering::SeqCst);

    *host.people.write().unwrap() = r#"[{"name":"Edsger"}]"#.to_string();
    assert_eq!(
        host.query(PEOPLE_IRI, ".[0].name").unwrap(),
        r#""Ada""#,
        "no cut, no change"
    );
    assert_eq!(
        host.reads.load(std::sync::atomic::Ordering::SeqCst),
        reads,
        "served from cache"
    );

    host.kernel.cut(PEOPLE_IRI);
    assert_eq!(
        host.query(PEOPLE_IRI, ".[0].name").unwrap(),
        r#""Edsger""#,
        "recomputed after the cut"
    );
}

#[test]
fn over_a_live_src_nothing_is_cached() {
    let host = Host::new();
    host.query(LIVE_IRI, ".").unwrap();
    let request = host.request("urn:json:query", &[("src", LIVE_IRI), ("q", ".")]);
    assert!(!host.kernel.is_cached(&request, &Capability::root()));
}

// ---- refusals ---------------------------------------------------------------------------

#[test]
fn refusals_are_typed_and_name_the_argument() {
    let host = Host::new();
    let (name, detail) = invalid_arg(host.query(PEOPLE_IRI, ".[ | "));
    assert_eq!(name, "q", "{detail}");
    let (name, detail) = invalid_arg(host.query(PEOPLE_IRI, "env"));
    assert_eq!(name, "q");
    assert!(detail.contains("environment"), "{detail}");
    let (name, _) = invalid_arg(host.query(PEOPLE_IRI, ".[0].name.x"));
    assert_eq!(name, "q", "a jq runtime error names the program");
    assert!(matches!(
        host.query(PEOPLE_IRI, "last(range(1e18))"),
        Err(Error::OverBudget { .. })
    ));
    let (name, detail) = invalid_arg(host.query(PEOPLE_IRI, "reduce range(1e6) as $i (null; [.])"));
    assert_eq!(name, "q");
    assert!(detail.contains("depth"), "{detail}");
    let (name, _) = invalid_arg(host.issue(
        "urn:json:query",
        &[("src", PEOPLE_IRI), ("q", "."), ("as", "text/html")],
    ));
    assert_eq!(name, "as");
    assert!(matches!(
        host.issue("urn:json:query", &[("q", ".")]),
        Err(Error::MissingArgument(_))
    ));
    assert!(matches!(
        host.issue("urn:json:query", &[("src", PEOPLE_IRI)]),
        Err(Error::MissingArgument(_))
    ));
    let (name, _) = invalid_arg(host.query("not an iri", "."));
    assert_eq!(name, "src");
}

// ---- CSV ----------------------------------------------------------------------------------

#[test]
fn csv_endpoints_round_trip_and_refuse_what_they_cannot_hold() {
    let host = Host::new();
    let json = host
        .issue("urn:json:from-csv", &[("content", PEOPLE_CSV)])
        .unwrap();
    assert_eq!(
        json.bytes,
        br#"[{"name":"Ada","age":"36"},{"name":"Grace","age":"85"}]"#
    );
    let rows = host
        .issue(
            "urn:json:from-csv",
            &[("content", PEOPLE_CSV), ("header", "false")],
        )
        .unwrap();
    assert_eq!(
        rows.bytes,
        br#"[["name","age"],["Ada","36"],["Grace","85"]]"#
    );
    let csv = host
        .issue(
            "urn:json:to-csv",
            &[("content", std::str::from_utf8(&json.bytes).unwrap())],
        )
        .unwrap();
    assert_eq!(csv.repr_type.media_type, "text/csv");
    assert_eq!(
        csv.bytes,
        PEOPLE_CSV.as_bytes(),
        "its own output round-trips"
    );

    let (name, detail) = invalid_arg(host.issue("urn:json:from-csv", &[("content", "a,b\n1\n")]));
    assert_eq!(name, "content");
    assert!(detail.contains("record 2"), "{detail}");
    let (_, detail) = invalid_arg(host.issue("urn:json:from-csv", &[("content", "a,a\n1,2\n")]));
    assert!(detail.contains("twice"), "{detail}");
    let (_, detail) = invalid_arg(host.issue("urn:json:to-csv", &[("content", r#"[{"a":[1]}]"#)]));
    assert!(detail.contains("nest"), "{detail}");
    let (name, _) = invalid_arg(host.issue(
        "urn:json:from-csv",
        &[("content", "a\n"), ("header", "maybe")],
    ));
    assert_eq!(name, "header");
}

#[test]
fn csv_edge_cases() {
    use ikigai_json::{csv_to_json, json_to_csv};
    assert_eq!(csv_to_json(b"", true).unwrap(), "[]");
    assert_eq!(
        csv_to_json(b"a\n", true).unwrap(),
        "[]",
        "a header and no records"
    );
    assert_eq!(
        csv_to_json(b"\xEF\xBB\xBFa\n1\n", true).unwrap(),
        r#"[{"a":"1"}]"#,
        "BOM skipped"
    );
    assert_eq!(
        csv_to_json(b"q\n\"he said \"\"hi\"\"\nthen left\"\n", true).unwrap(),
        r#"[{"q":"he said \"hi\"\nthen left"}]"#
    );
    assert!(csv_to_json(b"a\n\xff\n", true)
        .unwrap_err()
        .detail
        .contains("UTF-8"));
    let wide = format!("{}\n", vec!["x"; 1025].join(","));
    assert!(csv_to_json(wide.as_bytes(), false)
        .unwrap_err()
        .detail
        .contains("MAX_CSV_COLUMNS"));
    let field = format!("a\n{}\n", "x".repeat(1024 * 1024 + 1));
    assert!(csv_to_json(field.as_bytes(), true)
        .unwrap_err()
        .detail
        .contains("MAX_CSV_FIELD_BYTES"));
    let rows = "a\n".to_string() + &"1\n".repeat(1_000_001);
    assert!(csv_to_json(rows.as_bytes(), true)
        .unwrap_err()
        .detail
        .contains("MAX_CSV_ROWS"));
    let big = vec![b'a'; 16 * 1024 * 1024 + 1];
    assert!(csv_to_json(&big, false)
        .unwrap_err()
        .detail
        .contains("MAX_CSV_BYTES"));

    assert_eq!(json_to_csv(b"[]").unwrap(), "");
    assert_eq!(
        json_to_csv(br#"[[1,"a,b"],[true,null]]"#).unwrap(),
        "1,\"a,b\"\r\ntrue,\r\n"
    );
    assert!(json_to_csv(b"{}").unwrap_err().detail.contains("array"));
    assert!(json_to_csv(br#"[{"a":1},[1]]"#)
        .unwrap_err()
        .detail
        .contains("record 1"));
}
