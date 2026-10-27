//! The jq sandbox: a caller's program, run by jaq, under bounds that hold whatever it says.
//!
//! A jq program arrives in a request, and jq is a full language: it loops (`repeat`, `range`,
//! `reduce`, recursion), it builds values of any size and depth in a handful of characters
//! (`"x" * 1e12`, `. * .` on an integer twenty times, `[.[], .[]]` forty times), and many of
//! jaq's operations recurse on the Rust stack, where an overflow aborts the WHOLE host. So the
//! program is not run as written. Five mechanisms, each answering something the others cannot:
//!
//! 1. **A metered context.** jaq clones its evaluation context at every pipe, call, binding and
//!    fold step, and the context carries this crate's data, so every clone charges one unit of
//!    FUEL, checks the wall clock every few thousand units, and checks how much stack the
//!    evaluation has used. Running out of any of them ends the run with a refusal that names
//!    the bound. The refusal is a Rust unwind, not a jq error, so `try` cannot catch it.
//! 2. **A rewritten program.** The program (and every jq-defined builtin it can reach) is parsed
//!    to jaq's syntax tree and rewritten before it is compiled: each element collected into an
//!    array and each object value is walked for depth (charged as fuel), arithmetic goes
//!    through guarded natives that refuse a string repeat, integer or array product too large
//!    BEFORE computing it, comparisons walk their operands first, string interpolation is
//!    size-checked, and `..` and every `reduce`/`foreach` step pass a tick.
//! 3. **A curated library.** Natives are an ALLOWLIST: no environment, clock, logging, halting,
//!    input or module loading, and no regular expressions (their cost cannot be bounded before
//!    they run). Natives that recurse or allocate in proportion to their operands are wrapped
//!    in a guard. [`AVAILABLE`] and [`EXCLUDED`] name exactly what is in and out, and a test
//!    fails when jaq adds a native this file has not classified.
//! 4. **A large stack of its own** (native targets): the evaluation runs on a thread of
//!    [`EVAL_STACK`], and the stack check in (1) refuses at [`STACK_GUARD`], so everything that
//!    recurses without passing a check (dropping a value, formatting an error) has more than
//!    ten times that left.
//! 5. **Bounded output.** Every result is walked for depth and size before it is serialized, and
//!    the total is capped.
//!
//! On wasm there are no worker threads: (4) does not apply. The host can supply a clock
//! for (1), including through the kernel invocation. An unwind is an abort, trapping the
//! in-page kernel rather than returning a refusal. The page is a single user's, and a trap reaches nothing
//! else, so the bound still holds there; it is just less polite.

use std::cell::Cell;
use std::fmt::Write as _;

use jaq_core::box_iter::box_once;
use jaq_core::load::parse::{BinaryOp, Def, Pattern, Term};
use jaq_core::load::{self, Arena, File, Loader};
use jaq_core::native::{Filter, Fun};
use jaq_core::ops::Math;
use jaq_core::{data, Compiler, Ctx, Cv, DataT, Error, Exn, Native, ValXs, Vars};
use jaq_json::{Num, Val};
use jaq_std::ValT as _;

// --- the bounds ---------------------------------------------------------------------------

/// The bounds a run is held to. [`Limits::default`] is what `urn:json:query` uses.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The largest program, in bytes.
    pub max_program_bytes: usize,
    /// The largest input document, in bytes.
    pub max_input_bytes: usize,
    /// How much work a run may do, in fuel units (one per context clone, collected element,
    /// walked node, or unit of guarded arithmetic).
    pub fuel: u64,
    /// The wall-clock budget, in milliseconds. On wasm a caller must supply a clock.
    pub time_budget_ms: u64,
    /// The deepest value a run may build, compare, sort or serialize.
    pub max_depth: usize,
    /// The largest string a run may build by `+`, `*`, interpolation or `tojson`, in bytes.
    pub max_string_bytes: usize,
    /// The largest integer a run may build by arithmetic, in bits.
    pub max_int_bits: u64,
    /// The total size of the serialized results, in bytes.
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_program_bytes: MAX_PROGRAM_BYTES,
            max_input_bytes: MAX_INPUT_BYTES,
            fuel: FUEL,
            time_budget_ms: TIME_BUDGET_MS,
            max_depth: MAX_DEPTH,
            max_string_bytes: MAX_STRING_BYTES,
            max_int_bits: MAX_INT_BITS,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

/// The largest jq program: 4 KiB. A selection is a line; a view's whole query is a paragraph.
pub const MAX_PROGRAM_BYTES: usize = 4 * 1024;
/// The largest input document: 16 MiB.
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// The fuel a run may spend: 20,000,000 units. Measured on 2026-10-23 with other builds on the
/// CPU: about 0.6 s of jaq in a release build and 3 s in a debug one
/// (`the_default_fuel_is_spent_in_reasonable_time` prints it). Enough to select from every
/// record of a large document several times over.
pub const FUEL: u64 = 20_000_000;
/// The wall-clock budget: 5 seconds. Fuel is the deterministic bound; this one catches a slow
/// machine or a debug build. On wasm the caller supplies the clock.
pub const TIME_BUDGET_MS: u64 = 5_000;
/// The deepest value a run may build or consume: 512, twice [`MAX_JSON_NESTING`](crate::MAX_JSON_NESTING),
/// so a query can wrap its deepest input a few times over.
pub const MAX_DEPTH: usize = 512;
/// The largest string a run may build: 8 MiB.
pub const MAX_STRING_BYTES: usize = 8 * 1024 * 1024;
/// The largest integer arithmetic may build: 65,536 bits (about 19,700 decimal digits).
pub const MAX_INT_BITS: u64 = 65_536;
/// The total serialized size of the results: 8 MiB.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// The stack the evaluation runs on (native targets): 1 GiB of reserved address space, touched
/// only as deep as a run actually recurses.
pub const EVAL_STACK: usize = 1 << 30;
/// How much of [`EVAL_STACK`] the metered checks let a run use before refusing: 64 MiB. The
/// sixteen-fold remainder is for what recurses without passing a check: dropping a value, or
/// jaq formatting one into an error message.
pub const STACK_GUARD: usize = 64 << 20;

