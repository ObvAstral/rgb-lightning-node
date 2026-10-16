//! Bounded multi-label search with Pareto dominance on fee, LDK penalty, CLTV and hops.
use super::{graph::Graph, EdgePath, SearchBudget};
use crate::routing::policy::{ObjectiveWeights, RoutingPolicy};
use lightning::routing::router::RouteParameters;
use std::{cmp::Reverse, collections::BinaryHeap};

#[derive(Clone)]
struct Label {
    node: usize,
    path: EdgePath,
    objective: [u128; 4],
}
fn dominates(a: &[u128; 4], b: &[u128; 4]) -> bool {
    a.iter().zip(b).all(|(a, b)| a <= b) && a.iter().zip(b).any(|(a, b)| a < b)
}
fn dominates_prefix(g: &Graph, a: &Label, b: &Label) -> bool {
    dominates(&a.objective, &b.objective)
        && a.path.iter().all(|i| {
            b.path
                .iter()
                .any(|j| g.edges[*i].target == g.edges[*j].target)
        })
}
fn weighted(v: &[u128; 4], w: &ObjectiveWeights) -> u128 {
    v[0] * w.fee as u128
        + v[1] * w.reliability as u128
        + v[2] * w.cltv as u128
        + v[3] * w.hops as u128
}

pub(crate) fn paths(
    g: &Graph,
    params: &RouteParameters,
    policy: &RoutingPolicy,
    budget: &mut SearchBudget,
) -> Result<Vec<EdgePath>, &'static str> {
    let weights = policy
        .weights
        .as_ref()
        .ok_or("Multi-objective weights missing")?;
    let mut labels = vec![Label {
        node: g.source,
        path: vec![],
        objective: [0; 4],
    }];
    let mut at_node = vec![Vec::<usize>::new(); g.nodes.len()];
    at_node[g.source].push(0);
    let mut queue = BinaryHeap::new();
    queue.push(Reverse((0u128, 0usize)));
    let mut completed = vec![];
    while let Some(Reverse((_, id))) = queue.pop() {
        budget.step()?;
        let current = labels[id].clone();
        if current.node == g.target {
            completed.push(id);
            if completed.len() == policy.max_candidates {
                break;
            }
            continue;
        }
        if !at_node[current.node].contains(&id)
            || current.path.len() >= params.payment_params.max_path_length as usize
        {
            continue;
        }
        for edge_id in &g.outgoing[current.node] {
            budget.step()?;
            let edge = &g.edges[*edge_id];
            if edge.target == g.source
                || current
                    .path
                    .iter()
                    .any(|i| g.edges[*i].target == edge.target)
                || (params.rgb_payment.is_some() && edge.capacity_rgb == 0)
            {
                continue;
            }
            let objective = [
                current.objective[0] + edge.fee(params.final_value_msat)? as u128,
                current.objective[1] + edge.penalty as u128,
                current.objective[2] + edge.cltv as u128,
                current.objective[3] + 1,
            ];
            if objective[2] + g.final_cltv as u128
                > params.payment_params.max_total_cltv_expiry_delta as u128
            {
                continue;
            }
            let mut path = current.path.clone();
            path.push(*edge_id);
            let next = Label {
                node: edge.target,
                path,
                objective,
            };
            // A dominating prefix must allow the same loopless continuations.
            // Completed alternatives remain available for MPP allocation.
            if edge.target != g.target
                && at_node[edge.target]
                    .iter()
                    .any(|i| dominates_prefix(g, &labels[*i], &next))
            {
                continue;
            }
            let id = labels.len();
            labels.push(next);
            if edge.target != g.target {
                at_node[edge.target].retain(|i| !dominates_prefix(g, &labels[id], &labels[*i]));
            }
            if at_node[edge.target].len() >= policy.max_candidates {
                let worst = *at_node[edge.target]
                    .iter()
                    .max_by_key(|i| weighted(&labels[**i].objective, weights))
                    .unwrap();
                if weighted(&labels[worst].objective, weights) <= weighted(&objective, weights) {
                    continue;
                }
                at_node[edge.target].retain(|i| *i != worst);
            }
            at_node[edge.target].push(id);
            queue.push(Reverse((weighted(&objective, weights), id)));
        }
    }
    if completed.is_empty() {
        return Err("Multi-objective planner found no path");
    }
    // Prefer the Pareto front, using the explicitly supplied weights within each rank.
    completed.sort_by_key(|id| {
        let obj = &labels[*id].objective;
        let rank = completed_rank(&labels, obj, &at_node[g.target]);
        (rank, weighted(obj, weights), labels[*id].path.clone())
    });
    Ok(completed
        .into_iter()
        .map(|id| labels[id].path.clone())
        .collect())
}
fn completed_rank(labels: &[Label], objective: &[u128; 4], goals: &[usize]) -> usize {
    goals
        .iter()
        .filter(|i| dominates(&labels[**i].objective, objective))
        .count()
}
