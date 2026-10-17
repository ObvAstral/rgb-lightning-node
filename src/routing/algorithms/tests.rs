//! Independent finite oracles and boundary cases. These tests need no regtest services.
use super::{graph::Graph, min_cost_flow, multi_objective, yen, EdgePath, SearchBudget};
use crate::routing::{
    policy::ObjectiveWeights,
    tests::{diamond, edge, key, params, policy, run},
};
use lightning::routing::router::{Route, RouteParameters};
use std::{collections::HashSet, time::Instant};

const PLANNERS: [&str; 3] = ["yen", "min_cost_flow", "multi_objective"];

// Deliberately enumerate simple paths by DFS rather than reuse any planner's search,
// cost, dominance, allocation, or validation helpers.
fn enumerate_paths(g: &Graph, params: &RouteParameters) -> Vec<EdgePath> {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        g: &Graph,
        params: &RouteParameters,
        node: usize,
        seen: &mut Vec<usize>,
        path: &mut EdgePath,
        cltv: u64,
        out: &mut Vec<EdgePath>,
    ) {
        if cltv > params.payment_params.max_total_cltv_expiry_delta as u64 {
            return;
        }
        if node == g.target {
            out.push(path.clone());
            return;
        }
        if path.len() == params.payment_params.max_path_length as usize {
            return;
        }
        for i in &g.outgoing[node] {
            let e = &g.edges[*i];
            if seen.contains(&e.target)
                || e.capacity_msat == 0
                || (params.rgb_payment.is_some() && e.capacity_rgb == 0)
            {
                continue;
            }
            seen.push(e.target);
            path.push(*i);
            visit(g, params, e.target, seen, path, cltv + e.cltv as u64, out);
            path.pop();
            seen.pop();
        }
    }
    let mut out = vec![];
    visit(
        g,
        params,
        g.source,
        &mut vec![g.source],
        &mut vec![],
        g.final_cltv as u64,
        &mut out,
    );
    out
}

fn objectives(g: &Graph, path: &[usize], amount: u64) -> [u128; 4] {
    let mut result = [0, 0, 0, path.len() as u128];
    for i in path {
        let e = &g.edges[*i];
        result[0] += e.fees.base_msat as u128
            + amount as u128 * e.fees.proportional_millionths as u128 / 1_000_000;
        result[1] += e.penalty as u128;
        result[2] += e.cltv as u128;
    }
    result
}

fn subgraph(template: &Graph, mask: usize) -> Graph {
    let mut g = template.clone();
    g.edges.clear();
    for outgoing in &mut g.outgoing {
        outgoing.clear();
    }
    for (i, e) in template.edges.iter().enumerate() {
        if mask & (1 << i) != 0 {
            g.add(e.clone());
        }
    }
    g
}

fn assert_route_totals(g: &Graph, route: &Route, amount: u64, rgb: Option<u64>) {
    let mut btc_used = vec![0u128; g.edges.len()];
    let mut rgb_used = vec![0u128; g.edges.len()];
    let mut total_btc = 0u128;
    let mut total_rgb = 0u128;
    for path in &route.paths {
        let part = path.hops.last().unwrap();
        total_btc += part.fee_msat as u128;
        total_rgb += part.rgb_payment.map_or(0, |(_, n)| n as u128);
        let mut seen = HashSet::from([g.nodes[g.source]]);
        let mut incoming = 0u128;
        for (pos, hop) in path.hops.iter().enumerate().rev() {
            assert!(seen.insert(hop.pubkey), "route contains a cycle");
            let source = if pos == 0 {
                &g.nodes[g.source]
            } else {
                &path.hops[pos - 1].pubkey
            };
            let i = g.edge(source, &hop.pubkey, hop.short_channel_id).unwrap();
            incoming += hop.fee_msat as u128;
            btc_used[i] += incoming;
            rgb_used[i] += part.rgb_payment.map_or(0, |(_, n)| n as u128);
            assert!(incoming >= g.edges[i].htlc_min_msat as u128);
            assert!(incoming <= g.edges[i].htlc_max_msat as u128);
            assert!(btc_used[i] <= g.edges[i].capacity_msat as u128);
            assert!(rgb_used[i] <= g.edges[i].capacity_rgb as u128);
        }
        assert_eq!(part.pubkey, g.nodes[g.target]);
    }
    assert_eq!(total_btc, amount as u128);
    assert_eq!(total_rgb, rgb.unwrap_or(0) as u128);
}

