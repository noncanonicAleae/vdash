# Working as a team: the user, Claude and Gemini

Two AI agents build this project alongside the user:
- **Claude**: a Claude Code session in this directory. Reads `CLAUDE.md`.
- **Gemini**: Google's Antigravity CLI (`agy`). Reads `AGENTS.md`.

The user decides scope, priorities and every disagreement.

## Who does what (confirmed by the user, 2026-11-12)
- **Claude and Gemini are equal peers** with full write access; neither has a
  hierarchical upper hand.
- Either agent can implement roadmap cards and update docs after claiming the card
  and files on `team/BOARD.md`.
- The other agent reviews each finished card against its roadmap specification before
  the box is ticked.
- Review by the other model family is deliberate: it is what the paper did for specs.

## The channel
- **`team/BOARD.md`** is the shared, append-only record. Every message between the
  agents is on it, so the user can read the whole exchange. Never rewrite or delete
  entries.
- **Claude → Gemini**: `team/ask-gemini "message"` (or the message on stdin). It runs
  `agy` in this directory, continues the same Gemini conversation across calls, and
  appends both the message and the reply to the board. `--new` starts a fresh
  conversation; `GEMINI_MODEL` picks a model.
- **Gemini → Claude**: Gemini cannot start a turn in Claude's session. When it has
  something for Claude outside a reply, it appends an entry to the board headed
  `## <date> · Gemini → Claude`. Claude reads the end of the board before starting
  each piece of work.
- **The user → either**: say it in whichever session is open; the receiving agent
  copies anything the other needs onto the board.

Entry format:
```
## 2026-11-12 14:05 · Claude → Gemini

message text
```

## Ground rules
1. `docs/ROADMAP.md` §0 applies to both agents.
2. **One writer per file at a time.** Before editing, say on the board which card and
   which files you are taking. Neither agent edits a file currently claimed by the
   other; reviews are read-only. (Antigravity's `--mode plan` does not prevent
   writes, so this rule is by agreement, not enforced.)
3. Neither agent takes the other's word that something works. Tests and the gate
   decide.
4. Keep messages self-contained: name the card, the files and the exact question. The
   other agent does not share your context.
5. A review reply states: verdict (`ok` / `changes needed`), then findings with file
   and line, most serious first.
6. A disagreement that two exchanges do not settle goes to the user, with both
   positions stated in two or three lines each.
7. No secrets, keys or personal details on the board.
