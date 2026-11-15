// Copyright 2026 The fxdax Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Trace validation of the journal (TASK-444.1): sequences of the model test, power cuts and
//! remounts included, traced as their journal goes (fxfs's `journal_trace` feature), each a TLA+
//! module, `TraceJournalData`, that specs/journal/trace/TraceJournal.tla matches against
//! specs/journal: TLC follows the behaviours the trace allows, and the run is one of the spec's if
//! one reaches the trace's end (scripts/check-tla.sh, `journal/traces`).
//!
//! The journal records its steps in its own terms, byte offsets and object IDs; this maps them to
//! the spec's: its transactions are those of tracked objects, numbered from 1 in the order they
//! were written, and an offset in the journal is the number of the first transaction at or after
//! it. A transaction is durable once the journal is durable as far as its end. Each power cut is
//! traced where it fell, at the barrier its crash was recorded at (`Crashes::on_record`), or at an
//! `Op::Crash`'s snapshot, with what the replay of what it left found: the step checks, without
//! changing anything, that it found what the spec says a cut there leaves. A remount's replay is
//! the spec's `Recover`, from which the sequence goes on.
//!
//! JOURNAL_TRACES names the directory the traces are written to, a directory each: `run-NNNN`, or
//! `mutant-NNNN` for those of a mutant that reads the objects' offsets after write_super_block's
//! flush (TASK-149.5's bug), which the spec must reject.

use super::*;
use fxfs::object_store::journal::trace::{Event, JournalTrace, ObjectOffsets};
use proptest::strategy::ValueTree as _;
use std::cell::RefCell;

/// What a mount's journal is traced as.
#[derive(Clone, Default)]
pub(super) enum Traced {
    #[default]
    Not,
    /// The sequence's own, which its remounts go on with.
    Sequence(Rc<SequenceTrace>),
    /// A replay's of a power cut, which records what the replay found.
    Replay(Arc<JournalTrace>),
}

impl Traced {
    /// Where a mount's journal records its steps.
    pub(super) fn journal(&self) -> Option<Arc<JournalTrace>> {
        match self {
            Traced::Not => None,
            Traced::Sequence(sequence) => Some(sequence.journal.clone()),
            Traced::Replay(replay) => Some(replay.clone()),
        }
    }

    /// What a remount of this is traced as: the sequence goes on, and a cut's replay has been
    /// recorded.
    pub(super) fn remounted(&self) -> Traced {
        match self {
            Traced::Sequence(_) => self.clone(),
            _ => Traced::Not,
        }
    }

    /// Traces each crash `crashes` records as a power cut where it falls.
    pub(super) fn record_crashes(&self, crashes: &Crashes) {
        if let Traced::Sequence(sequence) = self {
            let (journal, recorded) = (sequence.journal.clone(), sequence.recorded.clone());
            crashes.on_record(move || recorded.lock().unwrap().push(journal.cut()));
        }
    }

    /// The power is cut here, as a snapshot of the device does.
    pub(super) fn cut(&self) -> Option<Cut> {
        match self {
            Traced::Sequence(sequence) => Some(Cut::new(sequence, sequence.journal.cut())),
            _ => None,
        }
    }

    /// The `index`-th crash recorded (`record_crashes`), in the order `Crashes::take` yields them.
    pub(super) fn recorded_cut(&self, index: usize) -> Option<Cut> {
        match self {
            Traced::Sequence(sequence) => {
                let cut = sequence.recorded.lock().unwrap()[index];
                Some(Cut::new(sequence, cut))
            }
            _ => None,
        }
    }
}

/// A power cut of a traced sequence, whose replay records what it found.
pub(super) struct Cut {
    sequence: Rc<SequenceTrace>,
    index: usize,
    replay: Arc<JournalTrace>,
}

impl Cut {
    fn new(sequence: &Rc<SequenceTrace>, index: usize) -> Self {
        Self { sequence: sequence.clone(), index, replay: Arc::new(JournalTrace::new()) }
    }

    /// What the mount of what the cut left is traced as.
    pub(super) fn replay(&self) -> Traced {
        Traced::Replay(self.replay.clone())
    }

    /// The mount replayed it: keeps what the replay found.
    pub(super) fn replayed(self) -> Result<(), String> {
        let events = self.replay.events();
        let replayed = events.into_iter().find(|event| matches!(event, Event::Replayed { .. }));
        let replayed = replayed.ok_or("the replay of a cut traced no replay")?;
        self.sequence.replays.borrow_mut().insert(self.index, replayed);
        Ok(())
    }
}

