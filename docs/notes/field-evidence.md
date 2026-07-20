# Field Evidence

External results that pressure-test Euler's thesis: long-horizon agent
work is real, it is producing findings that matter, and the weakest part
of every result so far is the record of how it happened. Each entry
tracks what happened, what the evidence trail actually was, and what
Euler takes from it.

A standing caveat: this work is moving very fast, and none of it has
finished the ordinary scientific vetting process. Formal verification,
where present, certifies that a proof is valid; it does not replace
peer review, which also asks whether the statement formalized is the
statement claimed, whether the result matters, and how it sits in the
literature. Entries here may be revised or retracted as the record
catches up. Read all of them with that grain of salt.

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

## 2026-07: a claimed Jacobian-conjecture counterexample, arithmetic-checkable in minutes

Posted by @__alpoge__ (July 2026): an explicit polynomial map from C^3
to C^3, credited to the model Fable, claimed to disprove the Jacobian
conjecture (open since 1939). The map is

`((1+xy)^3 z + y^2(1+xy)(4+3xy), y + 3x(1+xy)^2 z + 3xy^2(4+3xy), 2x - 3x^2 y - x^3 z)`

with Jacobian determinant a constant -2, and the three distinct points
(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2) all sent to (-1/4,0,0). A
polynomial map with nonzero constant Jacobian that is not injective is
exactly what the conjecture says cannot exist. The stated verification
was Wolfram Alpha links and a second model (Grok) agreeing.

- https://xcancel.com/__alpoge__/status/2079028340955197566

**What it shows.** The checkable core of this claim is arithmetic, not a
proof, so we checked it ourselves rather than transcribe it: a symbolic
algebra pass confirms the Jacobian determinant is identically -2, and
exact rational evaluation confirms all three distinct points land on
(-1/4,0,0). Both facts hold. Unlike a Lean proof of a hard theorem, a
counterexample of this shape reduces to two finite computations, and a
verification gate settles them in minutes.

**What it exposes.** Verified arithmetic is not a disproven conjecture,
and the distance between the two is the whole point of this ledger.
Certifying that det J = -2 and that three points collide answers the
mechanical question ("do these identities hold") and is silent on the
mathematical one ("is this a valid counterexample to the conjecture as
the field states it, or a known artifact, or a subtly mis-stated
hypothesis"). That second question is peer review, and this result has
had none: its evidence trail is a social-media post, calculator links,
and one model agreeing with another (agreement is not verification, and
two systems concurring is not two independent checks). An 85-year-old
conjecture falling to a map that fits on one line is precisely the
claim the standing caveat exists for. We record the arithmetic as
confirmed and the disproof as unverified, and expect this entry to be
revised as the record catches up.

**What Euler takes from it.**

- This is the cleanest case yet for provenance honesty in verified
  artifacts. An arithmetic (or Lean) verifier extension could attach a
  verified artifact to this map automatically, but the artifact's honest
  label is "det J is the constant -2 and F collides these three points,"
  never "the Jacobian conjecture is false." A verified artifact must
  claim exactly what was checked, not what someone concluded from it,
  and the causal chain must keep the check and the conclusion as
  separate, separately-trusted events.
- Cheap gates are worth wiring in precisely because they are cheap: the
  checkable part here cost one tool call. The verification-gate pattern
  does not require a hard proof to earn its place, it requires a
  claimant willing to state the checkable core.
- The contrast across this ledger sharpens: earlier entries had formal
  proofs and private processes, this one has a fully public process and
  no formal proof at all. Euler's substrate is meant to make the whole
  spectrum legible, so that "what was actually verified" is a property
  of the record rather than a matter of trusting the narrator.
