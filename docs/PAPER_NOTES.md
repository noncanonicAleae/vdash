# Paper notes: the AlphaProof Code framework

Source: `references/alphaproof_code_signal.pdf` (text in `references/alphaproof_code_signal.txt`).
Original URL: https://deepmind.google/documents/36/Formal_Verification_of_Signal_4.pdf
Blog: https://deepmind.google/blog/introducing-alphaproof-code/

The paper is mostly about the Signal result. The framework itself takes about 2 pages
(§2.1–2.4, pp. 3–6) plus a discussion in §7 (pp. 26–27). Everything we can reproduce
is summarised here.

## Workflow (Fig. 1, p. 2)
1. **Specification.** A *spec-writer* agent works with the user to turn models and
   properties from research papers into a Lean spec. *Reviewer* agents critique each
   draft. The user approves.
2. **Code → Lean.** (Signal-specific: stock `rustc` → AArch64 ELF → loaded into Lean
   with a formal ISA semantics.) **Out of scope for us.** Our "code" is whatever Lean
   definitions the spec contains.
3. **Proof.** A *prover team* (orchestrator, workers, …) proves the spec. A
   *gatekeeper* accepts a proof only if Lean's kernel checks it **and** `comparator`
   confirms it proves exactly a held-out copy of the spec.

Trusted: the spec, the semantics, Lean's kernel. Untrusted: all agent output.

## Agents are skills (§2.1, p. 4)
"All agents … are general-purpose coding agents whose behavior is defined by skills:
natural-language documents that describe an agent's task, the sources it should
consult and the checks it must perform."

## Spec quality (§2.1, p. 4)
- Reviewers review **from a skeptical perspective**. They are good at finding
  soundness and strength problems.
- Provers also prove **non-vacuity**: hypotheses are jointly satisfiable, and premises
  are met by concrete executions.
- **Mutation testing**: inject a natural bug, then prove the security theorem is
  *false* for the mutant.
- Historical bugs: prove the spec would have rejected them.
- The user's approval is a git commit, which becomes the gatekeeper's baseline.

## Proof synthesis team (§2.3, pp. 5–6)
- **Orchestrator**: divides up the work and schedules agents. **Never edits Lean
  files.**
- **Proof worker**: one subtask each.
- **Team lead**: groups workers under a larger, long-running goal. It absorbs the
  back-and-forth with its workers and sends the orchestrator *short summaries*: open
  obligations, completed lemmas, blockers. (Reason: limited context windows, and
  compaction loses state.)
- **Falsify first**: agents try to refute a goal before working on it.
- **Architect**: designs, and periodically revises, the high-level proof
  decomposition.
- **Integrator**: merges verified proofs into library modules, removes duplicate
  lemmas, generalises lemmas. **Only the integrator commits.**
- **Each file is edited by at most one worker at a time.**
- **Support roles**: schedule expensive Lean builds; monitor agents and resources.
- Scale: 40–80 agents per team, one machine, 24–72 h per campaign.

### Amendments (§2.3, p. 6)
Agents cannot edit the spec. They can propose an amendment to a **critic**, which
**never sees the proof attempts**. The critic accepts only if all three hold:
1. **Necessary**: shown by a machine-checked disproof, a counterexample, or an
   argument grounded in the code.
2. **Doesn't weaken security**, and any assumption it adds holds of the real code.
3. **Small**: typically a few lines in one definition.

Amendments that change trusted assumptions need the user's approval (or a user-set
policy). Each one is logged with its justification.

## Gatekeeping (§2.4, p. 6)
- A statement can be weakened *indirectly* by changing a definition it depends on, so
  the gatekeeper compares the **statement and its entire transitive definition
  closure**.
- At the start of the campaign, the spec is copied from the baseline commit into a
  **held-out directory no agent can write to**. It changes only through accepted
  amendments.
- Accept only if: the statement and dependencies are identical to the held-out copy,
  the proof uses only the standard axioms (`propext`, `Quot.sound`,
  `Classical.choice`), and the kernel accepts it.
- The gatekeeper runs a **deterministic script** that can be rerun by hand.
- Only the gatekeeper declares a top-level goal done.
- §7, p. 26: the gatekeeper sees **only file paths and theorem names**, none of the
  provers' dialogue. Lean replays every proof **in a fresh environment**. `sorry` and
  `native_decide` are rejected.

## Lessons from campaigns (§7, pp. 26–27)
- Proofs were produced by "Gemini 4 Carbon" agents; specs also used Claude Opus 5.5
  (see Fine print). No human wrote proof code.
- There is a separate **interface/monitor agent above the orchestrator**. It monitors
  the team and diagnoses bottlenecks with the humans.
- **Shared bulletin file** for notices, instead of agents repeating messages.
- **Team leads** try to refute goals before assigning them, fix shared parameters
  across callees, and point workers to existing lemmas.
- **Support agents** batch Lean builds to fit in memory, watch for stalled agents and
  **exhausted model quotas**, and refactor slow modules.
- The orchestrator **reshapes the team** as the campaign evolves.
- **Every new invariant must come with a witness** that it holds on some reachable
  state, so agents can't make progress with vacuous lemmas.
- **Spec weakening is a real failure mode**: one swarm redefined a hypothesis predicate
  as `False`, and the trick spread. Hence the strict gatekeeper.
- An amendment is allowed only if a stronger invariant or a different decomposition
  can't finish the proof instead. A proposal that weakens security to match the code
  is escalated to a human as a potential bug.

## Fine print (second pass over the whole paper, 2026-11-12)
Details the summary above missed or blurred. Section refs are to the paper.