/// A traced sequence's journal: its steps, and what each power cut's replay found.
pub(super) struct SequenceTrace {
    journal: Arc<JournalTrace>,
    /// The cuts the crashes recorded are, by their indices among the trace's cuts, in the order
    /// they were recorded.
    recorded: Arc<Mutex<Vec<usize>>>,
    /// What the replay of each cut found, by its index.
    replays: RefCell<BTreeMap<usize, Event>>,
}

impl SequenceTrace {
    pub(super) fn new(journal: JournalTrace) -> Rc<Self> {
        Rc::new(Self {
            journal: Arc::new(journal),
            recorded: Arc::default(),
            replays: RefCell::default(),
        })
    }

    /// That super-blocks were written one at a time: each padded, flushed and saved before the
    /// next padded, as TLC checks too, but in moments (TASK-444.1.1). A remount's journal is a new
    /// one.
    pub(super) fn super_blocks_one_at_a_time(&self) -> Result<(), String> {
        let mut writing = None;
        for (step, event) in self.journal.events().iter().enumerate() {
            match event {
                Event::Pad { checkpoint } => {
                    if let Some(other) = writing.replace(*checkpoint) {
                        return Err(format!(
                            "step {step}: a super-block at {checkpoint} begun as one at {other} was \
                             being written"
                        ));
                    }
                }
                Event::Save { .. } | Event::Replayed { .. } => writing = None,
                _ => {}
            }
        }
        Ok(())
    }

    /// The trace as the TLA+ module `TraceJournalData`, which `about` describes.
    pub(super) fn module(&self, about: &str) -> String {
        let mut spec = Mapped::default();
        let replays = self.replays.borrow();
        let mut cuts = 0;
        for event in self.journal.events() {
            match event {
                Event::Cut => {
                    if let Some(replayed) = replays.get(&cuts) {
                        spec.replayed(replayed, false);
                    }
                    cuts += 1;
                }
                event => spec.step(&event),
            }
        }
        let objects: Vec<_> = spec.objects.iter().map(|&object| name(object)).collect();
        format!(
            "---- MODULE TraceJournalData ----\n\
             \\* {about}: a sequence of the model test (crates/fxfs-posix), traced, generated.\n\
             TraceObjects == {{{}}}\n\
             TraceMaxLen == {}\n\
             Trace == <<\n    {}\n>>\n\
             ====\n",
            objects.join(", "),
            spec.max_len,
            spec.steps.join(",\n    ")
        )
    }
}

/// The spec's terms for a trace, as it is mapped step by step.
#[derive(Default)]
struct Mapped {
    /// The spec's transactions so far, as the journal's offsets each starts and ends at.
    transactions: Vec<(u64, u64)>,
    /// How many of them are durable, as the last step said.
    durable: usize,
    /// The most there were.
    max_len: usize,
    /// The objects named.
    objects: BTreeSet<u64>,
    steps: Vec<String>,
}

impl Mapped {
    /// The spec's offset of the journal's `offset`: the number of the first transaction at or after
    /// it.
    fn at(&self, offset: u64) -> usize {
        1 + self.transactions.partition_point(|&(start, _)| start < offset)
    }

    /// How many transactions end by the journal's `offset`.
    fn ended_by(&self, offset: u64) -> usize {
        self.transactions.partition_point(|&(_, end)| end <= offset)
    }

    /// `offsets` in the spec's terms: a set of <<object, old, cur>>, None 0.
    fn offsets(&mut self, offsets: &[ObjectOffsets]) -> String {
        let tuples: Vec<_> = offsets
            .iter()
            .map(|offsets| {
                self.objects.insert(offsets.object_id);
                let at = |offset: Option<u64>| offset.map_or(0, |offset| self.at(offset));
                format!("<<{}, {}, {}>>", name(offsets.object_id), at(offsets.old), at(offsets.cur))
            })
            .collect();
        format!("{{{}}}", tuples.join(", "))
    }

    /// A super-block's `offsets` in the spec's terms: a set of <<object, offset>>.
    fn super_block_offsets(&mut self, offsets: &[(u64, u64)]) -> String {
        let pairs: Vec<_> = offsets
            .iter()
            .map(|&(object, offset)| {
                self.objects.insert(object);
                format!("<<{}, {}>>", name(object), self.at(offset))
            })
            .collect();
        format!("{{{}}}", pairs.join(", "))
    }

