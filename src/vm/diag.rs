//! Structured diagnostics for blocks whose recovery did not close.
//!
//! A failure leaves almost nothing behind. [Terminator::Unresolved] keeps a sentence, and
//! the recovered graph keeps no trace of the expression that produced the branch, the
//! candidate predicates that were tried, or what each of them folded to. That is enough to
//! count failures and to bucket them by wording, and nothing more: in particular it cannot
//! answer whether the blocks that fail share a dispatch shape, which is the question that
//! decides whether recovery can be taught to cover them.
//!
//! This module keeps those facts. Recovery records them at the failure site, where the
//! arena and the emulator state still exist, and the command layer writes them out beside
//! the final graph's block list.
//!
//! Collection is opt-in through [begin]. With it off every hook is a single thread-local
//! read, so an ordinary recovery pays nothing.

/// One pinning of one candidate predicate.
#[derive(Clone, Debug, Default)]
pub struct PinArm {
    /// The value this arm pinned.
    pub value: u64,
    /// What the branch folded to with this pin forced into effect, overriding any pin the
    /// state already carried for the same candidate.
    pub target: Option<u64>,
    /// Whether the target is inside an executable section.
    pub executable: bool,
    /// Whether the target falls inside the VM bytecode section.
    pub in_vm_section: bool,
    /// Block identity the state would resume at.
    pub vip: Option<u64>,
    /// What appending the pin instead of forcing it would have folded to. It differs from
    /// the target only when an older pin for the same candidate was already in force: pins
    /// are searched front to back and substitution takes the first match, so an appended
    /// duplicate is shadowed rather than overriding. Recorded because a probe that read
    /// the shadowed value would report this value's target as the other value's.
    pub naive_target: Option<u64>,
}

/// A symbol the splitter considered pinning as a branch predicate.
#[derive(Clone, Debug, Default)]
pub struct FlagProbe {
    /// The candidate expression, rendered.
    pub expr: String,
    /// Value the state had already pinned this candidate to, if any.
    pub prior_pin: Option<u64>,
    /// One entry per value tried, in the order they were tried.
    pub arms: Vec<PinArm>,
}

/// One value of a narrow index the splitter enumerated.
#[derive(Clone, Debug, Default)]
pub struct SwitchArm {
    pub value: u64,
    pub target: Option<u64>,
    pub executable: bool,
    pub in_vm_section: bool,
    pub vip: Option<u64>,
}

/// A subexpression whose provable range was narrow enough to enumerate.
#[derive(Clone, Debug, Default)]
pub struct SwitchProbe {
    pub expr: String,
    /// Largest value the subexpression can take, from its known-zero bits.
    pub span: u64,
    /// One entry per value in the inclusive range `0..=span`, so a rejected enumeration can
    /// be read off in full rather than inferred from the fact that it was rejected.
    pub arms: Vec<SwitchArm>,
    /// Set when some value did not fold to a constant at all. Enumeration is then not a
    /// complete set of outcomes, which is why it was rejected.
    pub unfolded: bool,
    /// How many values folded to an address outside executable memory.
    pub non_executable_arms: usize,
}

/// A virtualized branch that no split could resolve.
#[derive(Clone, Debug, Default)]
pub struct SplitDiag {
    pub entry: u64,
    /// Recovery pass that produced this record. Pass 2 is the one whose graph is returned
    /// when the first pass found joins; records from both are kept so a block that only
    /// fails once the state is symbolized can be told from one that always fails.
    pub pass: u32,
    pub handler: u64,
    pub vip: Option<u64>,
    /// Address of the branch instruction that could not be resolved.
    pub site: u64,
    pub dest_expr: String,
    pub dest_width: String,
    /// Whether the destination expression reaches a value only the caller can supply. Such
    /// a branch is not something more folding could ever close.
    pub arg_dependent: bool,
    pub leaves: Vec<String>,
    pub leaf_total: usize,
    pub flag_candidates: Vec<FlagProbe>,
    pub narrow_indices: Vec<SwitchProbe>,
}

/// A block that ended unresolved for a reason other than an unsplittable branch, or for
/// which no split was attempted at all.
#[derive(Clone, Debug, Default)]
pub struct StopDiag {
    pub entry: u64,
    pub pass: u32,
    pub handler: u64,
    pub vip: Option<u64>,
    /// Name of the Stop variant, or none when recovery ended the block without one.
    /// The reason text and the block cost are not repeated here: the graph already
    /// carries both, and a second copy could drift from it.
    pub stop: String,
    pub site: u64,
}

