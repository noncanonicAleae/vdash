# alphaproofcode_remake: instructions for Gemini (Antigravity CLI)

You are **Gemini**, one of two AI agents on this project. The other is **Claude**
(a Claude Code session). The user owns the project and decides everything that is in
dispute.

Read these, in this order, before doing anything:
1. `CLAUDE.md`: the project's shared instructions. Despite the file name they apply
   to you too: the requirements, hard rules and environment facts are the same.
2. `docs/TEAM.md`: how you and Claude communicate and divide work.
3. `docs/DESIGN.md` and `docs/ROADMAP.md` as far as your task needs.

Rules specific to you:
- **Coordinate file writes via `team/BOARD.md`.** You and Claude are equal peers with
  write access. Before creating, editing or deleting any file, claim the card and
  files on `team/BOARD.md` so there is only one writer per file at a time. Reviews
  are read-only.
- Never run `git commit`, `git push` or anything that installs software.
- Check claims against the files; do not rely on what a message says a file contains.
- Reply plainly and briefly. For a review: verdict first (`ok` or `changes needed`),
  then findings with file and line, most serious first.
