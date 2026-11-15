---
id: doc-17
title: >-
  Spending frontier tokens on quality: what the forefront does, and what fits
  fxdax
type: guide
created_date: '2026-11-15 12:32'
updated_date: '2026-11-15 12:34'
---
Web research of 2026-11-15 (four parallel research agents, ~200 searches and fetches), checked
against main (36f194bbc). The ranked list in §2 is what to do; §1 is the evidence; §3 what not to
do; §4 licences. Filed as TASK-444 and its subtasks.

## The one rule under all of it

Every frontier result that worked pairs a model that *proposes* with something outside the model
that *disposes*: ASan, a model checker, a fuzzer, a mutant that a test must kill, a second
implementation. Model-only judgement fails in measured ways: Sean Heelan's o3 run had a
signal-to-noise near 1:50; agents grading their own work approve mediocre work (Anthropic, Mar
2026); AIxCC patches that passed every automated check were 16-60% wrong on manual review;
syzbot's LLM KCSAN assessor called every race benign; independent auditor agents that *edited*
code made results worse (Dan Luu, Sep 2026).

fxdax's comparative advantage is that it already owns cheap, deterministic disposers: guest/sim
seeds under ASan/UBSan, Shuttle schedules, the model test's oracle and power cuts, TLC, cargo-mutants
and guest/mutants. A token-hungry hunt at 1:50 is affordable only behind such a verifier. So every
practice below has the same gate: **no reproducer in one of the project's oracles, no finding**.

## 1. Evidence, by technique

**Code-to-spec conformance.**
- CCF "Smart Casual Verification" (NSDI 2025): trace validation of production C++ against TLA+
  found 6 bugs, one a serious commit-advance bug; now in CI.
- Specula (UIUC, Apache-2.0, arXiv 2607.25333): agents mine invariants (with citations) from code,
  comments, issues and history, write TLA+, instrument the code, trace-validate both ways, and
  replay each violation as a code-level test. 249 bugs in 48 systems (Rust among them); on 5
  systems 62 bugs against 3 for Claude Code given TLA+ tools. Median $57, 3.7 h a system. Can
  import existing models.
- MongoDB (2025): trace-checking the server failed (spec atomic where code takes two steps);
  *generating* tests from a spec's state graph succeeded: 4,913 behaviours as tests, 100% branch
  coverage of the target against AFL's 92%.
- SysMoBench (ICLR 2026): LLMs model "the textbook protocol, not the implementation" (15%
  conformance on etcd raft): models must be checked against code, never trusted.
- fxdax has this for followers only (specs/follower/trace, crates/follower/src/shuttle_trace.rs).
  Bucket is TASK-44.5, To Do. Journal and dax-cow have none.

**Memory ordering.** Shuttle models every atomic as SeqCst (its docs, and it warns at runtime), so
no fxdax test can see an Acquire/Release/Relaxed bug. The host crates (fxdax, physmem,
extent_store, extent_remote, follower, virtio_extent) hold 450 non-SeqCst orderings. LLMs reason
poorly about weak memory (arXiv 2501.14326), so an audit alone is not evidence; loom (MIT) is the
disposer. AtomVChecker's classes (ordering too weak to publish related state; AtomicPtr data
unsynchronised; stronger than needed) give the audit a shape. A fence-deletion mutant left one
project's loom test green: the tests themselves need ordering mutants.

**unsafe Rust.** 557 unsafe sites in the host crates; no Miri. The first Rust-in-Linux CVE
(CVE-2025-68260, Binder) was a `// SAFETY:` claim false under concurrency. SafeDep: ask whether any
sibling method can break an unsafe block's precondition; confirm with Miri. std-unsafe-doc: one
model writes preconditions, an independent one labels each claim Correct/Missing/Incorrect.

