# Field Evidence

External results that pressure-test Euler's thesis: long-horizon agent
work is real, it is producing findings that matter, and the weakest part
of every result so far is the record of how it happened. Each entry
tracks what happened, what the evidence trail actually was, and what
Euler takes from it.

The vision (`docs/vision.md`) states the bar: a paper cites an
agent-driven finding, and an independent researcher inspects how the
system arrived at it. This ledger is where we watch the world approach
that bar, and where the gaps between their evidence trails and that bar
become Euler's roadmap.

## 2026-07: a 30-year oracle-complexity gap closed in one 148-minute run

Kerger (UC Berkeley) adapted the Cycle Double Cover prompting
methodology to derivative-free convex optimization. In a single
148-minute uninterrupted session, GPT-5.6 Sol Pro produced the main
argument for a near-quadratic lower bound on the oracle complexity of
zeroth-order convex optimization, closing the gap against Protasov's
1996 upper bound. The proof was formally verified in Lean.

- Preprint, Lean code, prompts, proof map:
  https://github.com/PhillipKerger/zero-order-bounds-lean-verification
- The 148-minute session and the refinement session are linked from the
  repository as chat transcripts.

**What it shows.** A structured ten-page brief, one long uninterrupted
run, and an external verification gate is enough to close a real open
problem. The author's own reading: results attainable with existing
techniques are now attainable by these systems; human effort moves to
problems needing genuinely new approaches.

**What it exposes.** The proof is Lean-verified; the process is a chat
share link. A year of the author's failed constructions and multiple
failed GPT-5.4/5.5 sessions, the dead ends that shaped the search, are
lost or informal. Narration is the only process evidence, and narration
is not evidence.

**What Euler takes from it.**

- The methodology maps onto existing Euler primitives: the structured
  brief is a template (`euler-extensions` templates), the candidate
  search is a population with verification and tournament stages
  (the maxproof extension is this exact shape), long runs are steerable
  and resumable sessions, and observers watch for stalls from outside
  the working context.
- The missing piece worth building: a Lean 4 verifier extension
  (managed-process, `lake build` as the materialization step) that
  checks a proof artifact and emits a verified artifact whose
  provenance cites the events that produced the proof. Conjecture
  brief, population search, Lean gate, citable artifact: the bar as a
  pipeline.

## 2026-07: Erdős's unit-distance conjecture (1946) disproven, end to end automated

OpenAI's internal model disproved the planar unit-distance conjecture:
ν(n) ≥ n^(1+δ) for infinitely many n, refuting the n^(1+C/log log n)
bound Erdős conjectured in 1946. The construction passes through
unramified pro-3 class field towers (Golod–Shafarevich), norm-one units
in CM fields, and a Minkowski lattice projection: number theory composed
into combinatorial geometry. The community widely believed the
conjecture true; recent results had been accumulating evidence for it.

- https://cdn.openai.com/pdf/74c24085-19b0-4534-9c90-465b8e29ad73/unit-distance-proof.pdf

**What it shows.** The pipeline was automated at every stage before the
last: an AI-written problem statement, an autonomous solve, an AI
grading pipeline reporting high confidence, and only then human
researchers and external number-theory experts (who confirmed, then
simplified and strengthened the argument). Composing standard machinery
from a distant field into the target problem stretches the "existing
techniques only" reading of these results; the direction of the result
also matters, since the accumulated evidence pointed the other way and
a disproof requires a construction nobody was looking for.

**What it exposes.** The paper takes a real step toward the bar by
publishing the verbatim prompt and the model's raw final response as
first-class artifacts. But the step is manual and partial: the run
itself (duration, intermediate attempts, what the grading pipeline
actually checked) is not inspectable, and verification is social
rather than formal. The published manuscript is a human-edited
exposition layered over an autonomous solution: two artifacts with
different authorship and different trust properties, distinguishable
here only because the authors chose to say so.

**What Euler takes from it.**

- Publishing prompt and raw output verbatim is provenance done by hand.
  In Euler that layering is structural: the raw solve, the grading
  passes, and the human exposition are separate artifacts in one causal
  chain, and the reader does not have to trust an authorship footnote.
- The AI grading pipeline is the verification-gate pattern again
  (independent judgment before human attention), consistent with the
  guardian and critic roles; the two-stage trust story (automated gate,
  then expert review) is exactly the shape a proof-search pipeline
  should record as events.
- An automated conjecture-to-solve-to-grade pipeline is an orchestration
  pattern, and orchestration patterns are extensions. The population,
  gate, and tournament stages already exist in maxproof; this result
  adds the missing front stage (problem statement authoring) and rear
  stage (exposition drafting) to the eventual shape.

## 2026-07: Star Fleet Math industrializes verified problem-solving

A public platform (Snyder, July 2026) running parallel agentic
harnesses ("starships"): GPT-5.6 instances on dedicated servers with
large compute, vector-searchable Lean 4 theorem libraries, SAT/SMT
solvers and computer algebra systems, a second-model verification gate,
and a local memory system ("Ton 618") linking verified premises through
dependency graphs. Thirteen complete solutions to Erdős problems at
release, each formalized in Lean 4.

- https://www.starfleetmath.com/

**What it shows.** Long-horizon agent mathematics is industrializing:
fleets, not sessions. The verification culture is the strongest of any
entry here: every solution ships the formal statement, pinned project
dependencies, verification scripts for independent checkers, and axiom
audits confirming only standard axioms. Artifact provenance, done
seriously, by hand.

**What it exposes.** You can verify that each proof is true; you cannot
inspect how the fleet found it. The search itself, including the dead
ends across thirteen problems, stays private, and that is where the
reusable knowledge lives. And the pattern across this whole ledger is
now visible: every group hand-rolls its own harness (a prompt
methodology, an internal pipeline, a starship). The scaffolding that
manages sessions, records provenance, gates on verification, and
carries memory between attempts is being reinvented, bespoke and
informal, at every lab.

**What Euler takes from it.**

- The bespoke-harness proliferation is the platform gap. A starship is
  a workflow: population search, solver access, verification gate,
  premise memory. On Euler those are extensions over one provenance
  substrate, and the harness stops being the unrecorded part.
- A verified-premise dependency graph accumulating across runs is the
  causal-DAG direction validated independently: runs compound instead
  of restarting, and the graph of what is known and how it was
  established is the compounding asset.
- Verification bundles (pinned deps, axiom audit, checker script) are
  the artifact shape a Lean verifier extension should emit.

## 2026-07: persistence without verification amplifies early mistakes

A benchmark comparison of Fable 5 and GPT-5.6 Sol on KIRO (fiber-network
design, search space around 10^1223) across repeated 30-minute runs,
with and without goal-persistence features.

- https://charlesazam.com/blog/fable-5-gpt-5-6-sol-goal

**What it shows.** A persistence mechanism can win individual trials
while making average performance worse: long unsupervised runs amplify
poor early decisions rather than correcting them. Implementations that
used an independent evaluator behaved differently from ones where the
working model declares its own completion.

**What Euler takes from it.** This is the failure mode Euler's
structural-accuracy principles exist to counter: decorrelated candidate
populations instead of one escalating run, verification gates so
persistence cannot compound an error past the checker, observers and
honest caps that can call a stall, and independent review roles rather
than self-declared completion (the guardian model). Persistence plus a
verification gate is a tool; persistence alone is variance.