    fn object(&mut self, object: Option<u64>) -> String {
        match object {
            Some(object) => {
                self.objects.insert(object);
                name(object)
            }
            None => "\"-\"".to_owned(),
        }
    }

    fn step(&mut self, event: &Event) {
        let step = match event {
            Event::Commit { start, end, mutated, begin, end_flush, offsets } => {
                self.transactions.push((*start, *end));
                self.max_len = self.max_len.max(self.transactions.len());
                let mutated: Vec<_> = mutated.iter().map(|&object| self.object(Some(object))).collect();
                let (begin, end_flush) = (self.object(*begin), self.object(*end_flush));
                let after = self.offsets(offsets);
                format!(
                    "[a |-> \"Commit\", m |-> {{{}}}, b |-> {begin}, e |-> {end_flush}, after |-> {after}]",
                    mutated.join(", ")
                )
            }
            Event::Pad { checkpoint } => format!("[a |-> \"Pad\", c |-> {}]", self.at(*checkpoint)),
            Event::Flush { durable } => {
                self.durable = self.ended_by(*durable);
                format!("[a |-> \"Flush\", d |-> {}]", self.durable)
            }
            Event::Durable { durable } => {
                let durable = self.ended_by(*durable);
                if durable <= self.durable {
                    return;
                }
                self.durable = durable;
                format!("[a |-> \"DeviceFlush\", d |-> {durable}]")
            }
            Event::Save { checkpoint, offsets } => {
                let offsets = self.super_block_offsets(offsets);
                format!("[a |-> \"Save\", c |-> {}, off |-> {offsets}]", self.at(*checkpoint))
            }
            replayed @ Event::Replayed { .. } => return self.replayed(replayed, true),
            Event::Cut => unreachable!("cuts are mapped with their replays"),
            Event::Unsupported(what) => format!("[a |-> \"Unsupported\", what |-> \"{what}\"]"),
        };
        self.steps.push(step);
    }

    /// A replay found `replayed`: the sequence's own, at a remount, from which it goes on, if
    /// `recovers`, or a power cut's.
    fn replayed(&mut self, replayed: &Event, recovers: bool) {
        let Event::Replayed { checkpoint, offsets, end, offsets_after } = replayed else {
            unreachable!("not a replay: {replayed:?}")
        };
        let durable = self.ended_by(*end);
        let (checkpoint, offsets) = (self.at(*checkpoint), self.super_block_offsets(offsets));
        let after = self.offsets(offsets_after);
        let action = if recovers { "Recover" } else { "Cut" };
        self.steps.push(format!(
            "[a |-> \"{action}\", d |-> {durable}, c |-> {checkpoint}, off |-> {offsets}, after |-> {after}]"
        ));
        if recovers {
            self.transactions.truncate(durable);
            self.durable = durable;
        }
    }
}

/// An object's name in the spec.
fn name(object: u64) -> String {
    format!("\"o{object}\"")
}

/// Writes the trace of `ops`, run as `setting` says, into `dir`, named `name`, if given: the
/// sequence's trace, which must hold, unless `mutant`, whose run may fail, as its bug may lose
/// what the model test then finds lost.
fn traced(setting: Setting, ops: Vec<Op>, mutant: bool, dir: Option<&std::path::Path>, name: &str) {
    let journal =
        if mutant { JournalTrace::of_offsets_read_after_flush() } else { JournalTrace::new() };
    let sequence = SequenceTrace::new(journal);
    let done = setting.run_traced(ops.clone(), Rc::default(), Traced::Sequence(sequence.clone()));
    if !mutant {
        assert_eq!(done, Ok(()), "{setting:?}, {ops:?}");
        assert_eq!(sequence.super_blocks_one_at_a_time(), Ok(()), "{setting:?}, {ops:?}");
    }
    let module = sequence.module(&format!("{name}: {setting:?}, {ops:?}"));
    assert!(module.contains("\"Save\""), "{name} saved no super-block");
    if let Some(dir) = dir {
        let run = dir.join(name);
        std::fs::create_dir_all(&run).expect("a directory for the trace");
        std::fs::write(run.join("TraceJournalData.tla"), module).expect("writing the trace");
    }
}

