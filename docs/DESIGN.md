# Design

How the paper's roles (see `PAPER_NOTES.md`) map onto `vdash`, a standalone Rust program
that drives the `claude` and `agy` (Gemini) subscription CLIs. Agreed with the user on
2026-11-12; a second review pass is planned once the app runs end to end. Until then,
change a decision only for a concrete reason, and record the change here.

Diagrams: `docs/architecture.html`. Build spec: `docs/ROADMAP.md`.

## 0. Requirements from the user
1. **Rust.** Any crate is acceptable as long as the result is a clean, standalone,
   lightweight executable (§14).
2. **A nice CLI**: an interactive terminal app, plus scriptable one-shot commands.
   The app covers both the spec conversation and a live view of a running campaign.
3. **Slash commands inspired by Claude Code**, such as `/model` and `/btw` (§12).
4. **Claude and Gemini subscriptions concurrently**, split by role by default, with
   the option to choose (§3).
5. Subscription only; no API keys.

## 1. Scope and deliberate deviations
We rebuild three of the paper's four components: spec writing (§2.1), proof synthesis
(§2.3) and gatekeeping (§2.4). The machine-code semantics (§2.2) is out of scope: our
"code" is Lean definitions.

| Paper | Here | Why |
|---|---|---|
| Gatekeeper is an agent that calls a deterministic script | Only deterministic code; no LLM in the gate | Nothing for a model to add; fewer ways to fail |
| `comparator` checks statement + closure | Our own closure fingerprint plus `leanchecker`; `comparator` optional (not installed here) | No sandbox dependency. Weaker: see §8 and §15 |
| Prover agents invent non-vacuity and mutation theorems | The spec-writer writes them as trusted targets | Statements an untrusted agent writes can be trivially true; as targets the user sees them and the gate checks them, including after every amendment |
| 40–80 long-lived agents | A handful of fresh, bounded subprocess calls per round | Subscription rate limits |
| Agents message each other | Agents never talk; the engine passes structured replies and files | Context control and auditability |

## 2. Why a separate engine orchestrates
- Backend-agnostic: the team structure lives outside any model, so Claude and Gemini
  agents can sit in the same team.
- Trust boundaries are code: file ownership, held-out spec, gate.
- Context management is the paper's main concern. Every agent call is a fresh
  subprocess with a bounded prompt; memory lives in files.
- **Agents write only Lean files.** All other campaign state (plan, state, bulletin,
  lead notes, amendment records) is written by the engine from the agents' replies.

## 3. Backends: Claude and Gemini side by side
One trait: `Backend::run(AgentCall) -> AgentResult`. Exact argv, result fields and
failure classification are in `ROADMAP.md` (§1 facts, cards M2-02 and M2-03).

- **Claude**: `claude -p --output-format json --json-schema <reply schema>
  --append-system-prompt <skill> --tools <profile> ...`, prompt on stdin. The reply is
  the `structured_output` field. Never `--bare` (it requires an API key).
- **Gemini**, through Google's Antigravity CLI (`agy`), which is replacing Gemini
  CLI: `agy --output-format json --json-schema <reply schema> --mode <mode>
  -p=<skill + prompt>`. It has the same structured-output mechanism as Claude; it has
  no system-prompt flag and takes the prompt as an argument, not on stdin, so prompts
  are size-capped. Each backend's command is configurable.
- Both: API-key variables are stripped from the child environment
  (`use_subscription = true`); sessions are never resumed; user settings, MCP servers
  and extensions are switched off for agents as far as each CLI allows.

**Role assignment.** `[roles]` in `campaign.toml` maps each role to one or more
`backend[:model]` entries. Defaults when both CLIs are logged in:

| backend | roles |
|---|---|
| Claude | spec-writer, architect, orchestrator, team lead, integrator, critic, monitor |
| Gemini | reviewer, falsifier |
| both | worker |

So drafting and reviewing a spec are done by different model families (as in the
paper), and proof work is spread across both subscriptions. A role with several
entries is served by whichever listed backend has a free slot. The user changes any
of this in the file or live with `/model`.