/// How the expression DAG looked at the moment folding gave up.
///
/// The stop carries a node count, which says growth ran away but not what grew.
/// Without the composition the only answers available are guesses, and a trimming
/// rule designed against a guess is not something that can be argued for.
///
/// Kept separate from the branch records on purpose: this describes the folding
/// budget, and folding it into the dispatch-enumeration records would make one change
/// out of two unrelated mechanisms.
#[derive(Clone, Debug, Default)]
pub struct DivergedDiag {
    pub entry: u64,
    pub pass: u32,
    pub handler: u64,
    pub vip: Option<u64>,
    pub site: u64,
    /// Node count at the moment the limit was crossed.
    pub nodes: usize,
    /// Arena size as the composition saw it; equal to `nodes` unless the arena grew
    /// between the check and this walk.
    pub dag_total: usize,
    /// Node count per kind, largest first.
    pub by_kind: Vec<(String, usize)>,
    /// Node count per bit width, largest first.
    pub by_width: Vec<(String, usize)>,
    /// Total parent edges: `total - 1` is a tree, much more means reuse.
    pub edges: usize,
    /// Nodes with two or more parents.
    pub shared: usize,
    pub max_indegree: usize,
    /// The most-referenced subtrees, rendered shallowly.
    pub top_shared: Vec<(String, usize)>,
    /// Nodes reachable from every `Ref` retained by machine state, active pins, and the
    /// guest-visible events of this block. Whatever the arena holds beyond that cannot
    /// affect any computed value through those owners. Arena compaction would still need
    /// to remap all owners and clear or rebuild caches keyed by old references.
    pub live_dag: usize,
}

/// Everything one function recorded.
#[derive(Default)]
pub struct Sink {
    /// Recovery pass that produced the CFG returned to the caller.
    pub final_pass: u32,
    pub splits: Vec<SplitDiag>,
    pub stops: Vec<StopDiag>,
    pub diverged: Vec<DivergedDiag>,
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    on: bool,
    entry: u64,
    pass: u32,
    excluded_time: std::time::Duration,
}

thread_local! {
    static CTX: std::cell::RefCell<Ctx> = std::cell::RefCell::new(Ctx::default());
    static SINK: std::cell::RefCell<Sink> = std::cell::RefCell::new(Sink::default());
}

/// Start collecting for the given entry, discarding anything a previous function left
/// behind.
pub fn begin(entry: u64) {
    CTX.with(|c| {
        *c.borrow_mut() = Ctx {
            on: true,
            entry,
            pass: 0,
            excluded_time: std::time::Duration::ZERO,
        }
    });
    SINK.with(|s| {
        *s.borrow_mut() = Sink::default();
    });
}

/// Label subsequent records with the recovery pass that produced them.
pub fn set_pass(pass: u32) {
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.pass = pass;
        c.excluded_time = std::time::Duration::ZERO;
    });
}

pub fn is_on() -> bool {
    CTX.with(|c| c.borrow().on)
}

/// Run diagnostic-only work without charging it to Explorer's wall-clock budget.
///
/// Diagnostics must observe the same recovery that an ordinary run would produce. Large
/// DAG walks and candidate probes can take long enough to push later blocks past the
/// deadline, so their elapsed time is tracked separately and subtracted at budget checks.
pub fn measure<T>(f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let value = f();
    let elapsed = started.elapsed();
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        if c.on {
            c.excluded_time += elapsed;
        }
    });
    value
}

pub fn excluded_time() -> std::time::Duration {
    CTX.with(|c| {
        let c = c.borrow();
        if c.on {
            c.excluded_time
        } else {
            std::time::Duration::ZERO
        }
    })
}

pub fn record_split(mut d: SplitDiag) {
    if !fill(&mut d.entry, &mut d.pass) {
        return;
    }
    SINK.with(|s| s.borrow_mut().splits.push(d));
}

pub fn record_stop(mut d: StopDiag) {
    if !fill(&mut d.entry, &mut d.pass) {
        return;
    }
    SINK.with(|s| s.borrow_mut().stops.push(d));
}

pub fn record_diverged(mut d: DivergedDiag) {
    if !fill(&mut d.entry, &mut d.pass) {
        return;
    }
    SINK.with(|s| s.borrow_mut().diverged.push(d));
}

/// Stamp a record with the current function and pass. False when collection is off, which
/// is what makes every hook free in a normal run.
fn fill(entry: &mut u64, pass: &mut u32) -> bool {
    CTX.with(|c| {
        let c = c.borrow();
        if !c.on {
            return false;
        }
        *entry = c.entry;
        *pass = c.pass;
        true
    })
}

/// Stop collecting and hand back everything recorded.
///
/// Collection stays off until the next [begin], so a caller that never drains the sink
/// cannot leak records into the following function.
pub fn take() -> Sink {
    let final_pass = CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.on = false;
        c.pass
    });
    SINK.with(|s| {
        let mut sink = std::mem::take(&mut *s.borrow_mut());
        sink.final_pass = final_pass;
        sink
    })
}

/// Escape a string for embedding in JSON.
pub fn escape(s: &str) -> String {
    const BACKSLASH: char = 92u8 as char;
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => {
                out.push(BACKSLASH);
                out.push('"');
            }
            BACKSLASH => {
                out.push(BACKSLASH);
                out.push(BACKSLASH);
            }
            '\n' => {
                out.push(BACKSLASH);
                out.push('n');
            }
            '\r' => {
                out.push(BACKSLASH);
                out.push('r');
            }
            '\t' => {
                out.push(BACKSLASH);
                out.push('t');
            }
            c if (c as u32) < 0x20 => {
                out.push(BACKSLASH);
                out.push('u');
                out.push_str(&format!("{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}
