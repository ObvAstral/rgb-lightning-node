use super::{
    adaptive_router::AdaptiveRouter,
    algorithms::{
        self,
        graph::{Edge, Graph},
        SearchBudget,
    },
    policy::{Algorithm, PolicyStore, RoutingPolicy},
    telemetry::{RoutingEvent, Telemetry},
    validation,
};
use bitcoin::{
    secp256k1::{PublicKey, Secp256k1, SecretKey},
    Network,
};
use lightning::{
    ln::channelmanager::PaymentId,
    routing::{
        gossip::{NodeId, RoutingFees},
        router::{InFlightHtlcs, PaymentParameters, RouteParameters, Router},
    },
    sign::KeysManager,
    types::{
        features::{ChannelFeatures, NodeFeatures},
        payment::PaymentHash,
    },
    util::ser::{Readable, Writeable},
};
use lightning_persister::fs_store::FilesystemStore;
use rgb_lib::ContractId;
use std::sync::{Arc, RwLock};

fn key(i: u8) -> PublicKey {
    PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[i; 32]).unwrap())
}
fn policy(algorithm: &str) -> RoutingPolicy {
    let mut v =
        serde_json::json!({"algorithm":algorithm, "max_fee_msat":100000, "timeout_ms":10000});
    if algorithm == "multi_objective" {
        v["weights"] = serde_json::json!({"fee":1,"reliability":1,"cltv":1,"hops":1});
    }
    serde_json::from_value(v).unwrap()
}
fn params(amount: u64, rgb: Option<u64>) -> RouteParameters {
    RouteParameters::from_payment_params_and_value(
        PaymentParameters::for_keysend(key(4), 18, true),
        amount,
        rgb.map(|r| (ContractId::from([7; 32]), r)),
    )
}
fn edge(g: &mut Graph, from: u8, to: u8, scid: u64, btc: u64, rgb: u64, fee: u32, penalty: u64) {
    let source = g.node(key(from), NodeFeatures::empty());
    let target = g.node(key(to), NodeFeatures::empty());
    g.add(Edge {
        source,
        target,
        scid,
        capacity_msat: btc,
        htlc_max_msat: btc,
        htlc_min_msat: 0,
        capacity_rgb: rgb,
        fees: RoutingFees {
            base_msat: fee,
            proportional_millionths: 0,
        },
        cltv: if from == 1 { 0 } else { 10 },
        penalty,
        features: ChannelFeatures::empty(),
        announced: false,
    });
}
fn diamond() -> Graph {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 600, 60, 0, 0);
    edge(&mut g, 2, 4, 24, 600, 60, 0, 0);
    edge(&mut g, 1, 3, 13, 400, 40, 0, 0);
    edge(&mut g, 3, 4, 34, 400, 40, 0, 0);
    g
}
fn run(
    g: &Graph,
    p: &RouteParameters,
    policy: &RoutingPolicy,
) -> Result<lightning::routing::router::Route, &'static str> {
    let p = policy.constrain(p);
    let route = algorithms::plan(g, &p, policy, &mut SearchBudget::new(policy))?;
    validation::validate(&g.nodes[g.source], &p, policy, &route, Some(g))?;
    Ok(route)
}