**Scheduling.** Each backend has its own concurrency cap (`max_concurrent`) and its
own quota state. When one backend reports a usage limit it is paused with exponential
backoff while the other keeps working; calls for multi-backend roles flow to the
healthy one, and calls for a role pinned to the paused backend wait (or spill over if
`spillover = true`). If only one CLI is available, every role falls back to it with a
warning. `vdash doctor` reports what is installed and logged in.

### Tool profiles
| profile | meaning | roles |
|---|---|---|
| `none` | no tools; everything is inlined in the prompt | critic |
| `readonly` | read and search files | orchestrator, team lead, reviewer, architect, monitor |
| `lean` | readonly + edit files + run `vdash check` / `vdash axioms` only | spec-writer, worker, falsifier, integrator |

Each backend maps a profile to its own flags. Profiles reduce accidents; enforcement
is §7. Measured behaviour (ROADMAP facts C6–C11, A10–A17):
- **Claude** agents run with `--restricted --permission-mode dontAsk`, so file tools
  are confined to the campaign. They may run `vdash check`/`vdash axioms` (lean
  profile), `vdash search` and `grep` over Mathlib, read-only shell commands, and
  `WebSearch`/`WebFetch`.
- **Antigravity** agents in the readonly and lean profiles run with `--mode auto
  --sandbox`. Print mode's built-in safety monitor denies every shell command in the
  other modes and ends the turn (A15), so this is the only way they can run
  `vdash check` or `grep` Mathlib. The sandbox confines shell and file writes to the
  campaign (plus `/tmp`) and leaves reads and network open (A16); the state directory
  (anchor, control inbox) is therefore out of their reach. There is still no
  read-only mode, so the snapshot/revert is what undoes their edits inside the
  campaign. The monitor (which runs beside the engine with no snapshot) and the
  critic keep agy's default mode and run no commands. Mathlib is never added with
  `--add-dir`, which would make it writable (A17).

## 4. Campaign layout
```
<campaign>/
  campaign.toml
  sources/                 # user-owned: BRIEF.md, papers, reference code
  STRATEGY.md              # user's proof-strategy guidance; fed to architect and orchestrator
  skills/                  # optional per-campaign overrides of the built-in skills
  spec/DEPARTURES.md       # where the Lean departs from the sources, and the effect
  spec/review-N.md         # reviewer critiques per spec round
  lean/Apc/Spec/**         # TRUSTED. Definitions only.
      Impl.lean            #   convention: the artifact under verification (if any)
      Mutants.lean         #   convention: deliberately buggy variants (optional)
      Tests.lean           #   convention: #eval / `example ... := by decide` sanity tests
  lean/Apc/Challenge.lean  # TRUSTED. Target theorems, each `:= by sorry`
  lean/Apc/Solution.lean   # untrusted. Restates the targets with proofs; does not import Challenge
  lean/Apc/Proof/Lib/**    # untrusted lemma library (integrator-owned)
  lean/Apc/Proof/Work/**   # untrusted scratch files (one worker per file)
  .heldout/                # copy of the trusted files; changed only by the engine
  .build/{work,gate}/      # oleans
  plan.json  state.json  bulletin.md
  leads/<id>.md            # each lead's notes
  amendments/NNN-*.md      # proposal, evidence, critic verdict, approval
  escalations/NNN-*.md     # potential bugs and spec issues for the human
  logs/events.jsonl        # append-only event stream (what the app displays)
  logs/status.md           # one paragraph per round, plain text
  logs/<round>/<role>-<n>.json
```
The campaign is its own git repo. The baseline commit is the user's approval. Only
the engine commits.

**State directory outside the campaign**: `$XDG_STATE_HOME/vdash/<campaign-id>/`
(default `~/.local/state`). It holds what agents must not be able to touch:
- `anchor.json`: the baseline commit, a manifest hash of `.heldout/`, and the chain of
  applied amendments. `chmod a-w` does not stop a same-user process, so the anchor,
  not the file mode, is what the gate trusts.
