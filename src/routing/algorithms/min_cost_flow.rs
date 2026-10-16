//! Integral successive-shortest-augmenting-path min-cost flow with reverse residual arcs.
//! The linear cost amortizes base fees at the requested amount. BTC/RGB are coupled
//! proportionally, with a conservative BTC fee reserve; final HTLCs are validated exactly.
use super::{graph::Graph, units, EdgePath, SearchBudget};
use crate::routing::policy::RoutingPolicy;
use lightning::routing::router::RouteParameters;
use std::collections::VecDeque;

struct Arc {
    source: usize,
    target: usize,
    capacity: u64,
    cost: i128,
}

pub(crate) fn solve(
    g: &Graph,
    params: &RouteParameters,
    policy: &RoutingPolicy,
    budget: &mut SearchBudget,
) -> Result<Vec<(EdgePath, u64)>, &'static str> {
    let requested = units(params);
    if requested == 0 || params.final_value_msat == 0 {
        return Err("Cannot route a zero-value flow");
    }
    // At most this much BTC can be consumed by fees on a shared edge across all parts.
    // Zero-fee graphs require no reserve, including when no explicit fee cap was supplied.
    let max_edge_fee = g
        .edges
        .iter()
        .map(|e| e.fee(params.final_value_msat))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let reserve = params
        .max_total_routing_fee_msat
        .unwrap_or(params.final_value_msat / 100 + 50_000)
        .min(
            max_edge_fee
                .saturating_mul(params.payment_params.max_path_length as u64)
                .saturating_mul(params.payment_params.max_path_count as u64),
        );
    let mut arcs = vec![];
    let mut adjacency = vec![vec![]; g.nodes.len()];
    let mut initial = vec![];
    for edge in &g.edges {
        budget.step()?;
        let btc = edge
            .capacity_msat
            .min(edge.htlc_max_msat)
            .saturating_sub(if edge.target == g.target { 0 } else { reserve });
        let cap = (btc as u128 * requested as u128 / params.final_value_msat as u128)
            .min(requested as u128) as u64;
        let cap = if params.rgb_payment.is_some() {
            cap.min(edge.capacity_rgb)
        } else {
            cap
        };
        let cap =
            if (!g.supports_mpp || params.payment_params.max_path_count == 1) && cap < requested {
                0
            } else {
                cap
            };
        let cost =
            i128::try_from(edge.cost(params.final_value_msat)).map_err(|_| "Flow cost overflow")?;
        let index = arcs.len();
        arcs.push(Arc {
            source: edge.source,
            target: edge.target,
            capacity: cap,
            cost,
        });
        arcs.push(Arc {
            source: edge.target,
            target: edge.source,
            capacity: 0,
            cost: -cost,
        });
        adjacency[edge.source].push(index);
        adjacency[edge.target].push(index + 1);
        initial.push(cap);
    }
    let mut sent = 0;
    let mut augmentations = 0;
    while sent < requested {
        budget.step()?;
        if augmentations >= policy.max_candidates {
            return Err("Min-cost flow augmentation limit reached");
        }
        let mut distance = vec![i128::MAX; g.nodes.len()];
        let mut previous = vec![None; g.nodes.len()];
        let mut queued = vec![false; g.nodes.len()];
        let mut queue = VecDeque::from([g.source]);
        distance[g.source] = 0;
        queued[g.source] = true;
        while let Some(node) = queue.pop_front() {
            queued[node] = false;
            for index in &adjacency[node] {
                budget.step()?;
                let arc = &arcs[*index];
                if arc.capacity == 0 {
                    continue;
                }
                let d = distance[node]
                    .checked_add(arc.cost)
                    .ok_or("Flow distance overflow")?;
                if d < distance[arc.target] {
                    distance[arc.target] = d;
                    previous[arc.target] = Some(*index);
                    if !queued[arc.target] {
                        queue.push_back(arc.target);
                        queued[arc.target] = true;
                    }
                }
            }
        }
        if previous[g.target].is_none() {
            return Err("Min-cost flow has insufficient residual capacity");
        }
        let mut amount = requested - sent;
        let mut node = g.target;
        let mut chain = vec![];
        while node != g.source {
            budget.step()?;
            let index = previous[node].ok_or("Invalid residual predecessor")?;
            if chain.len() >= g.nodes.len() {
                return Err("Residual predecessor cycle");
            }
            amount = amount.min(arcs[index].capacity);
            chain.push(index);
            node = arcs[index].source;
        }
        for index in chain {
            arcs[index].capacity -= amount;
            arcs[index ^ 1].capacity += amount;
        }
        sent += amount;
        augmentations += 1;
    }
    let mut flow: Vec<u64> = initial
        .iter()
        .enumerate()
        .map(|(i, cap)| cap - arcs[2 * i].capacity)
        .collect();
    let mut result = vec![];
    let mut extracted = 0;
    while extracted < requested {
        budget.step()?;
        let path = extract(g, &flow, budget)?.ok_or("Cannot decompose min-cost flow")?;
        let value = path
            .iter()
            .map(|i| flow[*i])
            .min()
            .ok_or("Empty flow path")?
            .min(requested - extracted);
        for i in &path {
            flow[*i] -= value;
        }
        result.push((path, value));
        extracted += value;
        if result.len() > params.payment_params.max_path_count as usize {
            return Err("Min-cost flow requires too many HTLC paths");
        }
    }
    Ok(result)
}

fn extract(
    g: &Graph,
    flow: &[u64],
    budget: &mut SearchBudget,
) -> Result<Option<EdgePath>, &'static str> {
    let mut stack = vec![(g.source, 0usize)];
    let mut path = vec![];
    let mut visited = vec![false; g.nodes.len()];
    visited[g.source] = true;
    while let Some((node, next)) = stack.last_mut() {
        budget.step()?;
        if *node == g.target {
            return Ok(Some(path));
        }
        if *next == g.outgoing[*node].len() {
            visited[*node] = false;
            stack.pop();
            path.pop();
            continue;
        }
        let edge_id = g.outgoing[*node][*next];
        *next += 1;
        let target = g.edges[edge_id].target;
        if flow[edge_id] == 0 || visited[target] {
            continue;
        }
        visited[target] = true;
        path.push(edge_id);
        stack.push((target, 0));
    }
    Ok(None)
}