**Kernel patch review.** Chris Mason's review-prompts (MIT) run on netdev, BPF CI and inside
Sashiko; its "orc" mode pre-extracts changed functions and call graphs and fans out one agent per
file plus history/Fixes:/syzkaller agents; a 10-step false-positive guide demands a concrete
execution path. Sashiko (Google, Apache-2.0, now Linux Foundation; Claude Code backend
supported): staged review, then verification; LSF/MM 2026 sample of ~1,500 threads ~85% true,
~10% false positives, ~97% accuracy on critical/high. Lessons: run several times and aggregate;
keep a known-issues file so accepted risks stop recurring; reviews are swayed by commit-message
claims; netdev maintainers spend ~50% of their time disproving AI reviews without a reproducer
gate. Mason's `dax.md` is pmem-only (10 rules): fxdax would write its own. Darrick Wong posted
12+ "LLM-inspired bug fixes" series to XFS scrub in 2026 by sweeping whole files for
pre-existing bugs. semcode (Apache-2.0) indexes C and Rust for agents.

**Variant hunts from fix history.** Big Sleep's first real bug came from variant analysis of
recent fixes. KNighter (SOSP 2025, Apache-2.0): an LLM turns fix commits into Clang Static
Analyzer checkers refined against pre- and post-fix code; 92 new kernel bugs, 30 CVEs. An LLM +
Coccinelle study (arXiv 2611.16215): one small rule per variant, deterministic syntax repair,
compile-check or reject. fxdax already puts past bugs back as mutants (guest/mutants,
guest/sim/mutants.sh) and has one Coccinelle rule (scripts/cocci/region-lock.cocci, TASK-44.17).

**Semantic mutants.** Meta ACH (FSE 2025): engineers state fault concerns in prose; the model
writes realistic mutants and tests that kill them; 73% of tests accepted; an LLM equivalent-mutant
filter at ~0.95 precision. Meta JiT catching tests (2026): per-diff "dodgy" variants and tests that
fail on them, ~4x detection over baseline. Antithesis' mutation skill injects race-shaped bugs so
properties are proven able to fail.

**Assumptions as assertions.** Code-Augur (arXiv 2606.18619): for every site the agent judges
safe, write the reason as an in-source assertion and let fuzzers try to break it; 34-157% more
bugs than Claude Code alone. "Self-validation shares the same blind spots." This is AGENTS.md's
own lesson from the graveyard reap ("to close a narrow race, assert the rule"), applied
proactively.

**Hunting at scale.** Anthropic's Mythos runs: score every file 1-5 for bug likelihood, parallel
agents on different high-ranked files, a final agent to confirm; human severity agreed 89% of the
time over 198 reports. Heelan: context by a mechanical rule (handler + callees to depth 3 +
setup/teardown), one bug class per prompt, sample 100 times. Theori's Branch Flipper: show a model
the branch a fuzzer is stuck on plus a seed that reaches it; have it write a *generator*. Every
AIxCC team had the model write input generators, never inputs. KernelGPT/SyzGPT (Apache-2.0):
LLM-written syzkaller descriptions, validate-and-repair; 24 bugs, 11 CVEs.

**Workflow evidence.** Carlini's 16-agent C compiler: "the task verifier [must be] nearly perfect,
otherwise Claude will solve the wrong problem"; terse grep-able test output; a dedup agent.
ImpossibleBench (ICLR 2026): frontier models cheat on contradictory tests, stronger ones more.
Anthropic's guidance: enforce rules with hooks, not CLAUDE.md lines; reviewers read-only, "skip
style", verify before reporting. Dan Luu (160 runs a condition): generic technique prompts ("use
TDD/TLA+/Kani") were neutral or harmful; only 30 of 160 TLA+ models covered the risky parts;
Verus proofs often vacuous; structured fuzz inputs found real bugs half the time they were used.

## 2. Ranked for fxdax

Ranked by expected bugs found per effort, given what fxdax already has. Item N is TASK-444.N;
item 10 is TASK-149.4, filed before.