- `engine.lock` and `inbox/`: the running engine's lock and its control messages (§11).

## 5. Spec phase (paper §2.1)
Re-entrant; all state is in files, and the user may hand-edit the Lean between any two
steps. Each step exists as a one-shot command and as an action in the app, where the
user simply types to the spec-writer.

- `vdash init <dir> --host <lake project>` creates the layout.
- `vdash spec draft`: the `spec_writer` reads `sources/` and writes `Spec/**`,
  `Challenge.lean` and `spec/DEPARTURES.md`. Its skill requires:
  - a small set of top-level statements;
  - assumptions as hypotheses or structure fields, never `axiom`;
  - **non-vacuity targets**: for each target with hypotheses, a target stating they
    are jointly satisfiable (a concrete witness);
  - **mutation targets** where a refutation is feasible: a buggy variant in
    `Spec/Mutants.lean` and a target stating the property fails for it;
  - sanity tests in `Spec/Tests.lean`;
  - legible definitions with doc comments.
- Then the reviewers run in parallel, read-only, one per lens, seeing only files:
  1. **soundness**: is anything false, inconsistent or ill-typed in spirit?
  2. **strength**: over-restrictive input assumptions, weak output properties,
     under-constrained existential witnesses, universal statements that could hold
     vacuously over an empty set; "would it rule out known or plausible bugs?"
  3. **fidelity**: faithful to `sources/` and to `Impl.lean`; is `DEPARTURES.md`
     complete?
  4. **legibility**: can a human audit it?
  Output: `spec/review-N.md`.
- `vdash spec revise "<feedback>"`: the spec-writer gets the latest review plus the
  user's feedback; reviewers run again.
- **Loop** (user request, 2026-11-15): `draft`, `revise` and `review` keep going while
  any reviewer says `revise`: the spec-writer revises against the latest review (plus
  any guidance the user typed meanwhile), reviewers rerun, until all four say `ok`,
  the user says it is good enough (`/stop`; `/approve` stops then approves), or
  `[budget] spec_max_rounds` rounds have run. The loop holds the engine lock, so the
  user's messages reach it through the control inbox (§11) between rounds. `--once`
  runs a single step.
- `vdash approve`: requires that the trusted tree builds and that the only `sorry`s are
  the target proofs in `Challenge.lean`. Warns (does not block) if the spec changed
  since the last review, or if `Spec/**` outside `Tests.lean` contains a `theorem`.
  Then: baseline commit, copy to `.heldout/`, write the anchor.
- `vdash tcb`: print the transitive closure of the targets, i.e. exactly what a reader
  must read. Trust is this closure, not the `Spec/` directory.

Every theorem in `Apc.Challenge` is a target. Non-vacuity and mutation targets are not
special to the machinery.

## 6. Lean build
No lakefile per campaign; reuse the host project's Mathlib through `LEAN_PATH`:
- Every Lean subprocess sets `ELAN_TOOLCHAIN` to the host's `lean-toolchain`; outside
  the host directory `elan` would otherwise pick its default toolchain.
- `LEAN_PATH = <tree build dir> : $(cd host && lake env printenv LEAN_PATH)`, cached.
  A campaign with `use_mathlib = false` uses the build dir alone (the isort example).
- Parse the `import` lines of `Apc.*` modules, sort topologically, compile each with
  `lean -R <tree>/lean -o <build>/Apc/X.olean <file>`. Skip a module whose source hash
  and dependency hashes are unchanged.
- **Memory-bounded concurrency**: every `lean` invocation takes one of
  `max_parallel_lean` file-lock slots, shared across processes (the paper's
  build-scheduling support role).
- `vdash check <file>`: build dependencies, elaborate the file with `--json`, print
  errors, flag `sorry`. Output is capped, because it lands in an agent's context.
- Elaboration time per module is recorded; the slowest are shown to the integrator
  ("refactor slow modules").

