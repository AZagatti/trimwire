---
name: Feature request
about: Suggest a new feature or improvement
title: ''
labels: enhancement
assignees: ''
---

## The problem

What pain point would this solve? Concrete, not abstract.

## Proposed solution

What would the feature look like? (CLI command? config option? new strategy?)

## Alternatives considered

What else would solve the same problem? Why prefer the proposed approach?

## Scope check

trimwire is deliberately small — see [`SPIKE.md` §8](../../SPIKE.md) for the
build / defer / document tier split. Before opening:

- [ ] I've read the spike and this isn't already deferred / documented
- [ ] This doesn't require touching the system prompt or other forbidden surfaces (see [`AGENTS.md`](../../AGENTS.md))
- [ ] If it's a new strategy, I've sketched how it would respect the pairing invariants (SPIKE.md §5)
