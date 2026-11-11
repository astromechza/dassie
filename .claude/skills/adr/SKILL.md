---
name: adr
description: Use when a change makes, changes or reverses an architectural decision in dassie (crate boundaries, protocols, runtime model, storage, deployment). Covers writing, numbering and superseding ADRs in architecture/.
---

# Architecture Decision Records

Decisions live in `architecture/` as short Nygard-style ADRs.

## Scope

- Write an ADR only for a decision **made by the change that adds it**, and land it in that same change.
- Do not backfill decisions made elsewhere (design docs, chat, earlier work).
- Skip non-architectural choices: license, formatting, tooling tweaks, dependency bumps.

## Writing one

1. Pick the next free number: `ls architecture/` and take the highest `NNNN` + 1.
2. Copy [template.md](template.md) to `architecture/NNNN-kebab-title.md` (zero-padded, 4 digits).
3. Fill in `Status`, `Date` (ISO), and the three sections. Keep it short: a few
   paragraphs at most. Context says what forces the decision; Decision says what
   we do, in active voice; Consequences lists what becomes easier, harder, or
   ruled out.
4. Add a row to the index table in `architecture/README.md`.

## Changing one

- Never renumber or delete an ADR.
- To reverse or replace a decision: write a new ADR, then set the old one's
  status to `Superseded by NNNN` and update both rows in the index.
- Typos and clarifications that don't change the decision can be edited in place.

Status values: `Proposed`, `Accepted`, `Superseded by NNNN`.