/// Every native this sandbox offers, by `name/arity`, as jaq names them. Wrapped natives are
/// listed under their public name; their guard is described beside [`WRAPPED`].
pub const AVAILABLE: &[&str] = &[
    // jaq-core
    "error_empty/0",
    "path/1",
    "path_value/1",
    "range/3",
    "keys_unsorted/0",
    "key_values/0",
    "first/1",
    "last/1",
    "limit/2",
    "skip/2",
    // jaq-std: strings, arrays, numbers
    "floor/0",
    "round/0",
    "ceil/0",
    "utf8bytelength/0",
    "explode/0",
    "implode/0",
    "ascii_downcase/0",
    "ascii_upcase/0",
    "reverse/0",
    "sort/0",
    "sort_by/1",
    "group_by/1",
    "min_by_or_empty/1",
    "max_by_or_empty/1",
    "startswith/1",
    "endswith/1",
    "ltrimstr/1",
    "rtrimstr/1",
    "trim/0",
    "ltrim/0",
    "rtrim/0",
    "escape_sh/0",
    "escape_html/0",
    "unescape_html/0",
    "encode_uri/0",
    "decode_uri/0",
    "encode_base64/0",
    "decode_base64/0",
    // jaq-std: math (libm)
    "acos/0",
    "acosh/0",
    "asin/0",
    "asinh/0",
    "atan/0",
    "atanh/0",
    "cbrt/0",
    "cos/0",
    "cosh/0",
    "erf/0",
    "erfc/0",
    "exp/0",
    "exp10/0",
    "exp2/0",
    "expm1/0",
    "fabs/0",
    "frexp/0",
    "ilogb/0",
    "j0/0",
    "j1/0",
    "lgamma/0",
    "log/0",
    "log10/0",
    "log1p/0",
    "log2/0",
    "modf/0",
    "nearbyint/0",
    "rint/0",
    "sin/0",
    "sinh/0",
    "sqrt/0",
    "tan/0",
    "tanh/0",
    "tgamma/0",
    "trunc/0",
    "y0/0",
    "y1/0",
    "atan2/2",
    "copysign/2",
    "fdim/2",
    "fmax/2",
    "fmin/2",
    "fmod/2",
    "hypot/2",
    "jn/2",
    "ldexp/2",
    "nextafter/2",
    "pow/2",
    "remainder/2",
    "scalbln/2",
    "yn/2",
    "fma/3",
    // jaq-std: time, UTC only (pure functions of their input)
    "fromdateiso8601/0",
    "todateiso8601/0",
    "strftime/1",
    "gmtime/0",
    "strptime/1",
    "mktime/0",
    // jaq-json
    "fromjson/0",
    "tojson/0",
    "tobytes/0",
    "length/0",
    "contains/1",
    "has/1",
    "indices/1",
    "bsearch/1",
];

/// The natives jaq offers that this sandbox does NOT, by `name/arity`, with the reason.
pub const EXCLUDED: &[(&str, &str)] = &[
    ("env/0", "reads the host's environment"),
    (
        "now/0",
        "reads the clock, so the answer would not be a function of the input",
    ),
    ("localtime/0", "reads the host's time zone"),
    ("strflocaltime/1", "reads the host's time zone"),
    ("halt/1", "halting is the host's decision"),
    ("debug_empty/0", "writes to the host's log"),
    ("stderr_empty/0", "writes to the host's standard error"),
    (
        "matches/2",
        "regular expressions: the cost of a match cannot be bounded before it runs",
    ),
    (
        "split_matches/2",
        "regular expressions: the cost of a match cannot be bounded before it runs",
    ),
    (
        "split_/2",
        "regular expressions: the cost of a match cannot be bounded before it runs",
    ),
];

/// The natives this sandbox runs behind a guard, by `name/arity`. Each is renamed `__iki_NAME`
/// and called from a jq definition of the public name that checks the operands first.
pub const WRAPPED: &[&str] = &[
    "range/3",    // each output passes a tick, so `last(range(1e18))` runs out of fuel
    "tojson/0",   // size- and depth-checked before serializing
    "fromjson/0", // this crate's own: the string's nesting is scanned before jaq parses it
    "explode/0",  // charged by length (one value per character)
    "contains/1", // depth-checked; charged by the product of the operands' lengths
    "indices/1",  // likewise
    "bsearch/1",  // depth-checked
    "has/1",      // the key is depth-checked
    "sort/0",     // depth-checked (comparison recurses)
    "sort_by/1",  // the input and every key are depth-checked
    "group_by/1", // likewise, and the output is walked (it adds a level)
    "min_by_or_empty/1",
    "max_by_or_empty/1",
    "path_value/1", // its output is walked (it adds a level)
    "key_values/0", // likewise
];

/// The jq-defined builtins this sandbox drops, by `name/arity`, because they reach an excluded
/// native. (`getpath/1` is replaced, with the path's length checked first.)
pub const DROPPED_DEFS: &[&str] = &[
    "getpath/1",
    "stderr/0",
    "debug/0",
    "debug/1",
    "halt/0",
    "halt_error/1",
    "halt_error/0",
    "test/2",
    "scan_/2",
    "match/2",
    "capture/2",
    "split/2",
    "splits/2",
    "sub/3",
    "gsub/3",
    "scan/2",
    "test/1",
    "scan/1",
    "match/1",
    "capture/1",
    "splits/1",
    "sub/2",
    "gsub/2",
];

/// The guarding definitions, in jq. They come FIRST in the prelude, so the builtins defined
/// after them (`keys`, `unique`, `min`, `setpath`, …) bind to these rather than to the raw
/// natives, which no longer exist under their public names.
const WRAPPERS: &str = r#"
def range($from; $upto; $by): __iki_range($from; $upto; $by) | __iki_tick;
def tojson: __iki_sg | __iki_tojson;
def explode: __iki_charge_len | __iki_explode;
def contains($x): __iki_product($x) | __iki_contains($x);
def indices($x): __iki_product($x) | __iki_indices($x);
def bsearch($x): __iki_dg_of($x) | __iki_dg | __iki_bsearch($x);
def has($k): __iki_dg_of($k) | __iki_has($k);
def sort: __iki_dg | __iki_sort;
def sort_by(f): __iki_dg | __iki_sort_by(f | __iki_dg);
def group_by(f): __iki_dg | __iki_group_by(f | __iki_dg) | __iki_elem;
def min_by_or_empty(f): __iki_dg | __iki_min_by_or_empty(f | __iki_dg);
def max_by_or_empty(f): __iki_dg | __iki_max_by_or_empty(f | __iki_dg);
def path_value(f): __iki_path_value(f) | __iki_elem;
def key_values: __iki_key_values | __iki_elem;
def getpath($path): __iki_path_len($path) | reduce $path[] as $p (.; .[$p]);
"#;