#[test]
fn yen_matches_all_simple_paths_on_every_four_node_directed_graph() {
    let mut template = Graph::new(key(1), key(4), 18, true);
    for from in 1..=4 {
        for to in 1..=4 {
            if from == to {
                continue;
            }
            let i = template.edges.len();
            edge(
                &mut template,
                from,
                to,
                100 + i as u64,
                100_000,
                if i % 4 == 0 { 0 } else { 100 },
                if from == 1 { 0 } else { i as u32 % 3 },
                i as u64 % 5,
            );
            template.edges[i].fees.proportional_millionths = (i as u32 % 3) * 1000;
        }
    }
    let routing = policy("yen");
    // All 2^12 directed topologies, including cycles, disconnected components and
    // equal-cost paths. Vary hop/CLTV limits and BTC versus RGB eligibility.
    for mask in 0..1 << template.edges.len() {
        let g = subgraph(&template, mask);
        for rgb in [None, Some(10)] {
            let mut p = params(1001, rgb);
            p.payment_params.max_path_length = 1 + (mask % 3) as u8;
            p.payment_params.max_total_cltv_expiry_delta = 18 + 10 * ((mask / 3) % 3) as u32;
            let mut expected = enumerate_paths(&g, &p);
            expected.sort_by_key(|path| {
                let v = objectives(&g, path, p.final_value_msat);
                (v[0] + v[1], path.clone())
            });
            for k in [1, 2, 8] {
                let result = yen::paths(&g, &p, k, &mut SearchBudget::new(&routing));
                if expected.is_empty() {
                    assert!(result.is_err(), "mask={mask:#x}, rgb={rgb:?}, k={k}");
                } else {
                    assert_eq!(
                        result.unwrap_or_else(|e| panic!("mask={mask:#x}: {e}")),
                        expected[..expected.len().min(k)],
                        "mask={mask:#x}, rgb={rgb:?}, k={k}"
                    );
                }
            }
        }
    }
}

#[test]
fn yen_keeps_parallel_channels_distinct_and_excludes_zero_cost_cycles() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    for (from, to, scid) in [
        (1, 2, 12),
        (1, 2, 112),
        (2, 4, 24),
        (2, 4, 124),
        (2, 3, 23),
        (3, 2, 32),
        (3, 4, 34),
        (2, 1, 21),
        (1, 4, 14),
    ] {
        edge(&mut g, from, to, scid, 1000, 100, 0, 0);
    }
    let p = params(100, Some(10));
    let mut expected = enumerate_paths(&g, &p);
    expected.sort();
    let result = yen::paths(&g, &p, 256, &mut SearchBudget::new(&policy("yen"))).unwrap();
    assert_eq!(result, expected);
    assert_eq!(result.len(), 7);
}