## 7. Trust enforcement
- Before each agent round, snapshot the campaign directory (path → sha256 and bytes)
  except `.build/`, `logs/` and `.git/`.
- After the round, revert every changed file that is not a Lean file assigned to an
  agent in that round. Trusted files, `.heldout/`, `sources/`, `campaign.toml` and all
  engine-owned state are always reverted (outside the spec phase and amendment
  application). Each violation gets a bulletin line.
- One file has at most one writer per round; a conflicting assignment is deferred.
- While an engine runs, it is the only writer of campaign files other than agents'
  assigned Lean files. The app changes things by sending the engine a control message
  (§11), never by editing files under it.
- The gate verifies `.heldout/` against the anchor before using it.

## 8. Gatekeeper: deterministic, rerunnable with `vdash gate`
It sees file paths and theorem names only.

0. **Anchor check**: `.heldout/` manifest hash equals the anchor's; the amendment
   chain in the anchor matches `amendments/` approvals. Otherwise fail everything.
1. **Fresh replay**: copy `.heldout/` to a temp challenge tree; copy the work tree's
   `lean/` (including its Spec files, so tampering is detected, not masked, but
   without `Challenge.lean`) to a temp solution tree. Build both from scratch.
2. **Source scan** of untrusted files, comments and strings stripped: reject kernel
   bypasses (`skipKernelTC`, `addDeclWithoutChecking`, `debug.`), compiler trust
   (`unsafe`, `implemented_by`, `extern`, `native_decide`, `ofReduceBool`), `axiom`,
   and environment manipulation (`run_cmd`, `initialize`, `modifyEnv`, ...). Then
   `leanchecker` replays the untrusted modules through the kernel (`kernel_recheck`).
3. **Fingerprint** (an embedded Lean script):
   - Challenge env: targets are all theorems in module `Apc.Challenge`.
   - For each target, walk its transitive constant closure: type always; values for
     `def`/`opaque`; constructors for inductives; parent inductive for constructors
     and recursors. Theorems contribute their type only. The walk stops at constants
     outside the `Apc` package (Mathlib, core), recorded by name: both trees build
     against the same library files.
   - Each constant is serialised structurally (a DAG, binder names dropped) and
     hashed with sha256. Every in-package closure constant must live under `Apc.Spec`.
   - Solution env: the same for the same names; each target must be a `theorem` whose
     axioms ⊆ {`propext`, `Quot.sound`, `Classical.choice`}. This is what rejects
     `sorry` (`sorryAx`) and `native_decide`; step 2 is belt and braces.
4. A target is **accepted** iff the closure maps are identical, it is a theorem, and
   its axioms are standard. Per-target verdicts go to `state.json`; commit.
5. If `comparator_cmd` is set, run it too with positional args `<chal_build> <sol_build>`
   and env vars `VDASH_CHALLENGE_BUILD`, `VDASH_SOLUTION_BUILD`,
   `VDASH_CHALLENGE_TREE`, `VDASH_SOLUTION_TREE`, `VDASH_TARGETS`,
   `ELAN_TOOLCHAIN`, and (when `use_mathlib = true`) `VDASH_HOST_LEAN_PATH`;
   require exit 0 (failures set `comparator_disagrees` and `detail.comparator`).

`vdash axioms <file> <Name>...` (same script, existence + axiom check only) verifies
lemma-level claims before a goal's status becomes `proved`.

## 9. Proof phase (paper §2.3): `vdash prove`
Every agent reply is a structured object checked against its role's schema. An
invalid reply gets one repair call. A campaign can be killed at any point:
`state.json` records the stage, and each stage is safe to rerun.

One round:
1. `gate` for the current verdicts.
2. `architect` (if there is no plan or a revision is requested): reads the spec,
   `STRATEGY.md` and state; returns the goal decomposition, stored as `plan.json`.
   Goal fields: `id, kind, lean_name, statement, file, depends_on, parent_target`.
