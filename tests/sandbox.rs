//! The jq sandbox: that programs mean what jq means, and that every bound holds.
//!
//! The hostile programs run in a CHILD PROCESS (this test binary re-executed with one probe
//! named in its environment), because the failure they guard against is a stack overflow or an
//! allocation failure, which aborts the process: in-process it would read as a crashed test
//! runner, in a child it reads as a failed test with the signal named. The child calls the
//! sandbox from a 2 MiB thread, the size of a tokio worker's.

use ikigai_json::sandbox::{run, Limits, Refusal, AVAILABLE, DROPPED_DEFS, EXCLUDED, WRAPPED};
use std::process::Command;

fn jq(program: &str, input: &str) -> Result<Vec<String>, Refusal> {
    run(program, input.as_bytes(), &Limits::default())
        .map(|outs| outs.into_iter().map(|o| o.json).collect())
}

fn one(program: &str, input: &str) -> String {
    let out = jq(program, input).unwrap_or_else(|e| panic!("`{program}`: {e}"));
    assert_eq!(out.len(), 1, "`{program}` yields one result: {out:?}");
    out.into_iter().next().unwrap()
}

// ---- jq means what jq means ---------------------------------------------------------------

const PEOPLE: &str = r#"{"people":[{"name":"Ada","age":36,"langs":["en","fr"]},{"name":"Grace","age":85,"langs":["en"]}]}"#;