// --- refusals ---------------------------------------------------------------------------

/// Why a run produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The program does not parse, or names something that does not exist here.
    Program(String),
    /// The input is not a JSON document, or is past a bound before it is parsed.
    Input(String),
    /// The program ran and raised a jq error.
    Runtime(String),
    /// A bound was reached. `bound` names it (`fuel`, `stack`, `depth`, `string`, `integer`,
    /// `output`, `path`).
    Bound {
        /// Which bound.
        bound: &'static str,
        /// What happened, for a person.
        detail: String,
    },
    /// The wall-clock budget ran out.
    Timeout(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Program(m) => write!(f, "the jq program is not runnable here: {m}"),
            Refusal::Input(m) => write!(f, "the input is not usable: {m}"),
            Refusal::Runtime(m) => write!(f, "the jq program raised an error: {m}"),
            Refusal::Bound { bound, detail } => {
                write!(f, "the {bound} bound was reached: {detail}")
            }
            Refusal::Timeout(m) => write!(f, "{m}"),
        }
    }
}

/// The payload a run unwinds with when it reaches a bound. `try` cannot catch an unwind.
struct Exhausted(Refusal);

fn exhaust(bound: &'static str, detail: String) -> ! {
    std::panic::resume_unwind(Box::new(Exhausted(Refusal::Bound { bound, detail })))
}

// --- the meter --------------------------------------------------------------------------

/// What a run has spent, and where it must stop.
pub(crate) struct Meter<'a> {
    now: Option<&'a (dyn Fn() -> Option<ikigai_core::Time> + Sync)>,
    host_deadline: Option<ikigai_core::Time>,
    used: Cell<u64>,
    next_check: Cell<u64>,
    limits: Limits,
    stack_base: usize,
    #[cfg(not(target_family = "wasm"))]
    deadline: std::time::Instant,
}

/// How often (in fuel units) the wall clock and the stack are checked.
const CHECK_EVERY: u64 = 1024;

/// The address of a local in the caller's frame: how deep the stack is right now. Stacks grow
/// down on every target this builds for, and the comparison takes the absolute difference
/// anyway.
#[inline(never)]
fn stack_position() -> usize {
    let here = 0u8;
    std::hint::black_box(&here) as *const u8 as usize
}

impl<'a> Meter<'a> {
    fn new(
        limits: Limits,
        now: Option<&'a (dyn Fn() -> Option<ikigai_core::Time> + Sync)>,
    ) -> Self {
        Meter {
            host_deadline: now
                .and_then(|now| now())
                .map(|t| t.plus_millis(limits.time_budget_ms)),
            now,
            used: Cell::new(0),
            next_check: Cell::new(CHECK_EVERY),
            limits,
            stack_base: stack_position(),
            #[cfg(not(target_family = "wasm"))]
            deadline: std::time::Instant::now()
                + std::time::Duration::from_millis(limits.time_budget_ms),
        }
    }

    /// Spend `n` units, refusing past the fuel bound. The stack is checked on every call (it is
    /// one subtraction), the clock every [`CHECK_EVERY`] units.
    fn charge(&self, n: u64) {
        let used = self.used.get().saturating_add(n);
        self.used.set(used);
        if used > self.limits.fuel {
            exhaust(
                "fuel",
                format!(
                    "the program did more than {} units of work (one per pipe, call, binding, \
                     collected element or walked value); narrow the selection or the input",
                    self.limits.fuel
                ),
            );
        }
        let depth = self.stack_base.abs_diff(stack_position());
        if depth > STACK_GUARD {
            exhaust(
                "stack",
                format!(
                    "the program recursed more than {} MiB deep; a recursion this deep has no \
                     base case on this input",
                    STACK_GUARD >> 20
                ),
            );
        }
        if used >= self.next_check.get() {
            self.next_check.set(used + CHECK_EVERY);
            let timed_out = match (self.now, self.host_deadline) {
                (Some(now), Some(deadline)) => now().is_some_and(|time| time > deadline),
                _ => {
                    #[cfg(not(target_family = "wasm"))]
                    {
                        std::time::Instant::now() > self.deadline
                    }
                    #[cfg(target_family = "wasm")]
                    {
                        false
                    }
                }
            };
            if timed_out {
                std::panic::resume_unwind(Box::new(Exhausted(Refusal::Timeout(format!(
                    "the jq program ran past its {} ms budget",
                    self.limits.time_budget_ms
                )))));
            }
        }
    }

    /// Whether `n` more units would pass the fuel bound — for an operation whose cost is known
    /// before it runs, so it is refused instead of started.
    fn precharge(&self, n: u64, what: &str) {
        if self.used.get().saturating_add(n) > self.limits.fuel {
            exhaust(
                "fuel",
                format!(
                    "{what} would cost {n} units of work, more than the {} the program has left",
                    self.limits.fuel.saturating_sub(self.used.get())
                ),
            );
        }
        self.charge(n);
    }

    fn depth_refusal(&self, what: &str) -> ! {
        exhaust(
            "depth",
            format!(
                "{what} nests deeper than {} arrays and objects; jq's operations recurse once \
                 per level",
                self.limits.max_depth
            ),
        )
    }
}

// --- the data jaq carries ---------------------------------------------------------------

/// The data type this sandbox runs jaq over: [`Val`] values, and a context that carries the
/// meter.
pub(crate) struct Sandbox;

impl DataT for Sandbox {
    type V<'a> = Val;
    type Data<'a> = Metered<'a>;
}

/// The context data: the compiled program's lookup table and the run's meter. jaq clones it at
/// every pipe, call, binding and fold step, and every clone is charged.
pub(crate) struct Metered<'a> {
    lut: &'a jaq_core::Lut<Sandbox>,
    meter: &'a Meter<'a>,
}

impl Clone for Metered<'_> {
    fn clone(&self) -> Self {
        self.meter.charge(1);
        Metered {
            lut: self.lut,
            meter: self.meter,
        }
    }
}

