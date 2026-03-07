# ADR-0002: Redis Leader Election

- Status: Accepted
- Date: 2026-03-06

## Context

Only one scheduler should publish due scenario jobs at a time. Pulse already depends on Redis for runtime coordination.

## Decision

Use Redis lock semantics (`SET NX PX`) with owner-verified renew/relinquish scripts for leader election.

## Consequences

- Lightweight single-active scheduler election.
- Reuses existing Redis dependency and keeps implementation small.
- Correctness depends on Redis availability and TTL/renew tuning.

## Considered Alternatives

1. Kubernetes Lease API.
- Pros: native control-plane primitive for leader election.
- Cons: requires Kubernetes API access assumptions and cluster-coupled runtime behavior.

2. Consensus system (etcd/raft-based).
- Pros: stronger distributed guarantees.
- Cons: significantly higher operational and implementation complexity for current scope.

3. No leader election (all schedulers active).
- Pros: minimal implementation.
- Cons: duplicate scheduling and unstable load behavior.