#[test]
fn multi_objective_selects_a_global_pareto_optimum_on_small_graphs() {
    let mut template = Graph::new(key(1), key(4), 18, true);
    for (i, (from, to)) in [(1, 2), (1, 3), (2, 3), (3, 2), (2, 4), (3, 4), (1, 4)]
        .into_iter()
        .enumerate()
    {
        edge(
            &mut template,
            from,
            to,
            100 + i as u64,
            100_000,
            if i == 2 { 0 } else { 100 },
            if from == 1 { 0 } else { (i as u32 * 3) % 7 },
            (13 - i as u64) % 5,
        );
        template.edges[i].cltv = if from == 1 { 0 } else { 1 + i as u32 * 3 };
    }
    for mask in 0..1 << template.edges.len() {
        let g = subgraph(&template, mask);
        for rgb in [None, Some(10)] {
            let mut p = params(1000, rgb);
            p.payment_params.max_path_length = 1 + (mask % 3) as u8;
            p.payment_params.max_total_cltv_expiry_delta = 18 + (mask % 35) as u32;
            let expected = enumerate_paths(&g, &p);
            for weights in [
                [1, 0, 0, 0],
                [0, 1, 0, 0],
                [0, 0, 1, 0],
                [0, 0, 0, 1],
                [3, 7, 2, 5],
            ] {
                let mut routing = policy("multi_objective");
                routing.weights = Some(ObjectiveWeights {
                    fee: weights[0],
                    reliability: weights[1],
                    cltv: weights[2],
                    hops: weights[3],
                });
                let result =
                    multi_objective::paths(&g, &p, &routing, &mut SearchBudget::new(&routing));
                if expected.is_empty() {
                    assert!(
                        result.is_err(),
                        "mask={mask}, rgb={rgb:?}, weights={weights:?}"
                    );
                    continue;
                }
                let paths = result.unwrap_or_else(|e| panic!("mask={mask}: {e}"));
                assert_eq!(paths.iter().collect::<HashSet<_>>().len(), paths.len());
                assert!(paths.iter().all(|path| expected.contains(path)));
                let score = |v: [u128; 4]| -> u128 {
                    v.iter().zip(weights).map(|(v, w)| v * w as u128).sum()
                };
                let best = objectives(&g, &paths[0], p.final_value_msat);
                let expected_score = expected
                    .iter()
                    .map(|path| score(objectives(&g, path, p.final_value_msat)))
                    .min()
                    .unwrap();
                assert_eq!(
                    score(best),
                    expected_score,
                    "mask={mask}, weights={weights:?}"
                );
                for path in &expected {
                    let other = objectives(&g, path, p.final_value_msat);
                    assert!(
                        !other.iter().zip(best).all(|(a, b)| *a <= b) || other == best,
                        "selected a dominated path: mask={mask}, weights={weights:?}"
                    );
                }
            }
        }
    }
}

// Assign indivisible units to every combination of loopless paths, allowing the
// same path repeatedly. Sorting the path indices only eliminates permutations.
fn minimum_linear_cost(g: &Graph, candidates: &[EdgePath], amount: u64) -> Option<u128> {
    fn assign(
        g: &Graph,
        candidates: &[EdgePath],
        remaining: u64,
        first: usize,
        used: &mut [u64],
    ) -> Option<u128> {
        if remaining == 0 {
            return Some(0);
        }
        let mut best = None;
        for i in first..candidates.len() {
            let path = &candidates[i];
            if path.iter().any(|e| used[*e] == g.edges[*e].capacity_msat) {
                continue;
            }
            for e in path {
                used[*e] += 1;
            }
            if let Some(tail) = assign(g, candidates, remaining - 1, i, used) {
                let cost = tail
                    + path
                        .iter()
                        .map(|e| g.edges[*e].penalty as u128)
                        .sum::<u128>();
                best = Some(best.map_or(cost, |old: u128| old.min(cost)));
            }
            for e in path {
                used[*e] -= 1;
            }
        }
        best
    }
    assign(g, candidates, amount, 0, &mut vec![0; g.edges.len()])
}