impl<'a> data::HasLut<'a, Sandbox> for Metered<'a> {
    fn lut(&self) -> &'a jaq_core::Lut<Sandbox> {
        self.lut
    }
}

fn meter<'a>(cv: &Cv<'a, Sandbox>) -> &'a Meter<'a> {
    cv.0.data().meter
}

// --- walking values ----------------------------------------------------------------------

/// Walk `v` without recursion, charging one unit a node, and refuse past the depth bound. With
/// `budget`, also refuse past that many nodes and refuse non-string object keys (the walk
/// before serializing JSON). Returns the number of nodes visited.
///
/// Containers count as levels and scalars do not: `1` is 0 deep, `[]` 1, `[[1]]` 2, the same
/// count the input pre-scan makes.
fn walk(m: &Meter, v: &Val, what: &str, budget: Option<(u64, &'static str)>) -> u64 {
    let mut nodes = 0u64;
    let mut stack: Vec<(&Val, usize)> = vec![(v, 0)];
    while let Some((v, depth)) = stack.pop() {
        nodes += 1;
        m.charge(1);
        if let Some((budget, bound)) = budget {
            if nodes > budget {
                exhaust(
                    bound,
                    format!("{what} would serialize to more than {budget} bytes"),
                );
            }
        }
        match v {
            Val::Arr(a) => {
                if depth + 1 > m.limits.max_depth {
                    m.depth_refusal(what);
                }
                stack.extend(a.iter().map(|e| (e, depth + 1)));
            }
            Val::Obj(o) => {
                if depth + 1 > m.limits.max_depth {
                    m.depth_refusal(what);
                }
                for (k, x) in o.iter() {
                    if budget.is_some() && !matches!(k, Val::TStr(_) | Val::BStr(_)) {
                        exhaust(
                            "output",
                            format!("{what} has an object key that is not a string, which JSON cannot hold"),
                        );
                    }
                    stack.push((k, depth + 1));
                    stack.push((x, depth + 1));
                }
            }
            _ => {}
        }
    }
    nodes
}

/// The top-level size of a value: bytes of a string, elements of an array or object, 0 else.
fn size(v: &Val) -> u64 {
    match v {
        Val::TStr(s) | Val::BStr(s) => s.len() as u64,
        Val::Arr(a) => a.len() as u64,
        Val::Obj(o) => o.len() as u64,
        _ => 0,
    }
}

/// The bits an integer operand occupies (a float or decimal is not an integer here).
fn int_bits(v: &Val) -> u64 {
    match v {
        Val::Num(Num::BigInt(b)) => b.bits(),
        Val::Num(Num::Int(_)) => 64,
        _ => 0,
    }
}

fn is_str(v: &Val) -> bool {
    matches!(v, Val::TStr(_) | Val::BStr(_))
}

// --- natives -------------------------------------------------------------------------------

type SFun = Fun<Sandbox>;
type SRun = jaq_core::RunPtr<Sandbox>;

fn native(name: &'static str, arity: usize, run: SRun) -> SFun {
    (
        name,
        jaq_core::native::v(arity),
        Native::<Sandbox>::new(run),
    )
}

/// An identity that ticks, usable in path and update position too (`..`, a fold step, a path
/// length check), so it can be inserted anywhere without changing what a program means.
fn identity_native(name: &'static str, arity: usize, run: SRun) -> SFun {
    let paths: jaq_core::PathsPtr<Sandbox> = |cv| {
        cv.0.data().meter.charge(1);
        box_once(Ok(cv.1))
    };
    let update: jaq_core::UpdatePtr<Sandbox> = |cv, f| {
        meter(&cv).charge(1);
        f(cv.1)
    };
    (
        name,
        jaq_core::native::v(arity),
        Native::<Sandbox>::new(run)
            .with_paths(paths)
            .with_update(update),
    )
}

/// Refuse a path longer than a value may nest: the one way an assignment deepens a value by
/// more than one level in a step.
fn check_path_len(m: &Meter, p: &Val) {
    if size(p) > m.limits.max_depth as u64 {
        exhaust(
            "path",
            format!(
                "a path of {} steps is longer than the {} levels a value may nest",
                size(p),
                m.limits.max_depth
            ),
        );
    }
}

/// `__iki_path_len($path)`: an identity that checks `$path` in run, path AND update position
/// (`getpath($p)` is how `setpath` and `delpaths` assign).
fn path_len_native() -> SFun {
    let run: SRun = |mut cv| {
        let p = cv.0.pop_var();
        check_path_len(meter(&cv), &p);
        ok(cv.1)
    };
    let paths: jaq_core::PathsPtr<Sandbox> = |mut cv| {
        let p = cv.0.pop_var();
        check_path_len(cv.0.data().meter, &p);
        box_once(Ok(cv.1))
    };
    let update: jaq_core::UpdatePtr<Sandbox> = |mut cv, f| {
        let p = cv.0.pop_var();
        check_path_len(meter(&cv), &p);
        f(cv.1)
    };
    (
        "__iki_path_len",
        jaq_core::native::v(1),
        Native::<Sandbox>::new(run)
            .with_paths(paths)
            .with_update(update),
    )
}

fn ok<'a>(v: Val) -> ValXs<'a, Val> {
    box_once(Ok(v))
}

fn err<'a>(e: Error<Val>) -> ValXs<'a, Val> {
    box_once(Err(Exn::from(e)))
}

/// The guarded arithmetic: check what the operation would build, charge it, then let jaq do it.
fn math<'a>(m: &Meter, op: Math, a: Val, b: Val) -> ValXs<'a, Val> {
    let limits = m.limits;
    // Integers: a product's bits are the sum, a sum's the larger plus one.
    let (ia, ib) = (int_bits(&a), int_bits(&b));
    if ia > 64 || ib > 64 {
        let bits = match op {
            Math::Mul => ia + ib,
            Math::Add | Math::Sub => ia.max(ib) + 1,
            _ => ia.max(ib),
        };
        if bits > limits.max_int_bits {
            exhaust(
                "integer",
                format!(
                    "this arithmetic would build an integer of about {bits} bits, more than \
                     the {} allowed",
                    limits.max_int_bits
                ),
            );
        }
        m.charge(bits / 64);
    }
    match op {
        Math::Add => {
            let total = size(&a) + size(&b);
            if is_str(&a) && is_str(&b) && total > limits.max_string_bytes as u64 {
                exhaust(
                    "string",
                    format!("`+` would build a string of {total} bytes"),
                );
            }
            m.precharge(total, "`+`");
        }
        Math::Mul => {
            let repeat = |s: &Val, n: &Val| -> Option<u64> {
                let n = n.as_f64()?;
                (is_str(s) && n > 0.0).then(|| (size(s) as f64 * n.floor()) as u64)
            };
            if let Some(total) = repeat(&a, &b).or_else(|| repeat(&b, &a)) {
                if total > limits.max_string_bytes as u64 {
                    exhaust(
                        "string",
                        format!("`*` would repeat a string to {total} bytes"),
                    );
                }
                m.precharge(total, "`*`");
            }
            if matches!(a, Val::Obj(_)) && matches!(b, Val::Obj(_)) {
                // A deep merge recurses on both sides.
                walk(m, &a, "an object merged with `*`", None);
                walk(m, &b, "an object merged with `*`", None);
            }
        }
        Math::Sub => {
            if let (Val::Arr(x), Val::Arr(y)) = (&a, &b) {
                // Array difference compares every pair, each comparison recursing.
                walk(m, &b, "an array subtracted with `-`", None);
                m.precharge(
                    (x.len() as u64).saturating_mul(y.len() as u64),
                    "this array difference",
                );
            }
        }
        Math::Div => {
            if is_str(&a) {
                // Splitting yields up to one value per byte.
                m.precharge(size(&a), "this string split");
            }
        }
        Math::Rem => {}
    }
    box_once(op.run(a, b).map_err(Exn::from))
}

fn natives() -> Vec<SFun> {
    let mut out: Vec<SFun> = vec![
        identity_native("__iki_tick", 0, |cv| {
            meter(&cv).charge(1);
            ok(cv.1)
        }),
        native("__iki_elem", 0, |cv| {
            walk(
                meter(&cv),
                &cv.1,
                "a value collected into an array or object",
                None,
            );
            ok(cv.1)
        }),
        native("__iki_dg", 0, |cv| {
            walk(meter(&cv), &cv.1, "a value compared or sorted", None);
            ok(cv.1)
        }),
        native("__iki_dg_of", 1, |mut cv| {
            let x = cv.0.pop_var();
            walk(meter(&cv), &x, "a value compared or looked up", None);
            ok(cv.1)
        }),
        native("__iki_product", 1, |mut cv| {
            let x = cv.0.pop_var();
            let m = meter(&cv);
            walk(m, &x, "a value compared", None);
            walk(m, &cv.1, "a value compared", None);
            m.precharge(size(&cv.1).saturating_mul(size(&x).max(1)), "this search");
            ok(cv.1)
        }),
        native("__iki_sg", 0, |cv| {
            let m = meter(&cv);
            let budget = m.limits.max_string_bytes as u64;
            walk(
                m,
                &cv.1,
                "a value turned into a string",
                Some((budget, "string")),
            );
            ok(cv.1)
        }),
        native("__iki_charge_len", 0, |cv| {
            meter(&cv).precharge(size(&cv.1), "this conversion");
            ok(cv.1)
        }),
        path_len_native(),
        native("__iki_add", 2, |mut cv| {
            let (b, a) = (cv.0.pop_var(), cv.0.pop_var());
            math(meter(&cv), Math::Add, a, b)
        }),
        native("__iki_sub", 2, |mut cv| {
            let (b, a) = (cv.0.pop_var(), cv.0.pop_var());
            math(meter(&cv), Math::Sub, a, b)
        }),
        native("__iki_mul", 2, |mut cv| {
            let (b, a) = (cv.0.pop_var(), cv.0.pop_var());
            math(meter(&cv), Math::Mul, a, b)
        }),
        native("__iki_div", 2, |mut cv| {
            let (b, a) = (cv.0.pop_var(), cv.0.pop_var());
            math(meter(&cv), Math::Div, a, b)
        }),
        native("__iki_rem", 2, |mut cv| {
            let (b, a) = (cv.0.pop_var(), cv.0.pop_var());
            math(meter(&cv), Math::Rem, a, b)
        }),
        native("fromjson", 0, |cv| {
            let m = meter(&cv);
            let Some(bytes) = cv.1.as_utf8_bytes() else {
                return err(Error::typ(cv.1.clone(), "string"));
            };
            m.charge(bytes.len() as u64);
            if crate::scan::nesting(bytes, m.limits.max_depth) > m.limits.max_depth {
                m.depth_refusal("the JSON `fromjson` was given");
            }
            match jaq_json::read::parse_single(bytes) {
                Ok(v) => ok(v),
                Err(e) => err(Error::str(format!("cannot parse as JSON: {e}"))),
            }
        }),
    ];
    let wrapped = |name: &str| WRAPPED.iter().any(|w| w.split('/').next() == Some(name));
    let upstream = jaq_core::funs::<Sandbox>()
        .chain(jaq_std::funs::<Sandbox>())
        .chain(jaq_json::funs::<Sandbox>());
    for (name, args, run) in upstream {
        let id = format!("{name}/{}", args.len());
        if name == "fromjson" || EXCLUDED.iter().any(|(x, _)| *x == id) {
            continue;
        }
        if wrapped(name) {
            out.push((renamed(name), args, run));
        } else if AVAILABLE.contains(&id.as_str()) {
            out.push((name, args, run));
        }
        // Anything else is new upstream and unreviewed: left out, and `every_native_is_classified`
        // fails until it is listed.
    }
    out
}

/// `__iki_NAME` for a wrapped native, as a `'static` name (the set is fixed and small).
fn renamed(name: &str) -> &'static str {
    match name {
        "range" => "__iki_range",
        "tojson" => "__iki_tojson",
        "explode" => "__iki_explode",
        "contains" => "__iki_contains",
        "indices" => "__iki_indices",
        "bsearch" => "__iki_bsearch",
        "has" => "__iki_has",
        "sort" => "__iki_sort",
        "sort_by" => "__iki_sort_by",
        "group_by" => "__iki_group_by",
        "min_by_or_empty" => "__iki_min_by_or_empty",
        "max_by_or_empty" => "__iki_max_by_or_empty",
        "path_value" => "__iki_path_value",
        "key_values" => "__iki_key_values",
        other => unreachable!("`{other}` is not in WRAPPED"),
    }
}

