mod common;

use common::{Host, PEOPLE_IRI};
use futures::executor::block_on;
use ikigai_core::{Capability, Clock, Error, Time};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

struct AdvancingClock(AtomicU64);
impl Clock for AdvancingClock {
    fn now(&self) -> Time {
        Time::from_millis(self.0.fetch_add(10_000, Ordering::SeqCst))
    }
}

#[test]
fn query_deadline_uses_the_invocation_clock() {
    let host = Host::new();
    let request = host.request(
        "urn:json:query",
        &[("src", PEOPLE_IRI), ("q", "last(range(10000))")],
    );
    let kernel = host
        .kernel
        .with_clock(Arc::new(AdvancingClock(AtomicU64::new(0))));
    let result = block_on(kernel.issue(request, &Capability::root()));
    assert!(
        matches!(result, Err(Error::OverBudget { ref detail }) if detail.contains("ms budget")),
        "{result:?}"
    );
}

#[test]
fn sandbox_polls_host_clock_and_timeout_escapes_jq_try() {
    use ikigai_json::sandbox::{run_with_clock, Limits, Refusal};
    let calls = AtomicU64::new(0);
    let now = || {
        Some(Time::from_millis(
            calls.fetch_add(1, Ordering::SeqCst) * 100,
        ))
    };
    let limits = Limits {
        time_budget_ms: 150,
        fuel: 100_000,
        ..Limits::default()
    };
    let result = run_with_clock("try last(range(10000)) catch 0", b"null", &limits, &now);
    assert!(matches!(result, Err(Refusal::Timeout(_))), "{result:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "one initial read and two periodic checks"
    );
}

#[test]
fn a_fixed_or_absent_clock_keeps_fuel_as_a_bound() {
    use ikigai_json::sandbox::{run_with_clock, Limits, Refusal};
    let limits = Limits {
        fuel: 10_000,
        ..Limits::default()
    };
    for time in [Some(Time::from_millis(123)), None] {
        let result = run_with_clock("last(range(10000))", b"null", &limits, &|| time);
        assert!(
            matches!(result, Err(Refusal::Bound { bound: "fuel", .. })),
            "{result:?}"
        );
    }
}