#[test]
fn min_cost_flow_matches_every_small_integer_capacity_assignment() {
    let mut template = Graph::new(key(1), key(4), 18, true);
    for (i, (from, to)) in [(1, 2), (1, 3), (2, 3), (3, 2), (2, 4), (3, 4), (1, 4)]
        .into_iter()
        .enumerate()
    {
        edge(
            &mut template,
            from,
            to,
            100 + i as u64,
            2,
            0,
            0,
            (i as u64 * 3) % 7,
        );
    }
    let routing = policy("min_cost_flow");
    // Every capacity in {0,1,2} on seven directed edges, for demands 1, 2 and 3.
    for case in 0..3usize.pow(template.edges.len() as u32) {
        let mut g = subgraph(&template, 0);
        let mut digits = case;
        for e in &template.edges {
            let mut e = e.clone();
            e.capacity_msat = (digits % 3) as u64;
            e.htlc_max_msat = e.capacity_msat;
            digits /= 3;
            g.add(e);
        }
        for amount in 1..=3 {
            let p = params(amount, None);
            let candidates = enumerate_paths(&g, &p);
            let expected = minimum_linear_cost(&g, &candidates, amount);
            let result = run(&g, &p, &routing);
            match expected {
                None => assert!(result.is_err(), "case={case}, amount={amount}: infeasible"),
                Some(expected) => {
                    let route =
                        result.unwrap_or_else(|e| panic!("case={case}, amount={amount}: {e}"));
                    assert_route_totals(&g, &route, amount, None);
                    let mut actual = 0u128;
                    for path in &route.paths {
                        let mut source = g.nodes[g.source];
                        for hop in &path.hops {
                            let i = g.edge(&source, &hop.pubkey, hop.short_channel_id).unwrap();
                            actual += g.edges[i].penalty as u128 * path.final_value_msat() as u128;
                            source = hop.pubkey;
                        }
                    }
                    assert_eq!(actual, expected, "case={case}, amount={amount}");
                }
            }
        }
    }
}

#[test]
fn planners_reject_empty_disconnected_and_reverse_only_graphs() {
    let empty = Graph::new(key(1), key(4), 18, true);
    let mut disconnected = empty.clone();
    edge(&mut disconnected, 1, 2, 12, 1000, 100, 0, 0);
    edge(&mut disconnected, 2, 1, 21, 1000, 100, 0, 0);
    edge(&mut disconnected, 3, 4, 34, 1000, 100, 0, 0);
    let mut reverse = empty.clone();
    edge(&mut reverse, 4, 1, 41, 1000, 100, 0, 0);
    for algorithm in PLANNERS {
        for g in [&empty, &disconnected, &reverse] {
            for rgb in [None, Some(10)] {
                assert!(
                    run(g, &params(100, rgb), &policy(algorithm)).is_err(),
                    "{algorithm}"
                );
            }
        }
    }
}

#[test]
fn planners_reject_zero_btc_and_zero_rgb_amounts() {
    for algorithm in PLANNERS {
        for (btc, rgb) in [(0, None), (0, Some(1)), (100, Some(0))] {
            assert!(
                run(&diamond(), &params(btc, rgb), &policy(algorithm)).is_err(),
                "{algorithm}: {btc}, {rgb:?}"
            );
        }
    }
}

#[test]
fn planners_accept_exact_capacity_and_reject_one_unit_more() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 4, 14, 100, 10, 0, 0);
    for algorithm in PLANNERS {
        for rgb in [None, Some(10)] {
            let route = run(&g, &params(100, rgb), &policy(algorithm)).unwrap();
            assert_route_totals(&g, &route, 100, rgb);
            assert!(
                run(&g, &params(101, rgb), &policy(algorithm)).is_err(),
                "{algorithm}"
            );
        }
        assert!(
            run(&g, &params(100, Some(11)), &policy(algorithm)).is_err(),
            "{algorithm}"
        );
    }
}

#[test]
fn planners_enforce_both_htlc_boundaries_inclusively() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 4, 14, 1000, 100, 0, 0);
    g.edges[0].htlc_min_msat = 50;
    g.edges[0].htlc_max_msat = 100;
    for algorithm in PLANNERS {
        for (amount, valid) in [(49, false), (50, true), (100, true), (101, false)] {
            for rgb in [None, Some(10)] {
                let result = run(&g, &params(amount, rgb), &policy(algorithm));
                assert_eq!(
                    result.is_ok(),
                    valid,
                    "{algorithm}: {amount}, {rgb:?}, {result:?}"
                );
            }
        }
    }
}