// --- rewriting the program ------------------------------------------------------------------

type T<'s> = Term<&'s str>;

fn call<'s>(name: &'static str, args: Vec<T<'s>>) -> T<'s> {
    Term::Call(name, args)
}

fn pipe<'s>(l: T<'s>, r: T<'s>) -> T<'s> {
    Term::BinOp(Box::new(l), BinaryOp::Pipe(None), Box::new(r))
}

fn math_native(op: &Math) -> &'static str {
    match op {
        Math::Add => "__iki_add",
        Math::Sub => "__iki_sub",
        Math::Mul => "__iki_mul",
        Math::Div => "__iki_div",
        Math::Rem => "__iki_rem",
    }
}

/// The variable `l op= r` binds its right side to, as jq defines it: `r as $x | l |= . op $x`.
const RHS: &str = "$__iki_rhs";

/// Rewrite a term so its constructions, arithmetic, comparisons, interpolations, `..` and fold
/// steps pass the guards. Recursion here is bounded by the program's size (and runs on the
/// evaluation thread's stack).
fn rewrite<'s>(t: T<'s>) -> T<'s> {
    let b = |t: Box<T<'s>>| Box::new(rewrite(*t));
    match t {
        Term::Id | Term::Num(_) | Term::Var(_) | Term::Break(_) => t,
        Term::Recurse => pipe(Term::Recurse, call("__iki_tick", Vec::new())),
        Term::Str(fmt, parts) => Term::Str(
            fmt,
            parts
                .into_iter()
                .map(|p| match p {
                    load::lex::StrPart::Term(t) => {
                        load::lex::StrPart::Term(pipe(rewrite(t), call("__iki_sg", Vec::new())))
                    }
                    other => other,
                })
                .collect(),
        ),
        Term::Arr(None) => Term::Arr(None),
        Term::Arr(Some(f)) => Term::Arr(Some(Box::new(pipe(
            rewrite(*f),
            call("__iki_elem", Vec::new()),
        )))),
        Term::Obj(kvs) => Term::Obj(
            kvs.into_iter()
                .map(|(k, v)| match v {
                    // `{a}`, `{$x}`, `{"a"}`: the shorthand keeps its key term as it is.
                    None => (rewrite(k), None),
                    Some(v) => (
                        pipe(rewrite(k), call("__iki_elem", Vec::new())),
                        Some(pipe(rewrite(v), call("__iki_elem", Vec::new()))),
                    ),
                })
                .collect(),
        ),
        Term::Neg(t) => Term::Neg(b(t)),
        Term::BinOp(l, op, r) => {
            let (l, r) = (rewrite(*l), rewrite(*r));
            match op {
                BinaryOp::Math(op) => call(math_native(&op), vec![l, r]),
                BinaryOp::Cmp(op) => Term::BinOp(
                    Box::new(pipe(l, call("__iki_dg", Vec::new()))),
                    BinaryOp::Cmp(op),
                    Box::new(pipe(r, call("__iki_dg", Vec::new()))),
                ),
                BinaryOp::UpdateMath(op) => Term::BinOp(
                    Box::new(r),
                    BinaryOp::Pipe(Some(Pattern::Var(RHS))),
                    Box::new(Term::BinOp(
                        Box::new(l),
                        BinaryOp::Update,
                        Box::new(call(math_native(&op), vec![Term::Id, Term::Var(RHS)])),
                    )),
                ),
                BinaryOp::Assign | BinaryOp::Update | BinaryOp::UpdateAlt => Term::BinOp(
                    Box::new(l),
                    op,
                    Box::new(pipe(r, call("__iki_elem", Vec::new()))),
                ),
                BinaryOp::Pipe(p) => Term::BinOp(
                    Box::new(l),
                    BinaryOp::Pipe(p.map(rewrite_pattern)),
                    Box::new(r),
                ),
                op @ (BinaryOp::Comma | BinaryOp::Alt | BinaryOp::Or | BinaryOp::And) => {
                    Term::BinOp(Box::new(l), op, Box::new(r))
                }
            }
        }
        Term::Label(x, t) => Term::Label(x, b(t)),
        Term::Fold(kind, xs, pat, mut args) => {
            // The step (the second argument) passes a tick, in run AND update position, so a
            // fold used as a path cannot nest without being checked.
            args = args.into_iter().map(rewrite).collect();
            if args.len() >= 2 {
                let step = std::mem::take(&mut args[1]);
                args[1] = pipe(call("__iki_tick", Vec::new()), step);
            }
            Term::Fold(kind, b(xs), rewrite_pattern(pat), args)
        }
        Term::TryCatch(t, c) => Term::TryCatch(b(t), c.map(b)),
        Term::IfThenElse(branches, els) => Term::IfThenElse(
            branches
                .into_iter()
                .map(|(c, t)| (rewrite(c), rewrite(t)))
                .collect(),
            els.map(b),
        ),
        Term::Def(defs, body) => Term::Def(defs.into_iter().map(rewrite_def).collect(), b(body)),
        Term::Call(name, args) => Term::Call(name, args.into_iter().map(rewrite).collect()),
        Term::Path(t, path) => Term::Path(
            b(t),
            jaq_core::path::Path(
                path.0
                    .into_iter()
                    .map(|(part, opt)| {
                        let part = match part {
                            jaq_core::path::Part::Index(i) => {
                                jaq_core::path::Part::Index(rewrite(i))
                            }
                            jaq_core::path::Part::Range(from, upto) => {
                                jaq_core::path::Part::Range(from.map(rewrite), upto.map(rewrite))
                            }
                        };
                        (part, opt)
                    })
                    .collect(),
            ),
        ),
    }
}