1. **Trace-validate the journal and dax-cow specs against the code; generate tests from the
   specs' state graphs** (with TASK-44.5 for the bucket). Nothing today checks that the code is the
   spec; TLC and the spec mutations check only the designs. The follower trace machinery is the
   template. dax-cow's traces come from guest/sim seeds, the journal's from the model test's power
   cuts. Agents may change code or spec, but a spec change must still pass TLC and every spec
   mutation (Specula's anti-overfitting rule). Where a spec makes atomic what the code does in two
   steps, split the action first (MongoDB's failure).
2. **Memory-ordering audit confirmed by loom.** Per atomic: what it publishes, which load pairs
   with which store, the happens-before argument. A finding counts only when a loom model fails.
   Ordering mutants (Release/Acquire to Relaxed, a fence deleted) must turn a loom model red, or
   that's a missing test. dax.c's barriers would want LKMM litmus tests (herd7), licence pending
   (§4).
3. **Kernel review agents on guest/linux/fs/fxfs and the fs/dax.c patches.** review-prompts (orc)
   and Sashiko, both on Claude, with an fxfs/DAX context file of fxdax's invariants (doc-4, doc-9,
   doc-13, the dax-cow spec, dead-ends) and a known-issues file. First a whole-tree sweep for
   pre-existing bugs, 3-5 runs, findings kept only when they recur or a verifier confirms them;
   then on every change. A finding enters the backlog only with a guest/sim seed, a scenario, or a
   guest case.
4. **unsafe audit, two agents and a disposer.** Agent A writes each block's preconditions and
   proves its SAFETY claim against every other thread touching the memory (the Binder pattern:
   uffd handler vs mapper vs memfd lifetime) and SafeDep's sibling-method question; agent B labels
   each claim independently. Confirmation by a failing test, Miri on helpers factored away from
   syscalls, or Shuttle. clippy's `undocumented_unsafe_blocks` gates new code.
5. **Specula on extent_remote, WAL segments, compaction and the head.** Imports specs/bucket;
   invariants mined from the backlog, dead-ends and git history that nobody wrote down. Each claim
   arrives as a code-level test. Cheap in tokens; run in its own worktree.
6. **Variant hunts and rules from every fixed bug.** One agent per Done bug task: the root cause as
   a pattern, its other occurrences at HEAD, the pattern as a Coccinelle rule (C) or a dylint lint
   (Rust). Gate: flags the pre-fix tree, silent on the fix, few hits elsewhere, each triaged. A
   static rule applies the lesson at every call site, for good, where a mutant or seed covers one
   path.
7. **Concern-driven semantic mutants.** cargo-mutants changes operators; fxdax's real bugs are
   orderings (truncate before fsync, publish before seal, a missing fence, a lost wakeup).
   Agents write mutants from a list of concerns drawn from the backlog's bug classes, an
   equivalence filter drops no-ops, survivors become tasks under TASK-149 with a killing test.
8. **Assumptions as assertions on hostile and lifetime paths.** An agent walks virtio request
   decoding, extentd's codec, the window and uffd paths and dax.c, and writes each "this is safe
   because" as an assertion; fuzzers, the hostile tests, guest/sim and the hunt run with them on.
   Includes structure-aware generators for coverage plateaus in the cargo-fuzz targets.
9. **Guardrails for agent work, as hooks.** Implementer agents can't edit tests, specs or mutation
   configs in the same task (a PreToolUse deny, lifted by a separate test-writing agent); a diff
   check flags removed assertions, `#[ignore]`, loosened bounds and narrowed generators (AGENTS.md's
   "never write a test around a bug", enforced); reviewers read-only, report-only, verify before
   reporting, and from a second vendor (Codex) on load-bearing changes.
10. **syzkaller descriptions by agents** (TASK-149.4, already filed): the KernelGPT loop makes the
    descriptions cheap; a KCSAN guest beside it, races labelled by agents but "benign" never
    accepted without an argument in the code.

Not ranked, for later: Verus proofs of small pure cores (extent arithmetic, segment framing; MIT,
agents now prove >80% of real systems tasks, VeruSAGE), with `--no-cheating` and a check against
`assume`/`external_body`; a whole-daemon deterministic simulation (turmoil/madsim) with
linearizability checks of follower reads.

## 3. What not to do

- Generic technique prompts or long tutorial skills ("use TDD", "write TLA+"): measured neutral or
  harmful (Luu). Point agents at the risky parts by name instead.
- Agents that both audit and edit: keep reviewers report-only.
- Accept an agent's fix without a named violated invariant and a test that goes red with the bug
  back: ~20% of plausible agent kernel fixes match the developer's (Live-kBench), and many are
  "amputations" (early returns that hide the symptom). AGENTS.md's blind-spot rule already
  requires this; hold agents to it.
- Trust "benign" or "no bug" from a model: agents lean toward no-crash and miss >25% of real
  crashes (SysRB).
- One-shot spec writing without TLC feedback: almost nothing passes.

## 4. Licences (checked 2026-11-15, gh api and LICENSE files)

OSI, usable: review-prompts MIT; Sashiko Apache-2.0; semcode Apache-2.0; Specula Apache-2.0;
loom MIT; Shuttle Apache-2.0; Verus MIT; syzkaller Apache-2.0; KNighter, KernelGPT Apache-2.0;
dylint Apache-2.0; Miri MIT/Apache-2.0; cargo-mutants MIT; Buttercup AGPL-3.0.

Not usable: **Kani** (Apache-2.0 itself) runs on **CBMC, BSD-4-Clause** (advertising clause, not
OSI-approved): excluded. Antithesis is proprietary. agentic-pbt and MongoDB's test generator have
no licence (ideas only). trailofbits/skills is CC-BY-SA-4.0.

Pending: **herdtools7** (herd7, needed for LKMM litmus tests of dax.c's barriers) is under
CeCILL-B, whose OSI status is unclear: OPEN_QUESTIONS.md.

## Sources

- https://github.com/specula-org/Specula ; arXiv 2607.25333
- https://www.usenix.org/conference/nsdi25 (CCF Smart Casual Verification); arXiv 2404.16075
- https://www.mongodb.com/blog (Davis, 2025-06-02, eXtreme modelling); VLDB 2025 Schultz & Demirbas
- https://github.com/masoncl/review-prompts ; https://github.com/sashiko-dev/sashiko ;
  https://github.com/facebookexperimental/semcode
- https://lwn.net/Articles/1073583/ (LSF/MM 2026); https://lwn.net/Articles/1067741/ (netdev);
  https://lwn.net/Articles/1064830/ (mm); https://lwn.net/Articles/1049830/ (Maintainers Summit 2025)
- https://github.com/ise-uiuc/KNighter ; https://github.com/ise-uiuc/KernelGPT ; arXiv 2611.16215
- arXiv 2501.12862 (Meta ACH); arXiv 2601.22832 (Meta JiT)
- arXiv 2606.18619 (Code-Augur)
- https://www.anthropic.com/research/mythos-preview ; https://www.anthropic.com/research/property-based-testing ;
  https://www.anthropic.com/engineering/building-c-compiler
- https://sean.heelan.io/2025/05/22/how-i-used-o3-to-find-cve-2025-37899-a-remote-zeroday-vulnerability-in-the-linux-kernels-smb-implementation/
- arXiv 2602.07666 (AIxCC SoK); arXiv 2510.20270 (ImpossibleBench); arXiv 2602.02690 (Live-kBench);
  arXiv 2610.29332 (SysRB); arXiv 2501.14326 (LLMs and memory models)
- Shuttle's sync/atomic module docs (SeqCst only); https://github.com/tokio-rs/loom
- Dan Luu, "How well do agents use test and verification techniques?" (~2026-09)

Most 2026 figures are self-reported or preprints; the research agents fetched them but did not
replicate them.
