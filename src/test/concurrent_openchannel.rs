use super::*;

const TEST_DIR_BASE: &str = "tmp/concurrent_openchannel/";

#[serial_test::serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[traced_test]
async fn concurrent_openchannel() {
    initialize();

    const CHANNEL_CAPACITY_SAT: u64 = 100000;
    const PUSH_MSAT: u64 = 20000;
    const ASSET_AMOUNT: u64 = 100;

    let node1_peer_port = NODE1_PEER_PORT;
    let node2_peer_port = NODE2_PEER_PORT;
    let node3_peer_port = NODE3_PEER_PORT;
    let node4_peer_port = NODE4_PEER_PORT;
    let node5_peer_port = NODE5_PEER_PORT;
    let node6_peer_port = NODE6_PEER_PORT;

    let test_dir_node1 = format!("{TEST_DIR_BASE}node1");
    let test_dir_node2 = format!("{TEST_DIR_BASE}node2");
    let test_dir_node3 = format!("{TEST_DIR_BASE}node3");
    let test_dir_node4 = format!("{TEST_DIR_BASE}node4");
    let test_dir_node5 = format!("{TEST_DIR_BASE}node5");
    let test_dir_node6 = format!("{TEST_DIR_BASE}node6");

    let (node1_addr, _) = start_node(&test_dir_node1, node1_peer_port, false).await;
    let (node2_addr, _) = start_node(&test_dir_node2, node2_peer_port, false).await;
    let (node3_addr, _) = start_node(&test_dir_node3, node3_peer_port, false).await;
    let (node4_addr, _) = start_node(&test_dir_node4, node4_peer_port, false).await;
    let (node5_addr, _) = start_node(&test_dir_node5, node5_peer_port, false).await;
    let (node6_addr, _) = start_node(&test_dir_node6, node6_peer_port, false).await;

    // Put RGB on inputs that also cover each channel's BTC funding and fees.
    // Small RGB inputs would make parallel fundings compete for a BTC input.
    fund_wallet(address(node1_addr).await, 100_000_000);
    mine(false);
    create_utxos(node1_addr, false, Some(5), Some(110000)).await;
    mine(false);
    fund_and_create_utxos(node2_addr, None).await;
    fund_and_create_utxos(node3_addr, None).await;
    fund_and_create_utxos(node4_addr, None).await;
    fund_and_create_utxos(node5_addr, None).await;
    fund_and_create_utxos(node6_addr, None).await;

    // Each concurrent funding needs an independently spendable RGB allocation.
    // One 1000-unit allocation cannot fund five transactions at the same time.
    let asset_id = issue_asset_nia_with_amounts(node1_addr, vec![ISSUE_AMT / 5; 5])
        .await
        .asset_id;

    let _node1_pubkey = node_info(node1_addr).await.pubkey;
    let node2_pubkey = node_info(node2_addr).await.pubkey;
    let node3_pubkey = node_info(node3_addr).await.pubkey;
    let node4_pubkey = node_info(node4_addr).await.pubkey;
    let node5_pubkey = node_info(node5_addr).await.pubkey;
    let node6_pubkey = node_info(node6_addr).await.pubkey;

    let peers = [
        (&node2_pubkey, node2_peer_port),
        (&node3_pubkey, node3_peer_port),
        (&node4_pubkey, node4_peer_port),
        (&node5_pubkey, node5_peer_port),
        (&node6_pubkey, node6_peer_port),
    ];
    let requests = peers.iter().map(|(pubkey, port)| {
        open_channel_raw(
            node1_addr,
            pubkey,
            Some(*port),
            Some(CHANNEL_CAPACITY_SAT),
            Some(PUSH_MSAT),
            Some(ASSET_AMOUNT),
            Some(&asset_id),
            None,
            None,
            None,
            None,
            true,
            true,
        )
    });
    for result in futures::future::join_all(requests).await {
        result.expect("concurrent channel creation should succeed");
    }

    // Confirm all funding transactions together. A per-channel helper can
    // mine while another opening is still waiting for an earlier chain tip,
    // and its 10-second readiness deadline is too short for five fundings.
    let started = std::time::Instant::now();
    let mut confirmed = false;
    loop {
        let channels = list_channels(node1_addr).await;
        if !confirmed
            && channels.len() == peers.len()
            && channels.iter().all(|channel| {
                channel
                    .funding_txid
                    .as_ref()
                    .is_some_and(|txid| !get_txout(txid).is_empty())
            })
        {
            mine_n_blocks(false, 6);
            confirmed = true;
        }
        if channels.len() == peers.len() && channels.iter().all(|channel| channel.ready) {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "five concurrent RGB channels did not become ready"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let node1_usable_channels = node_info(node1_addr).await.num_usable_channels;
    assert_eq!(node1_usable_channels, 5);
}