/// The sequences traced: the pinned ones that reach the journal's corners (a store's flush around
/// a super-block, compactions racing, crashes and remounts), every cut of a few, and a few drawn at
/// random, cut now and then, remounting and compacting among their operations.
fn sequences() -> Vec<(Setting, Vec<Op>)> {
    let around_a_super_block = vec![
        Op::Write { file: 10, at: 29721, len: 1698, byte: 234 },
        Op::Create(3),
        Op::Compact,
        Op::Fill { file: 5, page: 8, pages: 5, stride: 2, byte: 114 },
        Op::FillWhileCompacting { file: 7, page: 7, pages: 13, byte: 45, flushing: true },
    ];
    let racing = vec![
        Op::Compact,
        Op::Create(0),
        Op::FillWhileCompacting { file: 0, page: 0, pages: 11, byte: 0, flushing: false },
    ];
    let crashes_and_remounts = vec![
        Op::Create(0),
        Op::Write { file: 0, at: 0, len: 1, byte: 1 },
        Op::Clone { from: 0, to: 0, page: 0, pages: 1, to_page: 1 },
        Op::Compact,
        Op::Crash,
        Op::Unlink(0),
        Op::Remount,
        Op::Write { file: 0, at: 4096, len: 8192, byte: 2 },
        Op::Sync,
        Op::Compact,
        Op::Remount,
    ];
    let mut sequences = vec![
        (Setting::awkward(5713515047801504255, ROOMY).cutting_everywhere(), around_a_super_block),
        (Setting::awkward(1290169520417531043, ROOMY).cutting_everywhere(), racing),
        (Setting::awkward(0, ROOMY), crashes_and_remounts),
    ];
    // Drawn as the model test draws them, from a fixed seed, so that the traces stay the same.
    let config = Config { rng_seed: RngSeed::Fixed(444), ..config(1) };
    let mut runner = TestRunner::new(config);
    let drawn = (any::<u64>(), proptest::collection::vec(op(), 8..24));
    for _ in 0..RANDOM_TRACES {
        let (seed, ops) = drawn.new_tree(&mut runner).expect("a sequence").current();
        sequences.push((Setting::awkward(seed, ROOMY).with_graveyard_of_seed(), ops));
    }
    let short = (any::<u64>(), proptest::collection::vec(short_op(), 1..=SHORT));
    for _ in 0..SHORT_TRACES {
        let (seed, ops) = short.new_tree(&mut runner).expect("a sequence").current();
        sequences.push((Setting::awkward(seed, ROOMY).cutting_everywhere(), ops));
    }
    sequences
}

/// The sequences drawn at random that are traced, and the short ones cut everywhere.
const RANDOM_TRACES: usize = 6;
const SHORT_TRACES: usize = 4;

/// Traces the sequences, and the mutant's runs of the one that reaches its bug, into
/// JOURNAL_TRACES if it names a directory, for scripts/check-tla.sh to check against the spec.
/// Without it, the traces are made, and dropped: what makes them works.
#[test]
fn journal_traces_follow_the_spec() {
    let dir = std::env::var_os("JOURNAL_TRACES").map(std::path::PathBuf::from);
    let sequences = sequences();
    for (index, (setting, ops)) in sequences.iter().enumerate() {
        traced(*setting, ops.clone(), false, dir.as_deref(), &format!("run-{index:04}"));
    }
    let (setting, ops) = sequences[0].clone();
    traced(setting, ops, true, dir.as_deref(), "mutant-0000");
}

/// Format writes its two super-blocks with no compaction's between them: the flush task, already
/// running, compacted a journal of a small reclaim size meanwhile, and its super-block came between
/// format's, the older saved last (TASK-444.1.1). Found by the trace validation, whose seed this
/// keeps, as when the device answers decides it.
#[test]
fn format_writes_its_super_blocks_alone() {
    let setting = Setting::awkward(16811558461824683114, ROOMY).cutting_everywhere();
    let sequence = SequenceTrace::new(JournalTrace::new());
    let ops = vec![Op::Mkdir(2), Op::Open(9)];
    let traced = Traced::Sequence(sequence.clone());
    assert_eq!(setting.run_traced(ops, Rc::default(), traced), Ok(()));
    assert_eq!(sequence.super_blocks_one_at_a_time(), Ok(()));
}