#[test]
fn planners_enforce_hop_and_cltv_boundaries_inclusively() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 100_000, 100, 0, 0);
    edge(&mut g, 2, 3, 23, 100_000, 100, 0, 0);
    edge(&mut g, 3, 4, 34, 100_000, 100, 0, 0);
    for algorithm in PLANNERS {
        for (hops, cltv, valid) in [
            (3, 38, true),
            (2, 38, false),
            (3, 37, false),
            (3, 17, false),
        ] {
            let mut p = params(1000, Some(10));
            p.payment_params.max_path_length = hops;
            p.payment_params.max_total_cltv_expiry_delta = cltv;
            let result = run(&g, &p, &policy(algorithm));
            assert_eq!(
                result.is_ok(),
                valid,
                "{algorithm}: hops={hops}, cltv={cltv}, {result:?}"
            );
        }
    }
}

#[test]
fn planners_compute_compounded_proportional_fees_with_integer_rounding() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 1_000_000, 100, 0, 0);
    edge(&mut g, 2, 3, 23, 1_000_000, 100, 3, 0);
    edge(&mut g, 3, 4, 34, 1_000_000, 100, 7, 0);
    g.edges[1].fees.proportional_millionths = 25_000;
    g.edges[2].fees.proportional_millionths = 10_000;
    for algorithm in PLANNERS {
        let mut routing = policy(algorithm);
        routing.max_fee_msat = Some(45);
        let route = run(&g, &params(1001, Some(10)), &routing).unwrap();
        assert_route_totals(&g, &route, 1001, Some(10));
        assert_eq!(route.get_total_fees(), 45, "{algorithm}");
        assert_eq!(
            route.paths[0]
                .hops
                .iter()
                .map(|h| h.fee_msat)
                .collect::<Vec<_>>(),
            [28, 17, 1001]
        );
        routing.max_fee_msat = Some(44);
        assert!(
            run(&g, &params(1001, Some(10)), &routing).is_err(),
            "{algorithm}"
        );
    }
}

#[test]
fn planners_handle_u64_max_without_arithmetic_overflow() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 4, 14, u64::MAX, u64::MAX, 0, 0);
    for algorithm in PLANNERS {
        for (btc, rgb) in [
            (u64::MAX, None),
            (u64::MAX, Some(u64::MAX)),
            (1, Some(u64::MAX)),
            (u64::MAX, Some(1)),
        ] {
            let route = run(&g, &params(btc, rgb), &policy(algorithm))
                .unwrap_or_else(|e| panic!("{algorithm}, {btc}, {rgb:?}: {e}"));
            assert_route_totals(&g, &route, btc, rgb);
        }
    }
}

#[test]
fn planners_reject_forwarding_amount_and_fee_overflow_without_panicking() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, u64::MAX, 100, 0, 0);
    edge(&mut g, 2, 4, 24, u64::MAX, 100, 1, 0);
    for algorithm in PLANNERS {
        assert!(
            run(&g, &params(u64::MAX, None), &policy(algorithm)).is_err(),
            "{algorithm}: amount overflow"
        );
    }
    g.edges[1].fees.proportional_millionths = u32::MAX;
    for algorithm in PLANNERS {
        assert!(
            run(&g, &params(u64::MAX, None), &policy(algorithm)).is_err(),
            "{algorithm}: fee overflow"
        );
    }
}

#[test]
fn planners_do_not_reuse_shared_btc_capacity_across_parts() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 5, 15, 999, 100, 0, 0);
    edge(&mut g, 5, 2, 52, 600, 60, 0, 0);
    edge(&mut g, 2, 4, 24, 600, 60, 0, 0);
    edge(&mut g, 5, 3, 53, 400, 40, 0, 0);
    edge(&mut g, 3, 4, 34, 400, 40, 0, 0);
    for algorithm in PLANNERS {
        for rgb in [None, Some(100)] {
            assert!(
                run(&g, &params(1000, rgb), &policy(algorithm)).is_err(),
                "{algorithm}"
            );
        }
    }
}

