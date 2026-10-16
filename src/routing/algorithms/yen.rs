//! Yen's loopless K-shortest paths, with bounded uniform-cost spur searches.
use super::{graph::Graph, EdgePath, SearchBudget};
use lightning::routing::router::RouteParameters;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
};

fn cost(g: &Graph, path: &[usize], amount: u64) -> u128 {
    path.iter().map(|i| g.edges[*i].cost(amount)).sum()
}
fn cltv(g: &Graph, path: &[usize]) -> u64 {
    path.iter().map(|i| g.edges[*i].cltv as u64).sum()
}

#[allow(clippy::too_many_arguments)]
fn shortest(
    g: &Graph,
    params: &RouteParameters,
    source: usize,
    banned_nodes: &HashSet<usize>,
    banned_edges: &HashSet<usize>,
    max_hops: usize,
    max_cltv: u64,
    budget: &mut SearchBudget,
) -> Result<Option<EdgePath>, &'static str> {
    let mut queue = BinaryHeap::new();
    queue.push(Reverse((0u128, Vec::<usize>::new(), source, 0u64)));
    while let Some(Reverse((score, path, node, delta))) = queue.pop() {
        budget.step()?;
        if node == g.target {
            return Ok(Some(path));
        }
        if path.len() >= max_hops {
            continue;
        }
        for edge_id in &g.outgoing[node] {
            budget.step()?;
            let edge = &g.edges[*edge_id];
            if banned_edges.contains(edge_id)
                || banned_nodes.contains(&edge.target)
                || edge.target == source
                || path.iter().any(|i| g.edges[*i].target == edge.target)
                || delta + edge.cltv as u64 > max_cltv
                || edge.capacity_msat == 0
                || (params.rgb_payment.is_some() && edge.capacity_rgb == 0)
            {
                continue;
            }
            let mut next = path.clone();
            next.push(*edge_id);
            queue.push(Reverse((
                score + edge.cost(params.final_value_msat),
                next,
                edge.target,
                delta + edge.cltv as u64,
            )));
        }
    }
    Ok(None)
}

pub(crate) fn paths(
    g: &Graph,
    params: &RouteParameters,
    k: usize,
    budget: &mut SearchBudget,
) -> Result<Vec<EdgePath>, &'static str> {
    let cltv_limit = params
        .payment_params
        .max_total_cltv_expiry_delta
        .checked_sub(g.final_cltv)
        .ok_or("Final CLTV exceeds limit")? as u64;
    let Some(first) = shortest(
        g,
        params,
        g.source,
        &HashSet::new(),
        &HashSet::new(),
        params.payment_params.max_path_length as usize,
        cltv_limit,
        budget,
    )?
    else {
        return Err("Yen found no path");
    };
    let mut selected = vec![first];
    let mut pending = BinaryHeap::<Reverse<(u128, EdgePath)>>::new();
    let mut known = HashSet::new();
    known.insert(selected[0].clone());
    while selected.len() < k {
        let previous = selected.last().unwrap().clone();
        for spur_index in 0..previous.len() {
            budget.step()?;
            let root = &previous[..spur_index];
            let spur = if spur_index == 0 {
                g.source
            } else {
                g.edges[previous[spur_index - 1]].target
            };
            let banned_nodes = root.iter().map(|i| g.edges[*i].source).collect();
            let banned_edges = selected
                .iter()
                .filter(|p| p.len() > spur_index && p[..spur_index] == *root)
                .map(|p| p[spur_index])
                .collect();
            let root_cltv = cltv(g, root);
            if root_cltv > cltv_limit {
                continue;
            }
            if let Some(tail) = shortest(
                g,
                params,
                spur,
                &banned_nodes,
                &banned_edges,
                params.payment_params.max_path_length as usize - root.len(),
                cltv_limit - root_cltv,
                budget,
            )? {
                let mut candidate = root.to_vec();
                candidate.extend(tail);
                if known.insert(candidate.clone()) {
                    pending.push(Reverse((
                        cost(g, &candidate, params.final_value_msat),
                        candidate,
                    )));
                }
            }
        }
        if let Some(Reverse((_, next))) = pending.pop() {
            selected.push(next);
        } else {
            break;
        }
    }
    Ok(selected)
}