#[test]
fn explicit_algorithm_is_required_at_every_api_entry_point() {
    assert!(serde_json::from_value::<RoutingPolicy>(serde_json::json!({})).is_err());
    assert!(
        serde_json::from_value::<RoutingPolicy>(serde_json::json!({"algorithm":null})).is_err()
    );
    assert!(
        serde_json::from_value::<RoutingPolicy>(serde_json::json!({"algorithm":"auto"})).is_err()
    );
    assert!(serde_json::from_value::<crate::routes::SendPaymentRequest>(
        serde_json::json!({"invoice":"invoice"})
    )
    .is_err());
    assert!(serde_json::from_value::<crate::routes::KeysendRequest>(
        serde_json::json!({"dest_pubkey":key(4).to_string(),"amt_msat":3000000})
    )
    .is_err());
    assert!(
        serde_json::from_value::<crate::routes::MakerExecuteRequest>(
            serde_json::json!({"swapstring":"x","payment_secret":"x","taker_pubkey":"x"})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<RoutingPolicy>(
        serde_json::json!({"algorithm":"yen","fallback":"ldk_dijkstra"})
    )
    .is_err());
}
#[test]
fn policy_rejects_invalid_bounds_and_weights() {
    let mut p = policy("multi_objective");
    assert!(p.validate().is_ok());
    p.weights = None;
    assert!(p.validate().is_err());
    let mut p = policy("yen");
    p.max_paths = Some(0);
    assert!(p.validate().is_err());
    p.max_paths = Some(2);
    p.timeout_ms = 0;
    assert!(p.validate().is_err());
    let mut p = policy("min_cost_flow");
    p.excluded_nodes.push("not a pubkey".into());
    assert!(p.validate().is_err());
}
#[test]
fn all_three_planners_split_btc_and_rgb_and_conserve_integer_totals() {
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        let route = run(&diamond(), &params(1000, Some(100)), &policy(a))
            .unwrap_or_else(|e| panic!("{a}: {e}"));
        assert_eq!(route.paths.len(), 2);
        assert_eq!(
            route
                .paths
                .iter()
                .map(|p| p.final_value_msat())
                .sum::<u64>(),
            1000
        );
        assert_eq!(
            route
                .paths
                .iter()
                .map(|p| p.hops.last().unwrap().rgb_payment.unwrap().1)
                .sum::<u64>(),
            100
        );
    }
}
#[test]
fn planners_handle_btc_only_and_rgb_rounding() {
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        assert_eq!(
            run(&diamond(), &params(1000, None), &policy(a))
                .unwrap()
                .paths
                .len(),
            2
        );
        let mut g = Graph::new(key(1), key(4), 18, true);
        edge(&mut g, 1, 4, 14, 7, 2, 0, 0);
        edge(&mut g, 1, 4, 15, 4, 2, 0, 0);
        let route = run(&g, &params(10, Some(3)), &policy(a)).unwrap();
        assert_eq!(route.paths.len(), 2);
    }
}
#[test]
fn yen_k_limit_is_effective_without_switching_to_another_planner() {
    let mut p = policy("yen");
    p.max_candidates = 1;
    assert!(run(&diamond(), &params(1000, Some(100)), &p).is_err());
    p.max_candidates = 2;
    assert!(run(&diamond(), &params(1000, Some(100)), &p).is_ok());
}
#[test]
fn min_cost_flow_uses_reverse_residual_arcs_to_undo_an_earlier_path() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 1, 0, 0, 0);
    edge(&mut g, 2, 3, 23, 1, 0, 0, 0);
    edge(&mut g, 3, 4, 34, 1, 0, 0, 0);
    edge(&mut g, 1, 3, 13, 1, 0, 0, 1);
    edge(&mut g, 2, 4, 24, 1, 0, 0, 2);
    let route = run(&g, &params(2, None), &policy("min_cost_flow")).unwrap();
    assert_eq!(route.paths.len(), 2);
    assert!(route
        .paths
        .iter()
        .all(|p| p.hops.iter().all(|h| h.short_channel_id != 23)));
    // Greedy path allocation cannot repair the first path on this topology.
    assert!(run(&g, &params(2, None), &policy("yen")).is_err());
}
#[test]
fn multi_objective_weights_change_the_selected_pareto_solution() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 100000, 0, 0, 0);
    edge(&mut g, 2, 4, 24, 100000, 0, 1, 100);
    edge(&mut g, 1, 3, 13, 100000, 0, 0, 0);
    edge(&mut g, 3, 4, 34, 100000, 0, 10, 0);
    let mut p = policy("multi_objective");
    p.weights.as_mut().unwrap().reliability = 0;
    assert_eq!(
        run(&g, &params(1000, None), &p).unwrap().paths[0].hops[0].pubkey,
        key(2)
    );
    p.weights.as_mut().unwrap().fee = 0;
    p.weights.as_mut().unwrap().reliability = 1;
    assert_eq!(
        run(&g, &params(1000, None), &p).unwrap().paths[0].hops[0].pubkey,
        key(3)
    );
}
#[test]
fn shared_capacity_is_not_counted_twice() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 5, 15, 1000, 90, 0, 0);
    edge(&mut g, 5, 2, 52, 600, 60, 0, 0);
    edge(&mut g, 2, 4, 24, 600, 60, 0, 0);
    edge(&mut g, 5, 3, 53, 400, 40, 0, 0);
    edge(&mut g, 3, 4, 34, 400, 40, 0, 0);
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        assert!(
            run(&g, &params(1000, Some(100)), &policy(a)).is_err(),
            "{a}"
        );
    }
}
#[test]
fn mpp_path_limits_and_positive_rgb_parts_are_enforced() {
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        let mut g = diamond();
        g.supports_mpp = false;
        assert!(run(&g, &params(1000, Some(100)), &policy(a)).is_err());
        let mut p = policy(a);
        p.max_paths = Some(1);
        assert!(run(&diamond(), &params(1000, Some(100)), &p).is_err());
        assert!(run(&diamond(), &params(1000, Some(1)), &policy(a)).is_err());
    }
}
#[test]
fn fees_htlc_minimum_and_cltv_are_checked_on_materialized_routes() {
    let mut g = Graph::new(key(1), key(4), 18, true);
    edge(&mut g, 1, 2, 12, 100000, 0, 0, 0);
    edge(&mut g, 2, 4, 24, 100000, 0, 25, 0);
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        let mut p = policy(a);
        p.max_fee_msat = Some(24);
        assert!(run(&g, &params(1000, None), &p).is_err());
        p.max_fee_msat = Some(25);
        let route = run(&g, &params(1000, None), &p).unwrap();
        assert_eq!(route.get_total_fees(), 25);
        p.max_cltv_delta = Some(27);
        assert!(run(&g, &params(1000, None), &p).is_err());
    }
    let mut g = diamond();
    g.edges[0].htlc_min_msat = 601;
    assert!(run(&g, &params(1000, Some(100)), &policy("yen")).is_err());
}
#[test]
fn validator_rejects_tampered_totals_contracts_exclusions_and_capacity() {
    let g = diamond();
    let p = policy("yen");
    let params = p.constrain(&params(1000, Some(100)));
    let route = run(&g, &params, &p).unwrap();
    let mut bad = route.clone();
    bad.paths[0].hops.last_mut().unwrap().fee_msat += 1;
    assert!(validation::validate(&key(1), &params, &p, &bad, Some(&g)).is_err());
    let mut bad = route.clone();
    bad.paths[0].hops[0].rgb_payment = Some((ContractId::from([8; 32]), 60));
    assert!(validation::validate(&key(1), &params, &p, &bad, Some(&g)).is_err());
    let mut p = p.clone();
    p.excluded_nodes.push(key(2).to_string());
    assert!(validation::validate(&key(1), &params, &p, &route, Some(&g)).is_err());
    let p = policy("yen");
    let mut g = g.clone();
    g.edges[0].capacity_rgb = 59;
    assert!(validation::validate(&key(1), &params, &p, &route, Some(&g)).is_err());
}
#[test]
fn search_stops_at_the_expansion_budget() {
    for a in ["yen", "min_cost_flow", "multi_objective"] {
        let mut p = policy(a);
        p.max_expansions = 1;
        assert!(run(&diamond(), &params(1000, Some(100)), &p)
            .unwrap_err()
            .contains("budget"));
    }
}
#[test]
fn policy_survives_restart_is_immutable_and_keeps_retry_fee_budget() {
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(FilesystemStore::new(dir.path().to_path_buf()));
    let id = PaymentId([42; 32]);
    let store = PolicyStore::new(fs.clone());
    assert!(store.get(id).is_err());
    let p = policy("yen");
    store.register(id, &p).unwrap();
    drop(store);
    let store = PolicyStore::new(fs);
    assert_eq!(store.get(id).unwrap(), p);
    assert!(store.register(id, &policy("min_cost_flow")).is_err());
    let mut retry = params(400, Some(40));
    retry.max_total_routing_fee_msat = Some(17);
    assert_eq!(p.constrain(&retry).max_total_routing_fee_msat, Some(17));
}
#[test]
fn rgb_inflight_is_directional_and_roundtrips() {
    let g = diamond();
    let route = run(&g, &params(1000, Some(100)), &policy("yen")).unwrap();
    let mut inflight = InFlightHtlcs::new();
    for path in &route.paths {
        inflight.process_path(path, key(1));
    }
    let source = NodeId::from_pubkey(&key(1));
    let target = NodeId::from_pubkey(&key(2));
    assert_eq!(inflight.used_liquidity_rgb(&source, &target, 12), Some(60));
    assert_eq!(inflight.used_liquidity_rgb(&target, &source, 12), None);
    let encoded = inflight.encode();
    let decoded = InFlightHtlcs::read(&mut &encoded[..]).unwrap();
    assert_eq!(decoded.used_liquidity_rgb(&source, &target, 12), Some(60));
}
#[test]
fn telemetry_records_decisions_and_outcomes_as_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = Telemetry::open(dir.path()).unwrap();
    telemetry.record(RoutingEvent::new(
        Some(PaymentId([1; 32])),
        Algorithm::Yen,
        "route_planned",
    ));
    telemetry.record(RoutingEvent::new(
        Some(PaymentId([1; 32])),
        Algorithm::Yen,
        "payment_succeeded",
    ));
    assert_eq!(telemetry.recent().len(), 2);
    let text = std::fs::read_to_string(dir.path().join("routing-events.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 2);
    for line in text.lines() {
        assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
    }
}
#[test]
fn adaptive_router_fails_closed_without_a_policy_after_restart_too() {
    let dir = tempfile::tempdir().unwrap();
    let logger = Arc::new(crate::disk::FilesystemLogger::new(dir.path().to_path_buf()));
    let network = Arc::new(crate::ldk::NetworkGraph::new(
        Network::Regtest,
        logger.clone(),
    ));
    let scorer = Arc::new(RwLock::new(crate::ldk::Scorer::new(
        Default::default(),
        network.clone(),
        logger.clone(),
    )));
    let keys = Arc::new(KeysManager::new(
        &[1; 32],
        0,
        0,
        false,
        dir.path().to_path_buf(),
    ));
    let fs = Arc::new(FilesystemStore::new(dir.path().to_path_buf()));
    let router = AdaptiveRouter::new(
        network.clone(),
        logger.clone(),
        keys.clone(),
        scorer.clone(),
        fs.clone(),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let p = params(1000, None);
    assert!(router
        .find_route(&key(1), &p, None, InFlightHtlcs::new())
        .is_err());
    assert!(router
        .find_route_with_id(
            &key(1),
            &p,
            None,
            InFlightHtlcs::new(),
            PaymentHash([2; 32]),
            PaymentId([2; 32])
        )
        .unwrap_err()
        .contains("policy"));
    router.register(PaymentId([2; 32]), &policy("yen")).unwrap();
    drop(router);
    let router =
        AdaptiveRouter::new(network, logger, keys, scorer, fs, dir.path().to_path_buf()).unwrap();
    assert!(router
        .find_route_with_id(
            &key(1),
            &p,
            None,
            InFlightHtlcs::new(),
            PaymentHash([2; 32]),
            PaymentId([2; 32])
        )
        .unwrap_err()
        .contains("Yen"));
    assert_eq!(router.recent_events()[0].algorithm, Algorithm::Yen);
}

fn local_channel(
    peer: PublicKey,
    scid: u64,
    capacity: u64,
    rgb: u64,
) -> lightning::ln::channel_state::ChannelDetails {
    use lightning::ln::{
        channel_state::{ChannelCounterparty, ChannelDetails},
        types::ChannelId,
    };
    use lightning::types::features::InitFeatures;
    use lightning::util::config::ChannelConfig;
    ChannelDetails {
        channel_id: ChannelId([scid as u8; 32]),
        counterparty: ChannelCounterparty {
            features: InitFeatures::empty(),
            node_id: peer,
            unspendable_punishment_reserve: 0,
            forwarding_info: None,
            outbound_htlc_minimum_msat: None,
            outbound_htlc_maximum_msat: None,
        },
        funding_txo: None,
        funding_redeem_script: None,
        channel_type: None,
        short_channel_id: Some(scid),
        outbound_scid_alias: None,
        inbound_scid_alias: None,
        channel_value_satoshis: 1000,
        user_channel_id: scid as u128,
        outbound_capacity_msat: capacity,
        next_outbound_htlc_limit_msat: capacity,
        next_outbound_htlc_minimum_msat: 0,
        inbound_capacity_msat: 0,
        unspendable_punishment_reserve: None,
        confirmations_required: Some(0),
        confirmations: Some(0),
        force_close_spend_delay: None,
        is_outbound: true,
        is_channel_ready: true,
        is_usable: true,
        is_announced: false,
        inbound_htlc_minimum_msat: None,
        inbound_htlc_maximum_msat: None,
        config: Some(ChannelConfig::default()),
        feerate_sat_per_1000_weight: None,
        channel_shutdown_state: None,
        pending_inbound_htlcs: vec![],
        pending_outbound_htlcs: vec![],
        next_outbound_htlc_limit_rgb: rgb,
        inbound_htlc_maximum_rgb: 0,
    }
}

#[test]
fn adaptive_router_dispatches_all_four_planners_and_reloads_the_same_policy_for_rgb_retries() {
    use lightning::{
        rgb_utils::{get_rgb_channel_info_path, write_rgb_channel_info, RgbInfo},
        routing::router::{RouteHint, RouteHintHop},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let logger = Arc::new(crate::disk::FilesystemLogger::new(path.clone()));
    let network = Arc::new(crate::ldk::NetworkGraph::new(
        Network::Regtest,
        logger.clone(),
    ));
    let scorer = Arc::new(RwLock::new(crate::ldk::Scorer::new(
        Default::default(),
        network.clone(),
        logger.clone(),
    )));
    let keys = Arc::new(KeysManager::new(&[1; 32], 0, 0, false, path.clone()));
    let fs = Arc::new(FilesystemStore::new(path.clone()));
    let contract = ContractId::from([7; 32]);
    let alice_bob = local_channel(key(2), 12, 600, 60);
    let alice_carol = local_channel(key(3), 13, 400, 40);
    for (channel, amount) in [(&alice_bob, 60), (&alice_carol, 40)] {
        write_rgb_channel_info(
            &get_rgb_channel_info_path(&channel.channel_id.to_string(), &path, false),
            &RgbInfo {
                contract_id: contract,
                schema: rgb_lib::AssetSchema::Nia,
                local_rgb_amount: amount,
                remote_rgb_amount: 0,
                batch_transfer_idx: None,
                counterparty_knows_asset: true,
            },
        );
    }
    let mut p = params(1000, Some(100));
    if let lightning::routing::router::Payee::Clear { route_hints, .. } =
        &mut p.payment_params.payee
    {
        for (peer, scid, cap, rgb) in [(2, 24, 600, 60), (3, 34, 400, 40)] {
            route_hints.push(RouteHint(vec![RouteHintHop {
                src_node_id: key(peer),
                short_channel_id: scid,
                fees: RoutingFees {
                    base_msat: 0,
                    proportional_millionths: 0,
                },
                cltv_expiry_delta: 10,
                htlc_minimum_msat: Some(0),
                htlc_maximum_msat: Some(cap),
                htlc_maximum_rgb: Some(rgb),
            }]));
        }
    }
    for (i, a) in ["ldk_dijkstra", "yen", "min_cost_flow", "multi_objective"]
        .iter()
        .enumerate()
    {
        let id = PaymentId([20 + i as u8; 32]);
        let hash = PaymentHash(id.0);
        let routing = policy(a);
        let router = AdaptiveRouter::new(
            network.clone(),
            logger.clone(),
            keys.clone(),
            scorer.clone(),
            fs.clone(),
            path.clone(),
        )
        .unwrap();
        router.register(id, &routing).unwrap();
        let route = router
            .find_route_with_id(
                &key(1),
                &p,
                Some(&[&alice_bob, &alice_carol]),
                InFlightHtlcs::new(),
                hash,
                id,
            )
            .unwrap_or_else(|e| panic!("{a}: {e}"));
        assert_eq!(route.paths.len(), 2, "{a}");
        assert_eq!(route.route_params.as_ref(), Some(&p));
        let held = route
            .paths
            .iter()
            .find(|part| part.hops[0].pubkey == key(2))
            .unwrap();
        let mut inflight = InFlightHtlcs::new();
        inflight.process_path(held, key(1));
        drop(router);
        let restored = AdaptiveRouter::new(
            network.clone(),
            logger.clone(),
            keys.clone(),
            scorer.clone(),
            fs.clone(),
            path.clone(),
        )
        .unwrap();
        let mut retry = p.clone();
        retry.final_value_msat = 400;
        retry.rgb_payment = Some((contract, 40));
        retry.max_total_routing_fee_msat = Some(0);
        let mut busy_bob = alice_bob.clone();
        busy_bob.outbound_capacity_msat = 0;
        busy_bob.next_outbound_htlc_limit_msat = 0;
        busy_bob.next_outbound_htlc_limit_rgb = 0;
        let route = restored
            .find_route_with_id(
                &key(1),
                &retry,
                Some(&[&busy_bob, &alice_carol]),
                inflight,
                hash,
                id,
            )
            .unwrap_or_else(|e| panic!("retry {a}: {e}"));
        assert_eq!(route.paths.len(), 1);
        assert_eq!(route.paths[0].final_value_msat(), 400);
        assert_eq!(route.paths[0].hops[0].pubkey, key(3));
        assert_eq!(route.paths[0].hops[0].rgb_payment, Some((contract, 40)));
        assert_eq!(restored.recent_events()[0].algorithm, routing.algorithm);
        assert_eq!(restored.recent_events()[0].amount_msat, Some(400));
    }
}

#[tokio::test]
async fn http_api_rejects_missing_unknown_and_null_algorithms_before_payment_handling() {
    let directory = tempfile::tempdir().unwrap();
    let args = crate::args::UserArgs {
        storage_dir_path: directory.path().to_path_buf(),
        daemon_listening_port: 0,
        ldk_peer_listening_port: 0,
        network: rgb_lib::BitcoinNetwork::Regtest,
        max_media_upload_size_mb: 5,
        max_aggregated_media_size_per_channel_mb: 10,
        max_pending_consignments: 10,
        max_media_files_per_channel: 10,
        root_public_key: None,
    };
    let (app, _state) = crate::app(args).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let caps: serde_json::Value = client
        .get(format!("{base}/routing/capabilities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(caps["requires_explicit_algorithm"], true);
    assert_eq!(caps["automatic_algorithm_fallback"], false);
    assert!(caps.get("default_algorithm").is_none());
    for (path, body) in [
        ("sendpayment", serde_json::json!({"invoice":"test"})),
        (
            "sendpayment",
            serde_json::json!({"invoice":"test","routing":{}}),
        ),
        (
            "sendpayment",
            serde_json::json!({"invoice":"test","routing":{"algorithm":null}}),
        ),
        (
            "sendpayment",
            serde_json::json!({"invoice":"test","routing":{"algorithm":"auto"}}),
        ),
        (
            "keysend",
            serde_json::json!({"dest_pubkey":key(4).to_string(),"amt_msat":3000000}),
        ),
        (
            "makerexecute",
            serde_json::json!({"swapstring":"x","payment_secret":"x","taker_pubkey":key(4).to_string()}),
        ),
    ] {
        let response = client
            .post(format!("{base}/{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(error["name"], "InvalidRequest");
    }
    server.abort();
    let _ = server.await;
}

#[test]
fn single_path_requests_skip_cheaper_partial_paths() {
    for algorithm in ["yen", "min_cost_flow", "multi_objective"] {
        let mut g = Graph::new(key(1), key(4), 18, false);
        edge(&mut g, 1, 2, 12, 60, 0, 0, 0);
        edge(&mut g, 2, 4, 24, 60, 0, 0, 0);
        edge(&mut g, 1, 3, 13, 1000, 0, 0, 0);
        edge(&mut g, 3, 4, 34, 1000, 0, 1, 0);
        let mut p = policy(algorithm);
        p.max_paths = Some(1);
        p.max_fee_msat = Some(1);
        let route = run(&g, &params(100, None), &p).unwrap();
        assert_eq!(route.paths.len(), 1);
        assert_eq!(route.paths[0].hops[0].pubkey, key(3));
    }
}

#[test]
fn min_cost_flow_matches_an_exhaustive_oracle_on_small_linear_networks() {
    fn paths(
        g: &Graph,
        node: usize,
        seen: &mut Vec<usize>,
        current: &mut Vec<usize>,
        out: &mut Vec<Vec<usize>>,
    ) {
        if node == g.target {
            out.push(current.clone());
            return;
        }
        for i in &g.outgoing[node] {
            let target = g.edges[*i].target;
            if seen.contains(&target) {
                continue;
            }
            seen.push(target);
            current.push(*i);
            paths(g, target, seen, current, out);
            current.pop();
            seen.pop();
        }
    }
    let mut random = 17u64;
    for case in 0..64 {
        let mut g = Graph::new(key(1), key(4), 18, true);
        for (i, (from, to)) in [(1, 2), (1, 3), (2, 3), (3, 2), (2, 4), (3, 4), (1, 4)]
            .into_iter()
            .enumerate()
        {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let capacity = random % 3;
            let cost = (random >> 8) % 7;
            edge(&mut g, from, to, 100 + i as u64, capacity, 0, 0, cost);
        }
        let mut candidates = vec![];
        paths(
            &g,
            g.source,
            &mut vec![g.source],
            &mut vec![],
            &mut candidates,
        );
        let mut expected: Option<u128> = None;
        for a in &candidates {
            for b in &candidates {
                let mut used = vec![0u64; g.edges.len()];
                for i in a.iter().chain(b) {
                    used[*i] += 1;
                }
                if used
                    .iter()
                    .enumerate()
                    .any(|(i, n)| *n > g.edges[i].capacity_msat)
                {
                    continue;
                }
                let cost = a.iter().chain(b).map(|i| g.edges[*i].penalty as u128).sum();
                expected = Some(expected.map_or(cost, |old| old.min(cost)));
            }
        }
        let result = run(&g, &params(2, None), &policy("min_cost_flow"));
        match expected {
            None => assert!(result.is_err(), "case {case}: infeasible oracle"),
            Some(expected) => {
                let route = result.unwrap_or_else(|e| panic!("case {case}: {e}"));
                let mut actual = 0u128;
                for path in &route.paths {
                    let mut source = key(1);
                    for hop in &path.hops {
                        let i = g.edge(&source, &hop.pubkey, hop.short_channel_id).unwrap();
                        actual += g.edges[i].penalty as u128 * path.final_value_msat() as u128;
                        source = hop.pubkey;
                    }
                }
                assert_eq!(actual, expected, "case {case}: linear optimum");
            }
        }
    }
}