fn rewrite_pattern(p: Pattern<&str>) -> Pattern<&str> {
    match p {
        Pattern::Var(x) => Pattern::Var(x),
        Pattern::Arr(ps) => Pattern::Arr(ps.into_iter().map(rewrite_pattern).collect()),
        Pattern::Obj(kps) => Pattern::Obj(
            kps.into_iter()
                .map(|(k, p)| (rewrite(k), rewrite_pattern(p)))
                .collect(),
        ),
    }
}

fn rewrite_def(d: Def<&str>) -> Def<&str> {
    Def {
        name: d.name,
        args: d.args,
        body: rewrite(d.body),
    }
}

/// The name the user's program is defined under, as the prelude's last definition.
const MAIN: &str = "__iki_main";

/// A builtin's `'static` strings, seen as the program's shorter lifetime.
fn shorten<'s>(d: Def<&'static str>) -> Def<&'s str> {
    d
}

/// The whole prelude: the guards, then every kept builtin rewritten, then the program.
fn prelude<'s>(program: T<'s>) -> Vec<Def<&'s str>> {
    let wrappers = load::parse(WRAPPERS, |p| p.defs()).expect("the wrappers parse");
    let dropped = |d: &Def<&str>| {
        let id = format!("{}/{}", d.name, d.args.len());
        DROPPED_DEFS.contains(&id.as_str())
    };
    let builtins = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs())
        .filter(|d| !dropped(d))
        .map(rewrite_def);
    let mut all: Vec<Def<&'s str>> = Vec::new();
    all.extend(wrappers.into_iter().map(shorten));
    all.extend(builtins.map(shorten));
    all.push(Def {
        name: MAIN,
        args: Vec::new(),
        body: rewrite(program),
    });
    all
}