3. `orchestrator` (readonly) gets goals and statuses, lead summaries, bulletin tail,
   gate verdicts, open escalations, and any guidance the user typed since the last
   round. Returns `{leads:[{lead_id, goal_ids, brief}], revise_plan, integrate,
   notes}`. Choosing the groupings each round is how it reshapes the team.
4. Leads in parallel (≤ `max_leads`). Each lead:
   a. **Plan** (with its notes file) → `{refute:[goal_id], shared:{definitions,
      lemma_statements, conventions}, assignments:[{goal_id, file, instructions,
      existing_lemmas}]}`. `shared` is the paper's "fix shared parameters across
      callees": interfaces are pinned before work fans out.
   b. **Falsify first**: a `falsifier` per refute goal tries to disprove it in a
      scratch file → `{goal_id, verdict: false|plausible|unknown, evidence}`. A
      `false` verdict on a goal that follows from the spec becomes an amendment
      proposal or an escalation, not more proof work.
   c. `worker`s (one per file; concurrency bounded per backend, §3) edit only their
      file, must run `vdash check`, and return `{goal_id, status, lean_names,
      new_definitions:[{name, witness}], notes, amendment?}`. **Witness rule**: each
      new invariant or hypothesis predicate must name a lemma showing it is inhabited.
      After each reply the engine restores the trusted files and runs the check
      itself; if the file does not build cleanly it calls the worker again with the
      diagnostics (up to two more times). Workers therefore never depend on being
      able to run `vdash check` (Antigravity's safety monitor sometimes refuses it).
   d. **Summarise** → `{summary, goal_status, notes, bulletin[], amendments[],
      escalations[]}`. Only the short summary reaches the orchestrator. The engine
      writes `notes` to `leads/<id>.md`.
5. The engine reverts policy violations (§7), then verifies every claimed lemma and
   every witness. Unverified claims are downgraded and noted.
6. Every `integrate_every` rounds, or on request: the `integrator` moves verified
   lemmas from `Proof/Work/**` to `Proof/Lib/**`, removes duplicates, generalises for
   reuse, refactors the slowest modules, keeps `Solution.lean` importing the library.
   The engine re-checks the build and commits; a broken integration is reverted.
7. Amendments and escalations (§10).
8. `gate`. Append a paragraph to `logs/status.md`. Stop when every target is accepted
   or the budget runs out.

## 10. Amendments and escalations (paper §2.3, §7)
A proposal is `{file, old_text, new_text, justification, evidence_kind:
disproof|counterexample|argument, evidence_lean_name?, alternatives_tried}`: an
exact-replace edit (`old_text` must occur once), not a diff.

1. The engine verifies a claimed machine-checked disproof first.
2. The `critic` (profile `none`) is shown only the spec, the edit, the justification
   and the verified evidence statement. It never sees proofs or agent dialogue.
   Criteria: **necessary** (and no stronger invariant or other decomposition would
   do); **does not weaken** the guarantee, and any added assumption holds of the real
   artifact; **small**; a neutral change must bring the spec **closer to the code**.
   Verdict: `accept | reject | escalate`, plus `touches_trusted_assumptions`.
3. **Escalate**: a proposal that would weaken the guarantee to match the code is never
   an amendment. It is written to `escalations/` as a potential bug for the human; the
   campaign continues on other goals. The human fixes the code or the spec out of band,
   dismisses it (`vdash amend reject N`), or knowingly accepts the weaker guarantee with
   `vdash amend approve N --accept-weakening` (`/amend approve N --accept-weakening`).
   Plain `approve` refuses an escalated proposal, and the flag is refused on anything
   else. An accepted weakening is applied like an amendment but recorded with
   `weakening_accepted = true`, which the record and `vdash amend list` show
   (`[WEAKENING ACCEPTED]`). `auto_neutral` never applies one.
4. Approval: `amendment_policy = "manual"` (default): the user approves or rejects
   (`vdash amend approve N`, or `/amend` in the app). `"auto_neutral"`: auto-apply when
   the verdict is accept and trusted assumptions are untouched; everything else waits.
5. The engine applies an approved amendment to the work tree and `.heldout/`, extends
   the anchor chain, and commits it as its own `amend:` commit. All accepted targets
   are reset; the next gate re-proves them against the amended spec. Because
   non-vacuity targets are ordinary targets, "keeps the witnesses covered" is enforced
   by the gate.
6. `vdash amend list` is the end-of-campaign review: every amendment, its
   justification, verdict and approver.

## 11. Processes: engine, app, events
- **Engine**: the process that runs proof rounds (`vdash prove`). It holds
  `engine.lock` in the state directory, so there is at most one per campaign. It
  appends one JSON line per happening to `logs/events.jsonl` (round and stage
  changes, agent started/finished with role, backend, goal, cost; gate verdicts;
  violations; quota pauses) and polls `inbox/` for control messages between agent
  launches: `pause`, `resume`, `stop`, `set_role`, `guidance`, `amend_decision`.
- **App**: `vdash` with no subcommand. It never runs rounds itself: it starts an engine
  as a detached child (or finds one already running through the lock) and follows the
  event file. Closing the app leaves the engine running; reopening it reattaches.
- **One-shot commands** (`vdash gate`, `vdash status`, `vdash check`, ...) work with or
  without an engine and are what scripts and agents use.

## 12. The interactive app
Layout: a header (campaign, phase, round and stage, accepted targets, cost and elapsed
time, health of each backend); a scrolling transcript of events and answers; a side
panel of running agents (role, backend, goal, elapsed) and goals by status; an input
line with history and slash-command completion. Narrow terminals drop the side panel.
Without a TTY, `vdash` prints `status` and exits.

**Plain text** goes to whoever the user is working with: in the spec phase, the
spec-writer (first message drafts, later ones revise); in the proof phase, it is
guidance for the orchestrator's next round (the paper's humans "guided the overall
proof strategy") and is also appended to `STRATEGY.md`.

**Slash commands:**

| command | effect |
|---|---|
| `/help` | list commands |
| `/status` | the plain summary |
| `/model` | show the role → backend:model table; `/model <role\|all> <backend>[:<model>]` changes it, live |
| `/btw <question>` | a side question answered by the read-only monitor agent from state, logs and bulletin, without disturbing the running work |
| `/spec`, `/review`, `/approve` | draft or revise the spec, rerun reviewers, approve |
| `/prove`, `/pause`, `/resume`, `/stop` | control the engine |
| `/gate`, `/tcb` | run the gate; show what must be trusted |
| `/amend [approve N \| reject N]` | list or decide amendments |
| `/agents`, `/logs <agent>` | running agents; one agent's transcript |
| `/bulletin`, `/cost` | shared notices; usage per role and backend |
| `/doctor` | toolchain and login checks |
| `/quit` | leave the app (the engine keeps running) |

`/btw` is the paper's monitor agent, "the interface between us and the team". Its
answers may include suggested skill edits, which only the user applies.

## 13. Config (`campaign.toml`)
A sketch; the authoritative key list with defaults is `ROADMAP.md` §3.1.
```toml
id = "…"
host_project = "/path/to/lean_project"
use_mathlib = true
use_subscription = true
[backends.claude]
command = ["claude"]
max_concurrent = 4
[backends.gemini]
command = ["agy"]
max_concurrent = 4
[roles]                       # backend[:model], or a list to share a role
spec_writer = "claude"
reviewer = "gemini"
worker = ["claude:sonnet", "gemini"]
[budget]
max_rounds = 20
max_leads = 3
max_parallel_lean = 4
agent_timeout_s = 1800
integrate_every = 3
[policy]
amendment_policy = "manual"   # or "auto_neutral"
kernel_recheck = true
spillover = false
comparator_cmd = ""
```

## 14. Implementation shape
- One Cargo package: a library (`vdash`) and one binary (`vdash`). No runtime files: the
  skills and the Lean fingerprint script are embedded at compile time, so the
  executable is self-contained. Skills remain Markdown files in the repo
  (`skills/*.md`), and a campaign can override one by placing a file in its own
  `skills/`.
- No async runtime. Agents and Lean are subprocesses supervised by ordinary threads
  and channels; that is all the concurrency this needs and it keeps the binary small.
- Crates are chosen for small, well-known building blocks: argument parsing,
  serde/JSON/TOML, sha2, a terminal UI library, file locks and process groups.
  External programs (`lean`, `lake`, `leanchecker`, `git`, `claude`, `gemini`) are
  invoked, not linked.
- Release profile tuned for size (LTO, one codegen unit, stripped); target: one file
  under about 10 MB.

## 15. Known gaps versus the paper
- No machine-code semantics or binary loading.
- The fingerprint is not a `lean4export` replay into an independent kernel, and the
  gate does not sandbox the build. `comparator` is not installed on the dev machine.
- Agents and the engine share one OS user; the trust anchor and snapshot/revert stand
  in for real write isolation.
- Sandboxed Antigravity agents can still write under `/tmp`, and they can run any
  command inside the campaign (fact A16). The monitor and critic run unsandboxed but
  with no shell. The control inbox in the state directory is not signed; with the
  sandbox, agents can no longer reach it.
- Scale is a handful of agents; subscription rate limits are the binding constraint.

## 16. Second design-review pass (2026-11-16)
Reviewed the codebase (`src/approve.rs`, `src/amend.rs`, `src/config.rs`, `src/gate/`,
`src/lean/`, `src/engine/`) against `docs/references/alphaproof_code_signal.txt`.

Core alignment holds: heldout spec vs. agent work tree separation, hash-chained trust
anchor, AST/DAG closure fingerprinting + standard-axiom check in `gate`, structured
`architect` → `orchestrator` → `lead` → `falsifier`/`worker` → `integrator` loop with
`Proof/Work` vs. `Proof/Lib`, witness checks on new definitions, and spec-only `critic`
context.

Findings and divergences observed:
- **Escalation approval (`src/amend.rs`)**: `vdash amend approve` applied a
  critic-`escalate` proposal like any pending amendment, whereas §10 (and paper §2.3 /
  §7) say a guarantee-weakening escalation is never an amendment. Resolved with the
  user (2026-11-16): applying one needs `--accept-weakening` and is recorded as a
  human-accepted weakening (§10 step 3).
- **Engine recovery beyond the paper (`src/engine/`, `src/approve.rs`)**: structured
  output `repair` retries, `stale` / exhausted-goal handling, `vdash restart`, and
  engine-side `run_worker_checked` rebuild/retry loops (up to two diagnostic retries for
  sandboxed `agy --sandbox` workers).
- **`approve` checks (`src/approve.rs`)**: warns on `theorem` in `Spec/` and on
  stale/incomplete `spec-review.json` rather than blocking, as specified in M1-09.
- **Fixes landed in this pass**:
  - Updated `state.module_times` during proof rounds (`file_status`, `check_decls`,
    `adopt_orphans`, `integrate`) while ignoring cached `0.0s` rebuilds and pruning
    deleted modules, so `slow_modules` is populated for `orchestrator`/`integrator`.
  - Validated `campaign.toml`'s `package = "Apc"` at load time to match hardcoded `Apc`
    assumptions in `Fingerprint.lean`, `gate`, `paths`, and skill prompts.
  - Keyed `LeanPathCache` by `host_project` in addition to manifest mtime for
    `use_mathlib = true` campaigns.
  - Enriched `policy.comparator_cmd` with `VDASH_*`, `ELAN_TOOLCHAIN`, and
    `VDASH_HOST_LEAN_PATH` env vars plus stderr/status diagnostics in `detail.comparator`.
- **Remaining M7 gaps**: `comparator` (`landrun` + `lean4export`) is absent on this
  host, and no VibeMath campaign (`use_mathlib = true`) has completed end-to-end (an
  exploratory Mathlib campaign, `quartic-query-separation`, reached 2/5 targets on round 5).
