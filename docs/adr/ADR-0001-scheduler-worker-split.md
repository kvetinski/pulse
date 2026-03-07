# ADR-0001: Scheduler/Worker Split

- Status: Accepted
- Date: 2026-03-06

## Context

Pulse must both schedule scenario jobs and execute them across replicas. We need a deployment model that is simple to operate and scales horizontally.

## Decision

Run both scheduler and worker loops in every Pulse pod, but allow only one active scheduler via leader election.

## Consequences

- One binary and one deployment role for all replicas.
- Horizontal scaling adds worker capacity without additional service types.
- Slight per-pod overhead from keeping both loops available.

## Considered Alternatives

1. Separate scheduler deployment and worker deployment.
- Pros: clear role isolation, independent scaling knobs.
- Cons: more operational complexity, additional rollout and failure modes.

2. Single-node scheduler+worker only.
- Pros: very simple runtime.
- Cons: no distributed scale and no scheduler failover.
