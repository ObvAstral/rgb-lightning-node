# Explicit per-payment routing

Every `POST /sendpayment`, `POST /keysend` and `POST /makerexecute` request must
include a `routing` object with an explicit `algorithm`. Omitted, null or unknown
algorithms are rejected during JSON deserialization, before submitting a payment.
There is no `auto` strategy, default algorithm or automatic algorithm fallback.
This is an intentional API change: clients, including AIR-GB, must supply a policy.

```json
{
  "invoice": "<BOLT11 invoice>",
  "routing": {
    "algorithm": "yen",
    "max_candidates": 32,
    "max_paths": 4,
    "max_fee_msat": 5000,
    "timeout_ms": 1000
  }
}
```

The existing router remains available by explicitly specifying
`{"algorithm":"ldk_dijkstra"}`. The other identifiers are `yen`, `min_cost_flow`
and `multi_objective`. `GET /routing/capabilities` lists supported algorithms.
This daemon does not run an AI model. The caller selects an algorithm; Rust
constructs and validates the route against current local state.

## Implementation

- `adaptive_router.rs` implements LDK's `Router` for the actual `ChannelManager`,
  including automatic retries. An uncorrelated `find_route` call fails closed.
  Swap legs use a separate method with an explicitly supplied policy. Alternative
  planners reuse LDK's randomized CLTV privacy offset before final validation.
- `policy.rs` validates the request and durably binds its immutable policy to
  the payment ID in the LDK KV store (`routing/policies/`). Policies survive
  process restarts. Missing or corrupt policies fail routing, including retries
  of legacy in-flight payments; no strategy is inferred for them.
- `algorithms/graph.rs` snapshots public gossip, usable local channels and invoice
  route hints. It filters contracts, unknown required features, exclusions and
  failed channels, and accounts for directed BTC/RGB already in flight. Local
  capacity already excludes pending HTLCs and is not subtracted twice.
- `validation.rs` checks every clear route, including the LDK strategy: exact
  requested BTC/RGB totals, consistent contracts and positive RGB parts, directed
  shared capacities, forwarding fees, HTLC limits, CLTV, MPP support, hop/path
  limits and exclusions. Blinded LDK routes receive structural/amount/fee checks;
  hidden channel capacities cannot be inspected.
- `telemetry.rs` records planner decisions and actual LDK path/payment outcomes.
  Payment preimages and wallet keys are not recorded.

The `ChannelManager` uses the concrete adaptive router rather than the LDK
`SimpleArcChannelManager` alias that fixes `DefaultRouter`. The fork's transient
`InFlightHtlcs` now includes a second directional map for RGB units, alongside
BTC. Its local serialization round-trips both maps; this is not a change to
channel persistence or Lightning onion/wire formats.

## Algorithms and limits

| Algorithm | Implementation | Important limits |
| --- | --- | --- |
| `ldk_dijkstra` | Existing RGB-aware reverse Dijkstra/MPP heuristic, followed by common validation | Supports clear and blinded recipients. LDK search itself is not preempted by `timeout_ms`. |
| `yen` | Yen's K shortest loopless paths; bounded uniform-cost spur searches; greedy allocation across candidates | Additive search cost estimates fees and LDK penalties at the requested amount. K and search budgets limit exploration. |
| `min_cost_flow` | Integral successive shortest augmenting paths with reverse residual arcs; positive flow decomposed into HTLC paths | Linear approximation of BTC/RGB proportional flow. Costs amortize base fees; a BTC fee reserve is used. Fixed fees, HTLC minima, rounding, CLTV and path count can make the final flow invalid. Such a flow is rejected. |
| `multi_objective` | Bounded multi-label Pareto search over fee, LDK penalty, CLTV and hop count; caller weights rank solutions | Limited labels/candidates and greedy allocation can miss a feasible or better split. |

The three alternatives support clear BOLT11 and keysend recipients, including
private invoice hints. BOLT12 offers require explicit `ldk_dijkstra`; hard node
or channel exclusions are rejected for offers because hidden hops cannot be
checked. Swaps plan each leg with the explicit strategy, retain the existing
single-path ring construction, and enforce the total swap fee cap.

For the alternative planners, final BTC and integer RGB amounts are split
proportionally, with exact remainder handling. The two channel constraints are
checked independently. These are bounded experimental planners, not a proof of
global optimality for the full two-resource, fixed-charge routing problem.
They can reject a payment that another strategy could route. Failure never
changes the selected algorithm.

A multi-objective policy must include explicit nonnegative integer weights,
each at most 1000000, with at least one positive weight:

```json
{
  "algorithm": "multi_objective",
  "weights": {"fee": 1, "reliability": 2, "cltv": 1, "hops": 10}
}
```

Weights multiply raw metrics: fee msat, LDK penalty msat, CLTV blocks and hop
count. `reliability` uses the LDK scorer for public channels; private hints use
an uncertainty penalty. It is not a learned RGB liquidity model or a measured
probability of success. Remote liquidity remains uncertain and may be stale.

`max_fee_msat`, `max_paths` and `max_cltv_delta` tighten existing payment limits.
The initial fee cap is also given to LDK so its remaining budget is preserved
across retries. A retry plans only its requested residual BTC/RGB quantities;
the original MPP totals remain managed by LDK. Policy parameters are immutable
for a payment ID. Retries use fresh state with the same algorithm and policy.

`max_candidates` defaults to 32 (range 1–256), `max_expansions` to 100000
(range 1–2000000), and `timeout_ms` to 1000 (range 1–10000). These defaults
bound parameters only; the algorithm is always required. Search limits are
cooperative and do not interrupt a running LDK baseline search.

## Telemetry and verification

`GET /routing/telemetry` returns at most 1024 recent events from the current
process, with algorithm, payment ID, planner duration, requested amounts, path
count, planned fees and errors. The persistent JSONL history is in
`.ldk/routing-events.jsonl`. `payment_succeeded` includes the actual LDK paid
fee when available. Events may be replayed by LDK after a crash; consumers
should account for replay when building training data. These endpoints use
the existing API authentication and read-only permissions.

Run the infrastructure-independent routing tests with:

```sh
cargo test --locked --no-default-features --features electrum,block-sync routing::tests
```

Tests cover explicit selection, policy restart/immutability, RGB and BTC splits,
rounding, shared capacities, fee/CLTV/HTLC limits, residual flow cancellation,
Pareto weights, bounded search, telemetry, tampered-route rejection, and an
exhaustive reference for min-cost flow on 64 small linear networks.
The existing regtest suite explicitly selects `ldk_dijkstra` to keep its
baseline regression coverage. Live regtest tests require the usual services;
their default initializer resets the regtest environment.
