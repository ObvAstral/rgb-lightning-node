//! Bounded planners over a common BTC/RGB snapshot.
pub(crate) mod graph;
mod min_cost_flow;
mod multi_objective;
mod yen;

use super::policy::{Algorithm, RoutingPolicy};
use graph::Graph;
use lightning::routing::router::{Path, Route, RouteHop, RouteParameters};
use std::time::{Duration, Instant};

pub(crate) type EdgePath = Vec<usize>;
pub(crate) struct SearchBudget {
    deadline: Instant,
    remaining: usize,
}
impl SearchBudget {
    pub fn new(policy: &RoutingPolicy) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(policy.timeout_ms),
            remaining: policy.max_expansions,
        }
    }
    pub fn step(&mut self) -> Result<(), &'static str> {
        if self.remaining == 0 || Instant::now() >= self.deadline {
            return Err("Routing search budget exhausted");
        }
        self.remaining -= 1;
        Ok(())
    }
}

pub(crate) fn plan(
    graph: &Graph,
    params: &RouteParameters,
    policy: &RoutingPolicy,
    budget: &mut SearchBudget,
) -> Result<Route, &'static str> {
    match policy.algorithm {
        Algorithm::Yen => {
            let candidates = yen::paths(graph, params, policy.max_candidates, budget)?;
            allocate(graph, params, &candidates, budget)
        }
        Algorithm::MultiObjective => {
            let candidates = multi_objective::paths(graph, params, policy, budget)?;
            allocate(graph, params, &candidates, budget)
        }
        Algorithm::MinCostFlow => {
            let flows = min_cost_flow::solve(graph, params, policy, budget)?;
            materialize(graph, params, &flows, budget)
        }
        Algorithm::LdkDijkstra => Err("LDK planner must be called through its adapter"),
    }
}

/// BTC final values and RGB quantities use a proportional allocation; fees are added separately.
/// This is an explicit approximation, not an exact two-commodity fixed-charge flow optimizer.
pub(crate) fn units(params: &RouteParameters) -> u64 {
    params
        .rgb_payment
        .map_or(params.final_value_msat, |(_, r)| r)
}
fn part_msat(remaining_msat: u64, amount: u64, remaining_units: u64) -> Result<u64, &'static str> {
    if remaining_units == 0 || amount == 0 || amount > remaining_units {
        return Err("Invalid flow allocation");
    }
    u64::try_from((remaining_msat as u128 * amount as u128).div_ceil(remaining_units as u128))
        .map_err(|_| "Flow amount overflow")
}

pub(crate) fn amounts(
    graph: &Graph,
    path: &[usize],
    final_msat: u64,
) -> Result<Vec<u64>, &'static str> {
    if path.is_empty() || final_msat == 0 {
        return Err("Empty or zero-value path");
    }
    let mut values = vec![final_msat; path.len()];
    for i in (0..path.len().saturating_sub(1)).rev() {
        values[i] = values[i + 1]
            .checked_add(graph.edges[path[i + 1]].fee(values[i + 1])?)
            .ok_or("Path amount overflow")?;
    }
    Ok(values)
}

fn fits_max(
    graph: &Graph,
    path: &[usize],
    msat: u64,
    rgb: u64,
    used_msat: &[u64],
    used_rgb: &[u64],
) -> bool {
    let Ok(values) = amounts(graph, path, msat) else {
        return false;
    };
    path.iter().zip(values).all(|(i, value)| {
        let e = &graph.edges[*i];
        value <= e.htlc_max_msat
            && value <= e.capacity_msat.saturating_sub(used_msat[*i])
            && rgb <= e.capacity_rgb.saturating_sub(used_rgb[*i])
    })
}

fn path_valid(graph: &Graph, path: &[usize], params: &RouteParameters) -> bool {
    if path.is_empty() || path.len() > params.payment_params.max_path_length as usize {
        return false;
    }
    let mut current = graph.source;
    let mut seen = vec![current];
    let mut cltv = graph.final_cltv as u64;
    for i in path {
        let e = &graph.edges[*i];
        if e.source != current || seen.contains(&e.target) {
            return false;
        }
        current = e.target;
        seen.push(current);
        cltv += e.cltv as u64;
    }
    current == graph.target && cltv <= params.payment_params.max_total_cltv_expiry_delta as u64
}