// --- running -------------------------------------------------------------------------------

/// Describe why a program fails to parse, from jaq's loader errors.
fn describe_load_errors(errors: load::Errors<&str, ()>) -> String {
    let mut out = Vec::new();
    for (_file, e) in errors {
        match e {
            load::Error::Io(v) => out.extend(v.into_iter().map(|(p, m)| format!("{p}: {m}"))),
            load::Error::Lex(v) => out.extend(
                v.into_iter()
                    .map(|(expect, at)| format!("expected {} at `{}`", expect.as_str(), clip(at))),
            ),
            load::Error::Parse(v) => out.extend(
                v.into_iter()
                    .map(|(expect, at)| format!("expected {} at `{}`", expect.as_str(), clip(at))),
            ),
        }
    }
    out.join("; ")
}

fn clip(s: &str) -> String {
    let s: String = s.chars().take(24).collect();
    s
}

/// Describe a compile error: a name that does not exist here, with the reason when it is one
/// this sandbox leaves out on purpose.
fn describe_compile_errors(errors: jaq_core::compile::Errors<&str, ()>) -> String {
    let mut out = Vec::new();
    for (_file, list) in errors {
        for (name, undefined) in list {
            let what = match undefined {
                jaq_core::compile::Undefined::Filter(arity) => {
                    let id = format!("{name}/{arity}");
                    let reason = EXCLUDED
                        .iter()
                        .find(|(x, _)| *x == id)
                        .map(|(_, why)| *why)
                        .or_else(|| {
                            DROPPED_DEFS
                                .contains(&id.as_str())
                                .then_some("it reaches something this sandbox leaves out")
                        })
                        .or_else(|| {
                            matches!(
                                name,
                                "input" | "inputs" | "input_filename" | "get_search_list"
                            )
                            .then_some("a query reads exactly one document, its `src`")
                        });
                    match reason {
                        Some(why) => format!("`{id}` is not available here: {why}"),
                        None => format!("`{id}` is not defined"),
                    }
                }
                jaq_core::compile::Undefined::Var => match name {
                    "$ENV" => {
                        "`$ENV` is not available here: it reads the host's environment".to_string()
                    }
                    "$__loc__" => "`$__loc__` is not available here".to_string(),
                    _ => format!("the variable `{name}` is not defined"),
                },
                other => format!("`{name}` is not defined ({other:?})"),
            };
            out.push(what);
        }
    }
    out.join("; ")
}

/// A writer that refuses past `limit` bytes, so serializing a value costs at most the limit.
struct Bounded {
    buf: String,
    limit: usize,
}

impl std::fmt::Write for Bounded {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.buf.len() + s.len() > self.limit {
            return Err(std::fmt::Error);
        }
        self.buf.push_str(s);
        Ok(())
    }
}

/// Format `v` with jaq's own `Display`, or `None` past `limit` bytes.
pub(crate) fn serialize(v: &dyn std::fmt::Display, limit: usize) -> Option<String> {
    let mut w = Bounded {
        buf: String::new(),
        limit,
    };
    write!(w, "{v}").ok().map(|()| w.buf)
}

/// Format `v` for a person, clipped to `limit` bytes.
fn clipped(v: &dyn std::fmt::Display, limit: usize) -> String {
    serialize(v, limit).unwrap_or_else(|| {
        let mut w = Bounded {
            buf: String::new(),
            limit,
        };
        let _ = write!(w, "{v}");
        w.buf.push_str(" …");
        w.buf
    })
}

/// One result of a run: its compact JSON text, and the raw text of a string result (what
/// `as=text/plain` answers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The value as compact JSON.
    pub json: String,
    /// `Some` when the value is a scalar: a string's own text, or a number's, boolean's or
    /// null's JSON spelling.
    pub scalar_text: Option<String>,
}

/// Run `program` over the JSON document `input` under `limits`, returning every result in
/// order. On wasm, use [`run_with_clock`] to enforce the wall-clock budget as well as fuel.
///
/// ```
/// use ikigai_json::sandbox::{run, Limits};
///
/// let out = run(".items[] | select(.n > 1) | .name", br#"{"items":[{"name":"a","n":1},{"name":"b","n":2}]}"#,
///     &Limits::default()).unwrap();
/// assert_eq!(out.len(), 1);
/// assert_eq!(out[0].json, "\"b\"");
/// assert_eq!(out[0].scalar_text.as_deref(), Some("b"));
/// ```
pub fn run(program: &str, input: &[u8], limits: &Limits) -> Result<Vec<Output>, Refusal> {
    let mut outputs = Vec::new();
    run_each(program, input, limits, limits.max_output_bytes, |output| {
        outputs.push(output);
        true
    })?;
    Ok(outputs)
}

/// Run with a host clock, including on wasm. A clock returning `None` leaves
/// native calls using their monotonic fallback and wasm calls bounded by fuel.
pub fn run_with_clock(
    program: &str,
    input: &[u8],
    limits: &Limits,
    now: &(dyn Fn() -> Option<ikigai_core::Time> + Sync),
) -> Result<Vec<Output>, Refusal> {
    let mut outputs = Vec::new();
    run_each_clock(
        program,
        input,
        limits,
        limits.max_output_bytes,
        Some(now),
        |output| {
            outputs.push(output);
            true
        },
    )?;
    Ok(outputs)
}