### Spec writing
- **Two model families.** Specs were developed with Gemini 4 Carbon *and* Claude
  Opus 5.5; "an agent from a different model family then reviewed the draft" (§7,
  p. 25). Proofs were Gemini 4 Carbon only (§7, p. 26).
- **Reviewer checklist** (§7, p. 25): faithfulness to the source paper *and to the
  implementation*; strength ("would it rule out known or plausible bugs?"); where the
  spec goes beyond the literature, consistency with the literature's conventions.
- **Three ways a spec is weak** (§2.1): overly restrictive assumptions on inputs, weak
  properties of the output, too few constraints on existentially quantified witnesses.
  §5.2.2 is a worked case: "there exists a reduction" is vacuous if the witness may be
  noncomputable or arbitrarily expensive.
- **Small and per component.** The spec-writer "drafted a small set of top-level
  statements" per component; campaigns were launched "on different parts of the
  system" (§7, p. 25).
- **Legibility.** "Left to themselves, agents produced specifications that were hard
  to audit"; humans directed rewrites for legibility (§7, p. 25).
- **Departure catalog.** Every place the Lean statements depart from the reference
  papers or the code is recorded with its effect on the result (about 200 and 35
  entries) (§7, p. 26).
- **Trust marking.** "Every file is marked as trusted, trusted in part, or checked by
  Lean." What a reader must read is the transitive closure of the headline theorems,
  about a third of the spec directories (§7, p. 25; §5.3).
- **Trusted files contain only definitions**; anything provable (program logic, proof
  automation) is untrusted and proved sound (§2.2).
- **Trusted definitions are tested**, not just reviewed: lockstep against independent
  oracles, published test vectors, known-answer tests (§2.2, App. C.10).
- **Contracts enter as hypotheses**: theorems quantify over "every extern
  interpretation meeting the contracts" (§6); contracts are part of the spec (§2.2).
- **Who writes the spec-quality theorems**: the *prover* agents "create theorems to
  check the specification's quality", and "agents add natural bugs to the code" (§2.1).
- **Non-vacuity in practice** (§5.1.2): honest sessions are proved reachable, the
  premises of the secrecy theorems are proved satisfiable, and the main statement
  carries a non-emptiness clause so a ∀ cannot hold vacuously (§5.1.5). A concrete
  model witnesses joint satisfiability of the algebraic assumptions (App. C.10).
- **Mutation / historical bugs** (§6): the mutant is the real code with a fix
  reverse-applied. Each disproof `¬Sec(mutant)` exhibits concrete witness inputs, has
  a spec part and an implementation part, and uses only the standard axioms. Not every
  theorem can be refuted on a mutant: the computational theorem cannot (it would
  require proving hardness assumptions).

### Human role (§2, §7 p. 27)
- Humans select what to prove, resolve ambiguities, evaluate trade-offs, make all
  final decisions.
- Humans also **guided the overall proof strategy** and the structure of the specs.
- During campaigns they reviewed **escalated spec issues and potential bugs**,
  adjudicated amendments that change trusted assumptions, and monitored progress
  through **status logs**.
- The monitor agent's logs were used "together with it to diagnose bottlenecks and
  **propose fixes to its processes**", i.e. skill changes. Skills were developed
  iteratively by watching campaigns.

### Prover team
- Leads "**fix shared parameters** such as stack bounds across callees" before fanning
  work out, and point workers to existing lemmas (§7).
- The integrator also **generalises** lemmas for reuse (§2.3).
- Support agents batch builds **to fit in memory** and **refactor slow modules** (§7).
- The witness rule applies to "every new invariant **an agent introduces**", at lemma
  level, not only to the plan (§7).
- Fig. 1 caption: provers propose amendments "if the specification is **vacuous** or
  otherwise incorrect".

### Amendments (§7, p. 27, beyond the three criteria)
- Allowed only if the proof cannot be completed with a stronger invariant or a
  different decomposition.
- Must keep **the non-vacuity witnesses and honest runs covered**.
- A neutral amendment must bring the spec **closer to the code**.
- Weakening security to match the code is **never** an amendment; it is escalated to a
  human as a potential bug. So the critic has three outcomes, not two.
- Logged with justification "on an **audited branch**"; **all** amendments are
  reviewed by humans at the end of each campaign.
- The paper is inconsistent on approval: §2.3 says trusted-assumption amendments are
  approved "by the user or under a user-set policy"; §2.4 says amendments are
  "approved by human authors before the gatekeeper applies them".

### Gatekeeping
- `comparator` is **the** mechanism, not an optional extra: it confirms statement and
  dependencies are identical, axioms are standard, and the kernel accepts (§2.4).
- The gatekeeper also **checks recorded approvals** for amendments (§2.4): the
  held-out copy must be explainable as baseline + approved amendments.
- The gatekeeper itself **applies** accepted amendments to the held-out copy (§2.4).
- "When proof development is complete, we re-run comparator manually" (§2.4).
- The TCB includes **tooling and build scripts** (§5.3). For us: the gate and policy
  code are trusted and must be out of agents' reach.

## Related open work (for ideas, not code reuse)
- `Kripner/nanoproof` (MIT): a clone of the *original* AlphaProof (RL + MCTS, trained
  models). It's a different paradigm, so it's not directly useful here.
- "A Minimal Agent for Automated Theorem Proving" (arXiv 2602.24273): an open,
  single-agent loop with iterative refinement, library search and context
  management. It's a good reference for the **worker** inner loop.
- `leanprover/comparator`: the upstream checker the paper uses. Use it optionally
  when installed (see DESIGN.md).
