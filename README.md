# Vdash (`⊢`)

```text
  ████╗                vdash@lean4
  ██╔═╝                ───────────
  ██║        ██╗   ██╗ App:      AlphaProof Code, rebuilt for Claude & Gemini
  █████████╗ ██║   ██║ Backends: claude · gemini (agy)
  ██╔══════╝ ╚██╗ ██╔╝ Verify:   Lean 4 kernel · leanchecker · held-out spec
  ██║         ╚████╔╝  Search:   Mathlib + Loogle (/search, /model, /login)
  ██║          ╚═══╝   Trust:    only the gatekeeper accepts a proof
  ╚═╝                  ███ ███ ███ ███ ███ ███ ███ ███
```

**Vdash** (`vdash`, named after the logical turnstile `⊢` in front of every Lean goal) is an open, subscription-friendly reimplementation of the multi-agent formal verification framework described in DeepMind's *AlphaProof Code: Verifying Signal's Security at the Machine-Code Level* (2026).

`vdash` is a single, self-contained Rust executable. It coordinates a hierarchical team of AI agents—**Spec Writer**, **Reviewers** (4 lenses), **Architect**, **Orchestrator**, **Team Leads**, **Falsifiers**, **Proof Workers**, **Integrator**, **Critic**, and **Monitor**—to draft, audit, and prove Lean 4 theorems against a specification you approve.

Agents run concurrently through your existing **Claude Code** (`claude`) and **Gemini / Antigravity** (`agy`) CLI subscriptions—no API keys required. If one subscription hits its rate or usage limit, shared roles spill over automatically to the other.

A deterministic **gatekeeper** accepts a solution only when Lean 4 elaborates it from scratch in a clean environment, every target theorem and its transitive definition closure match a held-out copy of the approved specification byte-for-byte in expression structure, no `sorry`, `native_decide`, custom axioms, or kernel bypasses appear anywhere, and `leanchecker` replays the kernel environment cleanly.

---

## 1. Required Dependencies

Before running a campaign, make sure the following are installed on your system:

