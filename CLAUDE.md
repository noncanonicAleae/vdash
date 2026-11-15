# alphaproofcode_remake

An open reimplementation of the agent framework from DeepMind's **AlphaProof Code**
paper (Chaudhuri et al., 2026-11-10, "AlphaProof Code: Verifying Signal's Security at
the Machine-Code Level"). DeepMind released only the Lean artifacts
(github.com/google-deepmind/code-verification). They did not release the agent framework.
This project rebuilds that framework as **Vdash** (binary `vdash`; renamed from `apc`
2026-11-14, Lean root namespace still `Apc`), a standalone Rust program that runs on
**Claude and Gemini subscriptions**. It drives the `claude` / `agy` CLIs in headless
mode and never uses an API key.

Read these before working:
- `docs/DESIGN.md`: the architecture, the decisions already made and why. Follow it.
  Don't re-litigate it.
- `docs/ROADMAP.md`: the implementation spec: verified facts, contracts, schemas, and
  one card per unit of work with its tests. Update the boxes and status log as you go.
- `docs/TEAM.md`: Gemini (Antigravity CLI) works on this project too. Read the end of
  `team/BOARD.md` before starting a piece of work; talk to Gemini with
  `team/ask-gemini "message"`.
- `docs/PAPER_NOTES.md`: what the paper says about the framework, with references.
- `docs/architecture.html`: diagrams (also published as a Claude artifact).
- `docs/references/alphaproof_code_signal.txt`: the full paper text. Use grep on it;
  don't read the whole file.

## User requirements (2026-11-12)
1. **Written in Rust.** Any crate is acceptable as long as the result is a clean,
   standalone, lightweight executable.
2. **A nice CLI**: an interactive terminal app covering both the spec conversation
   and a live view of a running campaign, plus scriptable one-shot commands.
3. **Slash commands inspired by Claude Code**, such as `/model` and `/btw`.
4. **Claude and Gemini subscriptions running concurrently**, split by role by
   default, with the option to choose.

## Hard rules
- **Subscription only.** Agents run as `claude -p ...` / `agy -p=...` subprocesses.
  Strip `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` / `GOOGLE_API_KEY` from the child
  environment by default, so the CLIs fall back to the logged-in subscription. Never
  pass `claude --bare`.
- **One standalone binary.** No async runtime, no C-library dependencies, no runtime
  data files: skills and the Lean fingerprint script are embedded at compile time.
- **Never trust an agent's claim that a goal is done.** Only the deterministic
  gatekeeper marks a target accepted. Lemma-level "proved" claims are re-verified the
  same way.
- **Code enforces trust boundaries, not prompts.** Snapshot the campaign before each
  agent round and revert any edit outside the files assigned to that round. Always
  revert edits to trusted files (outside the spec phase and accepted amendments).
- Skills (role prompts) are Markdown files in `skills/`. Change agent behaviour there,
  not in string literals in the code.
- Keep it small. Finish an end-to-end path on the toy example before adding features.

## How to implement
Follow `docs/ROADMAP.md` §0: cards in ID order, tests first, tick a box only when
every "Done when" line holds, and fix the document before deviating from it.

## Environment facts (verified 2026-11-12)
- Host Lean project: `~/lean_math` (toolchain `leanprover/lean4:v4.33.0`,
  Mathlib under `.lake/packages`). `lake env printenv LEAN_PATH` run there gives the
  Mathlib search path.
- Outside that directory `lean` resolves to a different toolchain; set
  `ELAN_TOOLCHAIN` from the host's `lean-toolchain` for every Lean subprocess.
- `claude` is at `~/.local/bin/claude` (v2.1.322).
- The Gemini backend is Google's **Antigravity CLI**, `agy` v1.5.7 at
  `~/.local/bin/agy` (Gemini CLI is being retired in its favour). It is logged in and
  one headless call has been verified; tool and permission behaviour is untested
  until the roadmap's M2-02 probe has been run. An unused `@google/gemini-cli` is also
  installed under bun.
- `cargo` / `rustc` 1.98.1.
- The full list of probed facts is `docs/ROADMAP.md` §1. Trust that list over memory.

## Conventions
- CLI entry point: the `vdash` binary. With no subcommand it opens the interactive app.
- Campaign layout, trust split and reply schemas are in `docs/DESIGN.md` and
  `docs/ROADMAP.md` §3.
- Test with `examples/isort/` (toy campaign, no Mathlib) before anything bigger.