// Returning false cancels evaluation, including when the body consumer has gone away.
pub(crate) fn run_each(
    program: &str,
    input: &[u8],
    limits: &Limits,
    max_value_bytes: usize,
    emit: impl FnMut(Output) -> bool + Send,
) -> Result<(), Refusal> {
    run_each_clock(program, input, limits, max_value_bytes, None, emit)
}

fn run_each_clock(
    program: &str,
    input: &[u8],
    limits: &Limits,
    max_value_bytes: usize,
    now: Option<&(dyn Fn() -> Option<ikigai_core::Time> + Sync)>,
    emit: impl FnMut(Output) -> bool + Send,
) -> Result<(), Refusal> {
    if program.len() > limits.max_program_bytes {
        return Err(Refusal::Bound {
            bound: "program",
            detail: format!(
                "the program is {} bytes, more than the {} allowed",
                program.len(),
                limits.max_program_bytes
            ),
        });
    }
    if input.len() > limits.max_input_bytes {
        return Err(Refusal::Input(format!(
            "the document is {} bytes, more than the {} allowed",
            input.len(),
            limits.max_input_bytes
        )));
    }
    let nesting = crate::scan::nesting(input, crate::MAX_JSON_NESTING);
    if nesting > crate::MAX_JSON_NESTING {
        return Err(Refusal::Input(format!(
            "the document nests arrays and objects deeper than {} (MAX_JSON_NESTING)",
            crate::MAX_JSON_NESTING
        )));
    }
    let limits = *limits;
    on_eval_stack(move || evaluate(program, input, limits, max_value_bytes, now, emit))
}

/// Run `work` on a thread of [`EVAL_STACK`] (native) and turn a bound's unwind into its
/// refusal. Any other panic is a bug and is resumed as one.
fn on_eval_stack<F>(work: F) -> Result<(), Refusal>
where
    F: FnOnce() -> Result<(), Refusal> + Send,
{
    let guarded = move || match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(payload) => match payload.downcast::<Exhausted>() {
            Ok(exhausted) => Err(exhausted.0),
            Err(other) => std::panic::resume_unwind(other),
        },
    };
    #[cfg(not(target_family = "wasm"))]
    {
        std::thread::scope(|scope| {
            let handle = std::thread::Builder::new()
                .name("ikigai-json-jq".to_string())
                .stack_size(EVAL_STACK)
                .spawn_scoped(scope, guarded)
                .map_err(|e| {
                    Refusal::Timeout(format!(
                        "could not start the thread a jq program runs on: {e}"
                    ))
                })?;
            match handle.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
    }
    #[cfg(target_family = "wasm")]
    {
        guarded()
    }
}

fn evaluate(
    program: &str,
    input: &[u8],
    limits: Limits,
    max_value_bytes: usize,
    now: Option<&(dyn Fn() -> Option<ikigai_core::Time> + Sync)>,
    mut emit: impl FnMut(Output) -> bool,
) -> Result<(), Refusal> {
    let meter = Meter::new(limits, now);
    // Parse once through the loader for its error messages (it also refuses `import` and
    // `include`, since no module can be read), then once more for the tree to rewrite.
    let arena = Arena::default();
    Loader::new([])
        .load(
            &arena,
            File {
                code: program,
                path: (),
            },
        )
        .map_err(|e| Refusal::Program(describe_load_errors(e)))?;
    let term = load::parse(program, |p| p.term())
        .ok_or_else(|| Refusal::Program("the program does not parse as a single filter".into()))?;

    let arena = Arena::default();
    let modules = Loader::new(prelude(term))
        .load(
            &arena,
            File {
                code: MAIN,
                path: (),
            },
        )
        .map_err(|e| Refusal::Program(describe_load_errors(e)))?;
    let filter = Compiler::default()
        .with_funs(natives())
        .compile(modules)
        .map_err(|e| Refusal::Program(describe_compile_errors(e)))?;

    let input = jaq_json::read::parse_single(input)
        .map_err(|e| Refusal::Input(format!("not a JSON document: {e}")))?;
    meter.charge(0);

    let ctx = Ctx::<Sandbox>::new(
        Metered {
            lut: &filter.lut,
            meter: &meter,
        },
        Vars::new([]),
    );
    let mut total = 0usize;
    for result in filter.id.run((ctx, input)) {
        let v = match jaq_core::unwrap_valr(result) {
            Ok(v) => v,
            Err(e) => return Err(Refusal::Runtime(clipped(&e, 1024))),
        };
        let left = limits
            .max_output_bytes
            .saturating_sub(total)
            .min(max_value_bytes);
        walk(&meter, &v, "a result", Some((left as u64, "output")));
        let Some(json) = serialize(&v, left) else {
            exhaust(
                "output",
                format!(
                    "the results serialize to more than {} bytes",
                    limits.max_output_bytes
                ),
            );
        };
        total += json.len() + 1;
        let scalar_text = match &v {
            Val::TStr(s) | Val::BStr(s) => Some(String::from_utf8_lossy(s).into_owned()),
            Val::Arr(_) | Val::Obj(_) => None,
            _ => Some(json.clone()),
        };
        if !emit(Output { json, scalar_text }) {
            break;
        }
    }
    Ok(())
}

/// Every native jaq offers, as `name/arity` — for the classification test.
#[doc(hidden)]
pub fn upstream_natives() -> Vec<String> {
    jaq_core::funs::<Sandbox>()
        .chain(jaq_std::funs::<Sandbox>())
        .chain(jaq_json::funs::<Sandbox>())
        .map(|(name, args, _): Filter<Native<Sandbox>>| format!("{name}/{}", args.len()))
        .collect()
}

/// Every jq-defined builtin jaq offers, as `name/arity` — for the classification test.
#[doc(hidden)]
pub fn upstream_defs() -> Vec<String> {
    jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs())
        .map(|d| format!("{}/{}", d.name, d.args.len()))
        .collect()
}

#[cfg(test)]
mod stream_tests {
    #[test]
    fn stopping_the_consumer_stops_before_the_next_jq_result() {
        let mut seen = 0;
        super::run_each(
            "0, error(\"must not run\")",
            b"null",
            &super::Limits::default(),
            100,
            |_| {
                seen += 1;
                false
            },
        )
        .unwrap();
        assert_eq!(seen, 1);
    }
}