#[test]
fn planners_skip_channels_without_rgb_but_allow_them_for_btc() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 4, 14, 1000, 0, 0, 0);
    for algorithm in PLANNERS {
        assert!(
            run(&g, &params(100, None), &policy(algorithm)).is_ok(),
            "{algorithm}"
        );
        assert!(
            run(&g, &params(100, Some(1)), &policy(algorithm)).is_err(),
            "{algorithm}"
        );
    }
}

#[test]
fn planners_preserve_capacity_from_dominated_parallel_prefixes_for_mpp() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 600, 60, 0, 0);
    edge(&mut g, 1, 2, 112, 400, 40, 0, 1);
    edge(&mut g, 2, 4, 24, 1000, 100, 0, 0);
    for algorithm in PLANNERS {
        for rgb in [None, Some(100)] {
            let route = run(&g, &params(1000, rgb), &policy(algorithm))
                .unwrap_or_else(|e| panic!("{algorithm}, {rgb:?}: {e}"));
            assert_eq!(route.paths.len(), 2, "{algorithm}");
            assert_route_totals(&g, &route, 1000, rgb);
            assert_eq!(
                route
                    .paths
                    .iter()
                    .map(|p| p.hops[0].short_channel_id)
                    .collect::<HashSet<_>>(),
                HashSet::from([12, 112])
            );
        }
    }
}

#[test]
fn planners_keep_a_sufficient_parallel_prefix_for_single_path_payments() {
    let mut g = Graph::new(key(1), key(4), 18, false);
    edge(&mut g, 1, 2, 12, 60, 6, 0, 0);
    edge(&mut g, 1, 2, 112, 1000, 100, 0, 1);
    edge(&mut g, 2, 4, 24, 1000, 100, 0, 0);
    for algorithm in PLANNERS {
        for rgb in [None, Some(10)] {
            let mut routing = policy(algorithm);
            routing.max_paths = Some(1);
            let route = run(&g, &params(100, rgb), &routing)
                .unwrap_or_else(|e| panic!("{algorithm}, {rgb:?}: {e}"));
            assert_eq!(route.paths.len(), 1, "{algorithm}");
            assert_eq!(route.paths[0].hops[0].short_channel_id, 112);
            assert_route_totals(&g, &route, 100, rgb);
        }
    }
}

#[test]
fn expired_deadlines_stop_every_planner_without_sleeping() {
    for algorithm in PLANNERS {
        let routing = policy(algorithm);
        let mut budget = SearchBudget::new(&routing);
        budget.deadline = Instant::now();
        assert_eq!(
            super::plan(&diamond(), &params(1000, Some(100)), &routing, &mut budget).unwrap_err(),
            "Routing search budget exhausted",
            "{algorithm}"
        );
    }
}

#[test]
fn min_cost_flow_obeys_the_augmentation_limit() {
    let mut routing = policy("min_cost_flow");
    routing.max_candidates = 1;
    assert_eq!(
        min_cost_flow::solve(
            &diamond(),
            &params(1000, Some(100)),
            &routing,
            &mut SearchBudget::new(&routing)
        )
        .unwrap_err(),
        "Min-cost flow augmentation limit reached"
    );
    routing.max_candidates = 2;
    assert!(run(&diamond(), &params(1000, Some(100)), &routing).is_ok());
}

#[test]
fn multi_objective_obeys_the_candidate_limit() {
    let mut routing = policy("multi_objective");
    routing.max_candidates = 1;
    assert!(run(&diamond(), &params(1000, Some(100)), &routing).is_err());
    routing.max_candidates = 2;
    assert!(run(&diamond(), &params(1000, Some(100)), &routing).is_ok());
}