#[test]
fn selection_from_collections_and_objects() {
    assert_eq!(one(".people[0].name", PEOPLE), r#""Ada""#);
    assert_eq!(
        jq(".people[] | select(.age > 40) | .name", PEOPLE).unwrap(),
        [r#""Grace""#]
    );
    assert_eq!(one("[.people[].name]", PEOPLE), r#"["Ada","Grace"]"#);
    assert_eq!(
        one(".people | map({name, n: (.langs | length)})", PEOPLE),
        r#"[{"name":"Ada","n":2},{"name":"Grace","n":1}]"#
    );
    assert_eq!(
        one(".people | sort_by(-.age) | .[0].name", PEOPLE),
        r#""Grace""#
    );
    assert_eq!(
        one(".people | group_by(.langs | length) | map(length)", PEOPLE),
        "[1,1]"
    );
    assert_eq!(one("[.people[].age] | add / length", PEOPLE), "60.5");
    assert_eq!(
        one(".people | map(.name) | join(\", \")", PEOPLE),
        r#""Ada, Grace""#
    );
    assert_eq!(
        one("\"\\(.people[0].name) is \\(.people[0].age)\"", PEOPLE),
        r#""Ada is 36""#
    );
    assert_eq!(
        one(".people[0] | keys", PEOPLE),
        r#"["age","langs","name"]"#
    );
    assert_eq!(
        one(".people[0] | to_entries | .[0]", PEOPLE),
        r#"{"key":"name","value":"Ada"}"#
    );
    assert_eq!(one("[paths] | length", r#"{"a":[1,{"b":2}]}"#), "4");
    assert_eq!(one("[.. | numbers]", r#"{"a":[1,{"b":2}]}"#), "[1,2]");
    assert_eq!(one(".a += 1", r#"{"a":1}"#), r#"{"a":2}"#);
    assert_eq!(one(".a |= . * 2", r#"{"a":3}"#), r#"{"a":6}"#);
    // jaq (unlike jq) does not create objects inside `null` on assignment, so the
    // intermediate object exists here.
    assert_eq!(
        one("setpath([\"a\",\"b\"]; 1)", r#"{"a":{}}"#),
        r#"{"a":{"b":1}}"#
    );
    assert_eq!(one("getpath([\"a\",\"b\"])", r#"{"a":{"b":7}}"#), "7");
    assert_eq!(one("del(.a)", r#"{"a":1,"b":2}"#), r#"{"b":2}"#);
    assert_eq!(one("[limit(3; repeat(1))]", "null"), "[1,1,1]");
    assert_eq!(one("[range(5)] | .[1:3]", "null"), "[1,2]");
    assert_eq!(one("reduce .[] as $x (0; . + $x)", "[1,2,3]"), "6");
    assert_eq!(one("[foreach .[] as $x (0; . + $x)]", "[1,2,3]"), "[1,3,6]");
    assert_eq!(one("def double: . * 2; map(double)", "[1,2]"), "[2,4]");
    assert_eq!(one("try error(\"no\") catch .", "null"), r#""no""#);
    assert_eq!(one(".[] |= tostring", "[1,2]"), r#"["1","2"]"#);
    assert_eq!(one("\"a,b\" / \",\"", "null"), r#"["a","b"]"#);
    assert_eq!(one("\"ab\" * 3", "null"), r#""ababab""#);
    assert_eq!(one("{a:1} * {b:2}", "null"), r#"{"a":1,"b":2}"#);
    assert_eq!(one("[1,2,3] - [2]", "null"), "[1,3]");
    assert_eq!(one("7 % 3", "null"), "1");
    assert_eq!(one("tojson | fromjson", r#"{"a":[1]}"#), r#"{"a":[1]}"#);
    assert_eq!(one("\"36\" | tonumber", "null"), "36");
    assert_eq!(
        one("[.[] | contains(\"b\")]", r#"["abc","x"]"#),
        "[true,false]"
    );
    assert_eq!(one("has(\"a\")", r#"{"a":1}"#), "true");
    assert_eq!(one("[1,2,1] | unique", "null"), "[1,2]");
    assert_eq!(one("[3,1] | min", "null"), "1");
    assert_eq!(one("\"x\" | ascii_upcase", "null"), r#""X""#);
    assert_eq!(one("0 | todate", "null"), r#""1970-01-01T00:00:00Z""#);
    assert_eq!(one("@base64", r#""hi""#), r#""aGk=""#);
}

#[test]
fn the_cartesian_order_of_operators_is_jaqs() {
    // Rewriting arithmetic and comparison into guarded natives must not reorder a stream.
    assert_eq!(one("[(1,2) - (10,20)]", "null"), "[-9,-19,-8,-18]");
    assert_eq!(one("[(1,2) < (2,1)]", "null"), "[true,false,false,false]");
    assert_eq!(
        one("[range(0,1; 3,4)]", "null"),
        "[0,1,2,0,1,2,3,1,2,1,2,3]"
    );
    assert_eq!(
        one("[{a: (1,2), b: (3,4)} | [.a, .b]]", "null"),
        "[[1,3],[1,4],[2,3],[2,4]]"
    );
    assert_eq!(one("[(1,2) as $x | ($x, 10) + 1]", "null"), "[2,11,3,11]");
    assert_eq!(one("5 - 3", "null"), "2", "operands in order");
    assert_eq!(one("\"ab\" + \"cd\"", "null"), r#""abcd""#);
}

#[test]
fn a_stream_is_every_result_in_order() {
    assert_eq!(jq(".[]", "[1,2,3]").unwrap(), ["1", "2", "3"]);
    assert_eq!(jq("empty", "null").unwrap(), Vec::<String>::new());
}

#[test]
fn scalars_carry_their_text() {
    let out = run(
        ".name",
        br#"{"name":"Ada \"the\" first"}"#,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(out[0].scalar_text.as_deref(), Some("Ada \"the\" first"));
    assert_eq!(out[0].json, r#""Ada \"the\" first""#);
    let out = run(".", b"36", &Limits::default()).unwrap();
    assert_eq!(out[0].scalar_text.as_deref(), Some("36"));
    let out = run(".", b"[1]", &Limits::default()).unwrap();
    assert_eq!(out[0].scalar_text, None);
}

// ---- what is in and what is out -----------------------------------------------------------

#[test]
fn every_native_is_classified() {
    let wrapped_names: Vec<&str> = WRAPPED.to_vec();
    for name in ikigai_json::sandbox::upstream_natives() {
        let known = AVAILABLE.contains(&name.as_str())
            || EXCLUDED.iter().any(|(x, _)| *x == name)
            || wrapped_names.contains(&name.as_str());
        assert!(
            known,
            "jaq offers `{name}`, which this sandbox has not classified: review it and list it \
             in AVAILABLE (with a guard in WRAPPED if it recurses or allocates by its operands) \
             or EXCLUDED"
        );
    }
    for wrapped in WRAPPED {
        assert!(
            AVAILABLE.contains(wrapped),
            "{wrapped} is wrapped, so it is available"
        );
    }
}

#[test]
fn every_builtin_definition_is_classified_and_the_kept_ones_compile() {
    for name in ikigai_json::sandbox::upstream_defs() {
        if DROPPED_DEFS.contains(&name.as_str()) {
            continue;
        }
        let (fun, arity) = name.rsplit_once('/').unwrap();
        if fun.starts_with('@') || arity != "0" {
            continue;
        }
        // Every kept zero-argument builtin resolves (it may still fail on this input).
        let program = format!("try {fun} catch null");
        if let Err(Refusal::Program(e)) = jq(&program, "null") {
            panic!("the kept builtin `{name}` does not compile: {e}");
        }
    }
}

#[test]
fn each_excluded_definition_is_refused_by_name() {
    for program in [
        "env",
        "$ENV",
        "now",
        "localtime",
        "input",
        "inputs",
        "debug",
        "stderr",
        "halt",
        "halt_error",
        "input_filename",
        "$__loc__",
        "test(\"a\")",
        "sub(\"a\"; \"b\")",
        "splits(\"a\")",
        "match(\"a\")",
        "\"a\" | strflocaltime(\"%Y\")",
        "debug(1)",
        "import \"x\" as x; .",
        "include \"x\"; .",
    ] {
        match jq(program, "\"a\"") {
            Err(Refusal::Program(m)) => assert!(!m.is_empty(), "{program}"),
            other => panic!("`{program}` must be refused as a program, got {other:?}"),
        }
    }
    let Err(Refusal::Program(m)) = jq("env", "null") else {
        unreachable!()
    };
    assert!(m.contains("environment"), "{m}");
    let Err(Refusal::Program(m)) = jq("test(\"a\")", "null") else {
        unreachable!()
    };
    assert!(m.contains("not available"), "{m}");
}

#[test]
fn a_syntax_error_says_where() {
    let Err(Refusal::Program(m)) = jq(".a | | .b", "null") else {
        panic!("a syntax error is a program refusal")
    };
    assert!(m.contains("expected"), "{m}");
}

#[test]
fn a_runtime_error_is_a_runtime_refusal() {
    let Err(Refusal::Runtime(m)) = jq(".a", "[1]") else {
        panic!("indexing an array by a key raises")
    };
    assert!(m.contains("cannot"), "{m}");
}

// ---- bounds, in process (each refuses without endangering the process) --------------------

fn bound_of(program: &str, input: &str, limits: Limits) -> &'static str {
    match run(program, input.as_bytes(), &limits) {
        Err(Refusal::Bound { bound, .. }) => bound,
        Err(Refusal::Timeout(_)) => "time",
        other => panic!("`{program}` must reach a bound, got {other:?}"),
    }
}

fn small() -> Limits {
    Limits {
        fuel: 200_000,
        ..Limits::default()
    }
}

#[test]
fn a_program_past_the_size_bound_is_refused_before_it_is_parsed() {
    let program = ". ".repeat(4 * 1024);
    assert_eq!(bound_of(&program, "null", Limits::default()), "program");
}

#[test]
fn loops_run_out_of_fuel_and_try_cannot_catch_it() {
    for program in [
        "last(range(1e18))",
        "[range(1e18)] | length",
        "last(repeat(1))",
        "def f: f; f",
        "def f: 1, f; last(f)",
        "reduce range(1e18) as $x (0; . + 1)",
        "[limit(1e9; repeat(.))] | length",
        "until(false; .)",
        "last(recurse(. + 1))",
        "try last(range(1e18)) catch 0",
        "def r: try r catch r; r",
        "[.[] as $a | .[] as $b | .[] as $c | 1] | length",
    ] {
        let input = if program.contains("[.[] as") {
            format!("[{}]", vec!["1"; 300].join(","))
        } else {
            "null".to_string()
        };
        let bound = bound_of(program, &input, small());
        assert!(
            matches!(bound, "fuel" | "stack"),
            "`{program}` reached {bound}"
        );
    }
}

#[test]
fn a_string_bomb_is_refused_before_it_is_built() {
    for program in [
        "\"x\" * 1e12",
        "\"x\" * 1e18 | length",
        "reduce range(40) as $i (\"ab\"; . + .) | length",
        "\"ab\" | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | .+. | length",
    ] {
        let bound = bound_of(program, "null", Limits::default());
        assert!(
            matches!(bound, "string" | "fuel" | "time"),
            "`{program}` reached {bound}"
        );
    }
}

#[test]
fn an_array_bomb_is_refused() {
    let doubling = format!("[1] {} | length", "| [.[], .[]] ".repeat(40));
    // In a debug build the wall clock can run out first; either way it stops.
    let bound = bound_of(&doubling, "null", Limits::default());
    assert!(matches!(bound, "fuel" | "time"), "{doubling}: {bound}");
    let bound = bound_of("[range(1e9)] | length", "null", Limits::default());
    assert!(matches!(bound, "fuel" | "time"), "{bound}");
    // The product is charged BEFORE the difference runs, so this one is fuel on any machine.
    let product = bound_of("[range(1e3)] as $a | $a - $a", "null", small());
    assert_eq!(product, "fuel");
}

#[test]
fn a_shared_value_cannot_expand_past_the_bounds_when_serialized() {
    // Forty levels of `{a: ., b: .}` share structure: 40 small objects, 2^40 leaves.
    let dag = format!("1 {}", "| {a: ., b: .} ".repeat(40));
    for tail in ["", "| tojson | length", "| \"\\(.)\" | length", "| . == ."] {
        let program = format!("{dag}{tail}");
        let bound = bound_of(&program, "null", Limits::default());
        assert!(
            matches!(bound, "fuel" | "time" | "string" | "output"),
            "`{tail}` reached {bound}"
        );
    }
}

#[test]
fn an_integer_bomb_is_refused() {
    assert_eq!(
        bound_of(
            "reduce range(40) as $i (3; . * .)",
            "null",
            Limits::default()
        ),
        "integer"
    );
    // Within the bound, big integers work.
    assert_eq!(
        one("100000000000000000000 * 100000000000000000000", "null"),
        "10000000000000000000000000000000000000000"
    );
}

#[test]
fn deep_values_are_refused_at_construction() {
    for program in [
        "reduce range(1e6) as $i (null; [.])",
        "reduce range(1e6) as $i (null; {a: .})",
        "reduce range(1e6) as $i ({}; .a = .)",
        "reduce range(1e6) as $i (null; [.]) | tojson",
        "def f: [f]; f",
        "[\"[\" * 100000 | fromjson]",
        "{} | setpath([range(100000) | \"a\"]; 1)",
        "reduce range(1e6) as $i (1; path_value(.))",
        "reduce range(1e6) as $i ([1]; group_by(.))",
    ] {
        let bound = bound_of(program, "null", Limits::default());
        assert!(
            matches!(bound, "depth" | "stack" | "path" | "fuel" | "time"),
            "`{program}` reached {bound}"
        );
    }
}

#[test]
fn a_long_update_path_cannot_deepen_a_value() {
    // jaq does not create containers inside `null` when it assigns, so a path longer than the
    // value raises rather than building a deep value; the fold step's tick bounds the work.
    assert!(matches!(
        jq("(reduce range(100000) as $i (.; .a)) = 1", "{}"),
        Err(Refusal::Runtime(_))
    ));
}

#[test]
fn output_is_bounded() {
    let limits = Limits {
        max_output_bytes: 1000,
        ..Limits::default()
    };
    assert_eq!(bound_of("range(1000)", "null", limits), "output");
    assert_eq!(bound_of("\"x\" * 2000", "null", limits), "output");
}

#[test]
fn non_string_keys_cannot_leave_as_json() {
    assert_eq!(bound_of("{([1]): 2}", "null", Limits::default()), "output");
}

#[test]
fn input_bounds() {
    let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    assert!(matches!(jq(".", &deep), Err(Refusal::Input(_))));
    assert!(matches!(jq(".", "{"), Err(Refusal::Input(_))));
    let limits = Limits {
        max_input_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        run(".", b"[1,2,3,4,5,6]", &limits),
        Err(Refusal::Input(_))
    ));
}

#[test]
fn the_default_fuel_is_spent_in_reasonable_time() {
    // In a release build the default fuel runs out well inside the wall-clock budget, so fuel
    // (deterministic) is the bound that stops a loop and the clock is the backstop. A debug
    // build on a slow CI runner can take longer than the budget (measured: CI's debug runner
    // hit the 5 s clock first), so there only "it stopped" is asserted.
    let start = std::time::Instant::now();
    let bound = bound_of("last(range(1e18))", "null", Limits::default());
    let elapsed = start.elapsed();
    eprintln!("default fuel spent in {elapsed:?} (bound: {bound})");
    if cfg!(debug_assertions) {
        assert!(matches!(bound, "fuel" | "time"), "{bound}");
    } else {
        assert_eq!(bound, "fuel", "release: fuel stops first ({elapsed:?})");
    }
}

#[test]
fn the_time_budget_holds() {
    let limits = Limits {
        fuel: u64::MAX,
        time_budget_ms: 200,
        ..Limits::default()
    };
    let start = std::time::Instant::now();
    assert_eq!(bound_of("last(range(1e18))", "null", limits), "time");
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
}

// ---- the hostile cases, in a child process ------------------------------------------------

const PROBE: &str = "IKIGAI_JSON_SANDBOX_PROBE";

/// The programs that, unguarded, abort the process (stack overflow or allocation failure).
const HOSTILE: &[&str] = &[
    "def f: [f]; f",
    "reduce range(1e7) as $i (null; [.]) | tojson",
    "reduce range(1e7) as $i (null; [.]) | . == .",
    "reduce range(1e7) as $i (null; [.]) | [..] | length",
    "reduce range(1e7) as $i (null; [.]) | contains(.)",
    "reduce range(1e7) as $i (null; [.]) | walk(.)",
    "reduce range(1e7) as $i (null; [.]) | .a",
    "try (reduce range(1e7) as $i (null; [.]) | .a) catch 1",
    "reduce range(1e7) as $i ({}; .a = .) | tojson",
    "{} | (reduce range(1e7) as $i (.; .a)) = 1",
    "\"[\" * 1000000 | fromjson",
    "\"x\" * 1e15",
    "reduce range(60) as $i (3; . * .)",
    "1 | [.,.] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | [.[],.[]] | length",
    "[range(1e10)]",
    "\"a\" * 10000000 | explode | length",
    "def f: try f catch f; f",
];

#[test]
fn hostile_programs_are_refused_and_abort_nothing() {
    for (i, program) in HOSTILE.iter().enumerate() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "probe_child", "--nocapture", "--test-threads=1"])
            .env(PROBE, i.to_string())
            .output()
            .expect("the test binary re-executes");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`{program}` killed the process ({}):\n{stderr}",
            out.status
        );
        let verdict = stdout
            .lines()
            .find_map(|l| l.split_once("VERDICT ").map(|(_, v)| v))
            .unwrap_or_else(|| panic!("`{program}` printed no verdict:\n{stdout}\n{stderr}"));
        assert!(
            verdict.starts_with("refused"),
            "`{program}` must be refused: {verdict}"
        );
        eprintln!("{program:.60}  →  {verdict:.120}");
    }
}

#[test]
fn probe_child() {
    let Ok(index) = std::env::var(PROBE) else {
        return;
    };
    let program = HOSTILE[index.parse::<usize>().unwrap()];
    let verdict = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || match run(program, b"null", &Limits::default()) {
            Ok(out) => format!("answered {} results", out.len()),
            Err(e) => format!("refused: {e}"),
        })
        .unwrap()
        .join()
        .unwrap();
    println!("VERDICT {verdict}");
}
