# 0001. Record architecture decisions

Status: Accepted
Date: 2026-11-11

## Context

Dassie is a new, single-maintainer project. Design discussion happens in
external notes and in chat sessions, which drift and are not versioned with the
code. We want the reasoning behind structural choices to sit next to the code
that embodies them.

## Decision

Record architectural decisions as short Nygard-style ADRs in `architecture/`,
numbered `NNNN-kebab-title.md`. An ADR is written only for a decision made by
the change that adds it and lands in the same change; decisions are not
backfilled from external documents. The conventions live in the project's `adr`
skill (`.claude/skills/adr/`) so both people and Claude sessions follow them.

## Consequences

- Each change that alters architecture carries its own rationale in review.
- External design notes remain useful for goals and open questions but are not
  the record of what was decided.
- ADRs are append-only; reversing a decision means a new ADR that supersedes
  the old one.