pub(crate) fn allocate(
    graph: &Graph,
    params: &RouteParameters,
    candidates: &[EdgePath],
    budget: &mut SearchBudget,
) -> Result<Route, &'static str> {
    let mut remaining_units = units(params);
    let mut remaining_msat = params.final_value_msat;
    let mut used_msat = vec![0u64; graph.edges.len()];
    let mut used_rgb = vec![0u64; graph.edges.len()];
    let mut allocations = vec![];
    let max_paths = if graph.supports_mpp {
        params.payment_params.max_path_count as usize
    } else {
        1
    };
    for path in candidates {
        budget.step()?;
        if remaining_units == 0 {
            break;
        }
        if allocations.len() == max_paths {
            break;
        }
        if !path_valid(graph, path, params) {
            continue;
        }
        let mut low = 0;
        let mut high = remaining_units;
        while low < high {
            budget.step()?;
            let mid = low + (high - low).div_ceil(2);
            let msat = part_msat(remaining_msat, mid, remaining_units)?;
            if fits_max(
                graph,
                path,
                msat,
                if params.rgb_payment.is_some() { mid } else { 0 },
                &used_msat,
                &used_rgb,
            ) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        if low == 0 {
            continue;
        }
        // Without MPP support only a complete path is permissible.
        if allocations.len() + 1 == max_paths && low != remaining_units {
            continue;
        }
        let msat = part_msat(remaining_msat, low, remaining_units)?;
        let values = amounts(graph, path, msat)?;
        if path
            .iter()
            .zip(&values)
            .any(|(i, value)| *value < graph.edges[*i].htlc_min_msat)
        {
            continue;
        }
        for (i, value) in path.iter().zip(values) {
            used_msat[*i] += value;
            if params.rgb_payment.is_some() {
                used_rgb[*i] += low;
            }
        }
        allocations.push((path.clone(), low));
        remaining_units -= low;
        remaining_msat -= msat;
    }
    if remaining_units != 0 || remaining_msat != 0 {
        return Err(
            "Selected planner could not allocate the complete BTC/RGB payment within its limits",
        );
    }
    materialize(graph, params, &allocations, budget)
}

pub(crate) fn materialize(
    graph: &Graph,
    params: &RouteParameters,
    flows: &[(EdgePath, u64)],
    budget: &mut SearchBudget,
) -> Result<Route, &'static str> {
    let mut remaining_units = units(params);
    let mut remaining_msat = params.final_value_msat;
    let mut paths = vec![];
    for (path, flow) in flows {
        budget.step()?;
        if !path_valid(graph, path, params) {
            return Err("Invalid flow decomposition");
        }
        let value = part_msat(remaining_msat, *flow, remaining_units)?;
        let values = amounts(graph, path, value)?;
        let mut hops = vec![];
        for (pos, edge_idx) in path.iter().enumerate() {
            let edge = &graph.edges[*edge_idx];
            hops.push(RouteHop {
                pubkey: graph.nodes[edge.target],
                node_features: graph.features[edge.target].clone(),
                short_channel_id: edge.scid,
                channel_features: edge.features.clone(),
                fee_msat: if pos + 1 == path.len() {
                    value
                } else {
                    values[pos] - values[pos + 1]
                },
                cltv_expiry_delta: path
                    .get(pos + 1)
                    .map_or(graph.final_cltv, |i| graph.edges[*i].cltv),
                maybe_announced_channel: edge.announced,
                payment_amount: value,
                rgb_payment: params.rgb_payment.map(|(c, _)| (c, *flow)),
            });
        }
        paths.push(Path {
            hops,
            blinded_tail: None,
        });
        remaining_units -= flow;
        remaining_msat -= value;
    }
    if remaining_units != 0 || remaining_msat != 0 {
        return Err("Incomplete flow decomposition");
    }
    Ok(Route {
        paths,
        route_params: Some(params.clone()),
    })
}