1. **Lean 4 & Lake (via `elan`)**
   - Install [`elan`](https://github.com/leanprover/elan), which provides `lean`, `lake`, and `leanchecker`.
2. **A Host Lake Project with Mathlib Built**
   - `vdash` does not duplicate gigabytes of Mathlib `.olean` build artifacts per campaign. Instead, each campaign points at an existing Lean 4 Lake workspace on your machine (your `--host` directory) that has `lean-toolchain` and prebuilt `.olean` files (such as Mathlib):
     ```bash
     # If you don't already have a Lean/Mathlib workspace:
     mkdir ~/lean_math && cd ~/lean_math
     lake +leanprover/lean4:v4.33.0 init my_host math
     lake exe cache get && lake build
     ```
3. **Git**
   - Required (`git` on `PATH`) for campaign baseline commits, round snapshots, and audit trails.
4. **At Least One AI CLI Backend (`claude` and/or `agy`)**
   - **Claude Code CLI** (`claude` on `PATH`) — uses your Claude Pro / Max / Team subscription.
   - **Google Antigravity CLI** (`agy` on `PATH`) — uses your Gemini subscription.
   - Having **both** installed is recommended so specification writing and reviewing come from distinct model families and proof rounds run across both pools concurrently.
5. **Optional Tools**
   - `curl`: used by `vdash search` to query the online [Loogle](https://loogle.lean-lang.org) Mathlib4 search API in addition to local `.lean` file search.
   - `lean4export` & `comparator`: if on `PATH`, the gatekeeper runs external export comparison alongside `leanchecker`.

Run `vdash doctor` inside any campaign to verify all tools, toolchain paths, `leanchecker`, and account logins.

---

## 2. Building & Installing `vdash`

Clone the repository, build the optimized release binary with Cargo and copy it onto your `PATH` (for example `~/.local/bin/vdash`). A recent stable Rust toolchain is required (developed on 1.98); there are no C-library dependencies.

```bash
git clone https://github.com/noncanonicAleae/vdash.git
cd vdash
cargo build --release
cp target/release/vdash ~/.local/bin/vdash
vdash --version
```

The compiled `target/release/vdash` binary embeds all role skills, JSON schemas, and the built-in insertion-sort (`isort`) example, so it runs from any directory without needing the source repository at runtime.

---

## 3. Signing In to Claude & Gemini Accounts

You can check your account status and sign in to either subscription directly from `vdash`—either from the shell or from inside the interactive TUI:

### From the CLI
```bash
vdash login           # Check login status of both Claude and Gemini
vdash login claude    # Launch interactive Claude OAuth sign-in
vdash login gemini    # Launch interactive Gemini (agy) OAuth sign-in
```

### From Within the Interactive App
```text
/login                # Show sign-in status for claude and gemini
/login claude         # Temporarily suspend the TUI, sign in to Claude, and resume
/login gemini         # Temporarily suspend the TUI, sign in to Gemini, and resume
```

---

## 4. Creating & Running a Campaign

### `vdash init` Syntax
```bash
vdash init <CAMPAIGN_DIR> --host <HOST_LAKE_PROJECT_DIR> [--example isort]
```
- `<CAMPAIGN_DIR>`: new or empty directory to create for the campaign.
- `--host <HOST_LAKE_PROJECT_DIR>`: path to your Lean 4 Lake project containing `lean-toolchain` (and `.lake/packages/mathlib` if using Mathlib).
- `--example isort` *(optional)*: populates the campaign with a complete, self-contained insertion-sort specification (`Impl.lean`, `Defs.lean`, `Mutants.lean`, `Tests.lean`, and 4 target theorems in `Challenge.lean`) so you can test the proof engine immediately.

### How `vdash` Finds Your Campaign
Once a campaign is created, you can run `vdash` in any of three ways:
1. **`cd` into the campaign directory** (or any subdirectory) and run `vdash`.
2. **Pass `-C <CAMPAIGN_DIR>`** from anywhere: `vdash -C ~/campaigns/my_proof`.
3. **Set `VDASH_CAMPAIGN`**: `export VDASH_CAMPAIGN=~/campaigns/my_proof`.

---

## 5. End-to-End Workflows

### Workflow A: Quick Start with the Built-In `isort` Example
```bash
# 1. Create the campaign pointing at your Lean host project
vdash init /tmp/isort_demo --host ~/lean_math --example isort
cd /tmp/isort_demo

# 2. Check your setup, approve the spec, and open the interactive TUI
vdash doctor
vdash approve
vdash
```
Inside the TUI:
- Type `/prove` (or `/prove single` for single-agent mode) to launch the proof engine in the background.
- Watch the live progress bar, active agents, and gatekeeper verdicts in real time.

### Workflow B: Starting Your Own Theorem or Verification Problem
```bash
vdash init ~/campaigns/my_theorem --host ~/lean_math
cd ~/campaigns/my_theorem
vdash
```
1. **Provide Your Specification Brief (or PDF/Markdown Sources)**:
   - You can drop any reference papers, **PDF specifications**, notes, or code into `sources/` (e.g. `sources/spec.pdf`, `sources/notes.md`).
   - In the `vdash` TUI, simply **type what you want proved in plain English** (or reference the files in `sources/`).
   - `vdash` saves your initial prompt as `sources/BRIEF.md`, runs the **Spec Writer** to draft `lean/Apc/Spec/*.lean` and `lean/Apc/Challenge.lean`, and automatically runs **4 parallel Reviewers** (`soundness`, `strength`, `fidelity`, `legibility`) to audit the draft.
2. **Iterate on the Spec**:
   - Type plain-text feedback in the TUI (or `/spec <feedback>`) to have the Spec Writer revise the Lean definitions and target theorems, followed by another 4-lens review (`/review` reruns the reviewers).
   - The spec phase **loops on its own**: while any reviewer says `revise`, the Spec Writer revises against the latest review and the reviewers run again, until all four say `ok`, you say it's good enough (`/stop`, or `/approve`, which stops the loop and then approves), or `[budget] spec_max_rounds` rounds (default 10) have run. Plain text typed during the loop is queued as guidance for the next revision. CLI: `vdash spec draft|revise|review` loop too; add `--once` for a single step.
3. **Freeze & Approve the Spec**:
   - Once the review reports `0 blocker` findings and you are happy with `lean/Apc/Challenge.lean`, run `/approve` (or `vdash approve`).
   - This verifies that `Spec/*` has no `sorry` or `axiom`, checks that every target in `Challenge.lean` has a `sorry` placeholder, records a baseline Git commit, copies the trusted spec into `.heldout/lean` (read-only), and writes a SHA-256 trust anchor outside the campaign directory (`~/.local/state/vdash/<id>/anchor.json`).
4. **Prove the Targets**:
   - Run `/prove` in the TUI (or `vdash prove` on the CLI).
   - While the team is proving, any plain text you type in the TUI is sent as live **guidance** to the Orchestrator for the next round.
   - Ask side questions at any time without interrupting the run using `/btw <question>`.

---

## 6. Interactive TUI Reference

Run `vdash` with no subcommand inside a campaign to open the terminal UI.

### Slash Commands

| Command | Description |
|---|---|
| `/help` | List all slash commands and usage |
| `/status` | Show campaign phase, target verdicts, goal counts, and amendment counts |
| `/login [claude\|gemini]` | Check account status or sign in interactively to Claude or Gemini |
| `/model` (or `/model list`) | Show the role → `backend[:model]` table, backend health, and available models |
| `/model <role\|all> <backend>[:<model>]` | Assign a role (or `all` roles) to a backend and optional model |
| `/model <role\|all> <backend> <model>` | Three-argument shorthand, e.g. `/model worker claude opus` |
| `/model <claude\|gemini> <model>` | Set the model for every role currently assigned to that backend |
| `/model default` | Restore the default split (Claude: planning, leads, integrator, critic, monitor; Gemini: reviewer, falsifier; workers on both). Also `vdash model default` |
| `/search <query>` | Search local Mathlib/campaign `.lean` files and online Loogle documentation |
| `/btw <question>` | Ask the read-only Monitor agent a side question while work continues |
| `/spec [feedback]` | Draft the spec from `sources/BRIEF.md`, or revise it with feedback |
| `/review` | Rerun the four spec reviewers (`soundness`, `strength`, `fidelity`, `legibility`) |
| `/approve` | Freeze and approve the spec (baseline commit, `.heldout/`, and trust anchor) |
| `/prove [single]` | Start the proof engine in the background (`single` runs one worker without leads) |
| `/pause`, `/resume`, `/stop` | Pause, resume, or cleanly stop a running proof engine |
| `/kill` | Kill the engine and every agent subprocess immediately, for good (see below) |
| `/verbose [on\|off]` | Toggle (or set) live display of the Lean lines agents add to the proof files as they write them |
| `/gate` | Run the deterministic gatekeeper immediately |
| `/tcb` | Display the Trusted Computing Base (every target's transitive definition closure) |
| `/amend` | List proposed specification amendments and their Critic verdicts |
| `/amend approve <N>` / `reject <N>` | Apply or reject pending amendment `#N`; `reject` also dismisses an escalation |
| `/amend approve <N> --accept-weakening` | Apply an escalated proposal, knowingly accepting a weaker guarantee |
| `/amend propose <file.json>` | Submit an amendment JSON proposal to the Critic |
| `/agents` | List agents currently running with elapsed times |
| `/logs <agent-id>` | Inspect one agent's full JSON reply, cost, and transcript path (e.g. `/logs worker-3`) |
| `/bulletin` | Show the last 20 shared team notices from `bulletin.md` |
| `/cost` | Show call count, runtime, and USD cost breakdown by role and backend |
| `/doctor` | Check external binaries, Lean toolchain, `leanchecker`, and logins |
| `/clear` | Clear the on-screen transcript |
| `/quit` | Exit the TUI (a detached proof engine continues running in the background) |

#### `/stop` versus `/kill`
`/stop` is cooperative: the engine finishes the agent it is waiting on, reverts out-of-scope edits and exits. `/kill` (`vdash kill`) is for when you want everything gone now. It sends SIGTERM to the engine and to every process it started (the `claude` / `agy` calls and their children), waits up to 8 seconds, then SIGKILLs whatever is left. A round killed midway is not reverted on the spot; the next engine start snapshots the tree again, and the gatekeeper never trusts agent edits regardless.

#### `/verbose`
Agents edit Lean files in place, so by default you only see their summaries. With `/verbose` on, the transcript also shows each new line that appears in `lean/Apc/Proof/**`, the proof work directory and `Solution.lean` (at most 14 lines per file per update). Turn it off with `/verbose off` (or `/verbose` again).

### Keyboard Shortcuts
- **`Enter`**: Send command or plain-text message (or accept selected completion).
- **`Tab` / `Shift-Tab`**: Autocomplete slash commands, role names, backend names, model names, and agent IDs.
- **`↑` / `↓`**: Navigate command history (or move through the autocomplete popup).
- **`PgUp` / `PgDn`** (or mouse wheel): Scroll the transcript.
- **`Ctrl-A` / `Ctrl-E` / `Ctrl-W` / `Ctrl-U`**: Move to start/end of line, delete previous word, or clear input.
- **`Ctrl-C` twice** (or **`Ctrl-D`**): Quit the TUI (leaving any background engine running).

---

## 7. Choosing Claude & Gemini Models

Every agent role can be routed to either `claude`, `gemini`, or a list of both (`["claude:sonnet", "gemini:gemini-4-argon-high"]`).

### Built-In Model Catalog (with Tab Completion)
- **Claude (`claude`)**: aliases `opus`, `sonnet`, `haiku`, `fable`, and the pinned ids
  `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`, `claude-fable-5-2`.
  All of them work on a Pro subscription; Fable costs about three times Opus per call.
- **Gemini (`agy`)**: Gemini 4 Argon `gemini-4-argon-{low,medium,high}`, Gemini 4
  Hydrogen `gemini-4-hydrogen-{low,medium,high,xhigh}`, Gemini 3.8 Flash
  `gemini-3.8-flash-{low,medium,high}`. The suffix is the reasoning level. `agy models`
  prints the live list.

Any other id the CLI accepts also works; the catalog only drives completion.

### Examples (in the TUI or via `vdash model`)

Inside the interactive TUI:
```text
/model                                           # Show current assignments and model catalog
/model worker gemini:gemini-4-argon-high         # Set worker to Gemini 4 Argon High
/model worker claude opus                        # Set worker to Claude Opus
/model claude claude-sonnet-5-5                  # Switch all Claude roles to claude-sonnet-5-5
/model gemini gemini-4-hydrogen-high             # Switch all Gemini roles to gemini-4-hydrogen-high
/model all claude:sonnet                         # Put every role on Claude Sonnet
/model all gemini:gemini-4-argon-high            # Put every role on Gemini 4 Argon High
```

From the command line (using `-C <DIR>` or from inside the campaign directory):
```bash
vdash -C <DIR> model                                        # Show role assignments and model catalog
vdash -C <DIR> model worker gemini:gemini-4-argon-high      # Set worker to Gemini 4 Argon High
vdash -C <DIR> model claude claude-sonnet-5-5               # Switch all Claude roles to claude-sonnet-5-5
vdash -C <DIR> model all gemini:gemini-4-argon-high         # Move every role onto Gemini
```

When an engine is running, `/model` (or `vdash model`) delivers the change live via the control inbox. The backend pool picks it up on its next backend choice, so it also rescues an agent that is waiting on a backend's usage limit: the retry goes to the new model at once (an agent already running is not interrupted). When no engine is running, it updates `campaign.toml` in place while preserving your comments.

### Switching Subscriptions When One Runs Out of Quota

By default, planning roles (`architect`, `orchestrator`, `team_lead`, `integrator`, `critic`, `monitor`) are pinned to `claude`, reviewing/falsifying roles (`reviewer`, `falsifier`) are pinned to `gemini`, and `worker` is shared across `["claude", "gemini"]`.

If one of your subscriptions runs out of quota mid-campaign:

1. **Switch roles to the remaining backend and resume in the background**:
   ```bash
   # Stop the waiting engine, switch all roles to Gemini, and restart detached:
   vdash -C <CAMPAIGN_DIR> stop
   vdash -C <CAMPAIGN_DIR> model all gemini:gemini-4-argon-high
   vdash -C <CAMPAIGN_DIR> prove --detach
   ```
   *(Or to move everything onto Claude: `vdash -C <CAMPAIGN_DIR> model all claude:opus`.)*
   Resuming `vdash prove` automatically picks up from the current round and stage in `state.json` without redoing already-completed stages.

2. **Enable automatic cross-backend spillover in `campaign.toml`**:
   If you want pinned roles (such as `team_lead` or `orchestrator`) to automatically fall over to the other subscription whenever their primary backend hits a usage limit, set `spillover = true` under `[policy]` in `campaign.toml`:
   ```toml
   [policy]
   spillover = true
   ```

---

## 8. Managing Background Proof Runs from the CLI

You can run and control the proof engine either inside the TUI or headlessly from the shell:

```bash
# Start the proof engine in the background (logs to logs/engine.out and logs/events.jsonl):
vdash -C <CAMPAIGN_DIR> prove --detach

# Run for at most N rounds, or in single-worker mode (no Architect/Orchestrator/Team Leads):
vdash -C <CAMPAIGN_DIR> prove --rounds 3
vdash -C <CAMPAIGN_DIR> prove --single

# Check live progress, target verdicts, and proved/open goal counts:
vdash -C <CAMPAIGN_DIR> status

# Ask the read-only Monitor agent a side question while the engine is running:
vdash -C <CAMPAIGN_DIR> btw "Which lemmas are currently blocking quartic_upper_bound?"

# Pause, resume, or cleanly stop a running engine (takes effect at the next checkpoint):
vdash -C <CAMPAIGN_DIR> pause
vdash -C <CAMPAIGN_DIR> resume
vdash -C <CAMPAIGN_DIR> stop

# Kill the engine and all agent subprocesses right now (no waiting for a checkpoint):
vdash -C <CAMPAIGN_DIR> kill

# Run the deterministic gatekeeper at any time (human-readable or JSON output):
vdash -C <CAMPAIGN_DIR> gate
vdash -C <CAMPAIGN_DIR> gate --json
```

---

## 9. Searching Mathlib & Online Documentation

Both you and the AI agents can search local Lean/Mathlib files and online documentation:

```bash
# Search both online Loogle (Mathlib4) and local Mathlib / campaign .lean files:
vdash search "List.Perm"
vdash search "Finset.sum_nonneg"

# Search only local .lean files (offline mode, or set VDASH_OFFLINE=1):
vdash search --offline "isort_correct"
```
Inside the TUI, run:
```text
/search List.Perm
```
During spec writing, planning, falsification, and proving, agents can invoke `vdash search "<query>"` as well as `WebSearch` and `WebFetch` to look up Mathlib4 theorem names, type signatures, and Lean 4 documentation.

---

## 10. CLI Subcommand Reference

Every operation in the TUI has a corresponding CLI command for scripting or headless use:

```bash
vdash [-C <DIR>] init <DIR> --host <HOST_DIR> [--example isort]
vdash [-C <DIR>] login [claude|gemini]
vdash [-C <DIR>] doctor
vdash [-C <DIR>] model [<role|all|claude|gemini> <backend[:model]|model>]
vdash [-C <DIR>] search [--offline] <QUERY>...
vdash [-C <DIR>] spec draft [--once]
vdash [-C <DIR>] spec revise "<FEEDBACK>" [--once]
vdash [-C <DIR>] spec review [--once]
vdash [-C <DIR>] approve
vdash [-C <DIR>] check <LEAN_FILE>
vdash [-C <DIR>] axioms <LEAN_FILE> <DECL_NAME>...
vdash [-C <DIR>] tcb
vdash [-C <DIR>] gate [--json]
vdash [-C <DIR>] prove [--rounds <N>] [--single] [--detach]
vdash [-C <DIR>] status
vdash [-C <DIR>] pause | resume | stop | kill
vdash [-C <DIR>] btw "<QUESTION>"
vdash [-C <DIR>] amend list | approve <N> [--accept-weakening] | reject <N> | propose <FILE.json>
```

### Exit Codes
| Code | Meaning |
|---|---|
| `0` | Success (all checks passed / all targets accepted) |
| `1` | Checked and failed (e.g. Lean compile error, unaccepted gate target, or failed doctor check) |
| `2` | Usage or configuration error (bad arguments, invalid `campaign.toml`, missing campaign) |
| `3` | Trust violation (held-out spec or external trust anchor was tampered with) |
| `4` | Backends exhausted (all configured subscriptions are unauthenticated or over quota past `max_quota_wait_seconds`) |
| `5` | Internal or I/O error |

---

## 11. Campaign Directory Layout & Trust Model

```text
<campaign>/
├── campaign.toml            # Host path, backends, per-role models, budgets, and policy
├── state.json               # Live phase, round, target verdicts, goal plan, and cost usage
├── STRATEGY.md              # Human guidance read by the Architect and Orchestrator
├── bulletin.md              # Append-only shared notices across agent rounds
├── sources/                 # Your problem brief (BRIEF.md), PDFs, papers, or reference code
├── spec/                    # Spec review reports (review-N.md) and DEPARTURES.md
├── lean/
│   └── Apc/
│       ├── Spec/            # TRUSTED definitions, implementation under test, and sanity tests
│       ├── Challenge.lean   # TRUSTED target theorems (each := by sorry)
│       ├── Solution.lean    # UNTRUSTED top-level proof file importing Proof/ and proving targets
│       └── Proof/
│           ├── Lib/         # UNTRUSTED shared lemma library maintained by the Integrator
│           └── Work/        # UNTRUSTED per-goal scratch files and falsifier refutations
├── .heldout/lean/           # Read-only copy of the approved Spec/ and Challenge.lean
├── amendments/              # Spec amendment records and Critic evaluations
├── escalations/             # Escalations requiring human decision
└── logs/                    # Full JSON transcripts of every agent call and events.jsonl
```

### How Soundness Is Enforced
- **Trusted Computing Base (TCB)**: Only `lean/Apc/Spec/**`, `lean/Apc/Challenge.lean`, Lean 4's kernel, and `vdash`'s gatekeeper are trusted. Everything written by an agent during proving is untrusted.
- **Automatic Policy Revert**: Before every agent round, `vdash` snapshots the campaign tree. Any out-of-scope file edit by a worker, falsifier, or integrator—including any attempt to touch `Spec/`, `Challenge.lean`, or `.heldout/`—is automatically reverted and logged as a violation.
- **External Trust Anchor**: Upon `vdash approve`, the SHA-256 hashes of every trusted file and the expression-DAG fingerprints of every target theorem's full `Apc.*` constant closure are stored outside the campaign in `~/.local/state/vdash/<id>/anchor.json` (`0444`).
- **Independent Gatekeeper (`vdash gate`)**: Rebuilds the held-out tree and solution tree in fresh temporary directories, verifies all expression-DAG fingerprints against the external anchor, scans all untrusted Lean sources for forbidden tokens (`sorry`, `admit`, `axiom`, `native_decide`, `unsafe`, `implemented_by`, `extern`, `run_cmd`, `by_elab`, `macro`, `elab`, `syntax`, `notation`, `local_notation`, `scoped`, `set_option debug.*`), checks that each target uses only standard axioms (`propext`, `Quot.sound`, `Classical.choice`), and runs `leanchecker` on the fresh environment.

---

## 12. Background & Status

- Vdash is an independent reimplementation. DeepMind released only the Lean artifacts ([google-deepmind/code-verification](https://github.com/google-deepmind/code-verification)), not the agent framework; the design here follows the paper's description. The paper itself is not redistributed in this repository: fetch it from its official source and, if you want `docs/PAPER_NOTES.md` references to resolve, save the text as `docs/references/alphaproof_code_signal.txt` (git-ignored).
- Design and decisions: [`docs/DESIGN.md`](docs/DESIGN.md). Implementation spec and status log: [`docs/ROADMAP.md`](docs/ROADMAP.md). Architecture diagrams: [`docs/architecture.html`](docs/architecture.html).
- Run the tests with `cargo test` (the Lean-backed suites need a host project: set `VDASH_TEST_HOST` to it, or `APC_SKIP_LEAN=1` to skip them).
- Licensed under the [MIT License](LICENSE).
