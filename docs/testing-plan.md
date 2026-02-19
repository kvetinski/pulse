# Distributed Pulse Testing Plan

## Unit tests
- Split planner:
  - rate split and concurrency split across slices
  - execution key generation stability
- Redis due-claim logic:
  - once scenarios only schedule once
  - every-interval scenarios schedule repeatedly
- Leader lock renew:
  - acquire, renew success, renew failure fallback

## Integration tests
- Redis + Kafka + pulse single node:
  - scheduled jobs appear in jobs topic
  - consumed jobs publish result records
- Multi-node pulse:
  - only one leader at a time
  - workers in the same consumer group divide jobs

## Failure tests
- Kill leader pod:
  - leadership transfers after lock TTL
  - scheduling resumes without manual intervention
- Kafka transient failure:
  - consumer retries and continues
- Redis transient failure:
  - leader loop reports loss and retries

## Load tests
- High-slice scenarios:
  - verify balanced work distribution across worker pods
- Duplicate-prevention behavior:
  - ensure idempotency store suppresses replay execution
