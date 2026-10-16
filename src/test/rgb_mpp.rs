use super::*;

const TEST_DIR_BASE: &str = "tmp/rgb_mpp/";
const INCOMPLETE_TEST_DIR_BASE: &str = "tmp/rgb_mpp_incomplete/";

async fn wait_for_rgb_mpp_payment(
    node: SocketAddr,
    payment_hash: &str,
    expected_status: HTLCStatus,
) -> Payment {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if let Some(payment) = check_payment_status(node, payment_hash, expected_status).await {
                break payment;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("RGB MPP {payment_hash} did not reach {expected_status:?}"))
}

async fn wait_for_rgb_channel_balances(
    node: SocketAddr,
    channel_id: &str,
    local: u64,
    remote: u64,
    pending_ldk_dir: Option<&Path>,
) {
    let started = std::time::Instant::now();
    loop {
        let channels = list_channels(node).await;
        let channel = channels
            .iter()
            .find(|c| c.channel_id == channel_id)
            .unwrap();
        let pending = pending_ldk_dir.map(|ldk_dir| {
            let pending_path =
                lightning::rgb_utils::get_rgb_channel_info_path(channel_id, ldk_dir, true);
            let info = lightning::rgb_utils::parse_rgb_channel_info(&pending_path);
            (info.local_rgb_amount, info.remote_rgb_amount)
        });
        if channel.asset_local_amount == Some(local)
            && channel.asset_remote_amount == Some(remote)
            && pending.is_none_or(|amounts| amounts == (local, remote))
        {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "RGB balances on {node}, channel {channel_id}: settled {:?}/{:?}, pending {pending:?}, expected {local}/{remote}",
            channel.asset_local_amount, channel.asset_remote_amount,
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[test]
fn rgb_mpp_requires_both_totals_before_claiming() {
    use lightning::ln::channelmanager::test_mpp_totals_complete as complete;

    assert!(complete(10_000_000, 10_000_000, Some(100), Some(100)));
    assert!(!complete(10_000_000, 10_000_000, Some(40), Some(100)));
    assert!(!complete(9_000_000, 10_000_000, Some(100), Some(100)));
    assert!(!complete(10_000_000, 10_000_000, None, Some(100)));
    assert!(!complete(10_000_000, 10_000_000, Some(101), Some(100)));
    assert!(complete(10_000_000, 10_000_000, None, None));
}

#[serial_test::serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[traced_test]
async fn rgb_mpp_uses_two_rgb_limited_routes() {
    initialize();

    let (alice, _) = start_node(&format!("{TEST_DIR_BASE}alice"), NODE1_PEER_PORT, false).await;
    let (bob, _) = start_node(&format!("{TEST_DIR_BASE}bob"), NODE2_PEER_PORT, false).await;
    let (carol, _) = start_node(&format!("{TEST_DIR_BASE}carol"), NODE3_PEER_PORT, false).await;
    let (dave, _) = start_node(&format!("{TEST_DIR_BASE}dave"), NODE4_PEER_PORT, false).await;

    for node in [alice, bob, carol, dave] {
        fund_and_create_utxos(node, None).await;
    }

    let asset_id = issue_asset_nia(alice).await.asset_id;
    for node in [bob, carol] {
        let recipient_id = rgb_invoice(node, None, false).await.recipient_id;
        send_asset(
            alice,
            &asset_id,
            Assignment::Fungible(300),
            recipient_id,
            None,
        )
        .await;
        mine(false);
        refresh_transfers(node).await;
        refresh_transfers(node).await;
        refresh_transfers(alice).await;
    }
    assert_eq!(asset_balance_spendable(alice, &asset_id).await, 400);

    let bob_pubkey = node_info(bob).await.pubkey;
    let carol_pubkey = node_info(carol).await.pubkey;
    let dave_pubkey = node_info(dave).await.pubkey;

    // Give every hop BTC liquidity in both directions for the return payment,
    // while preserving the channel reserves and the minimum BTC value per HTLC.
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let alice_bob = open_channel(
        alice,
        &bob_pubkey,
        Some(NODE2_PEER_PORT),
        None,
        Some(10_000_000),
        Some(60),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let alice_carol = open_channel(
        alice,
        &carol_pubkey,
        Some(NODE3_PEER_PORT),
        None,
        Some(10_000_000),
        Some(40),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let bob_dave = open_channel(
        bob,
        &dave_pubkey,
        Some(NODE4_PEER_PORT),
        None,
        Some(10_000_000),
        Some(60),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let carol_dave = open_channel(
        carol,
        &dave_pubkey,
        Some(NODE4_PEER_PORT),
        None,
        Some(10_000_000),
        Some(40),
        Some(&asset_id),
    )
    .await;

    let LNInvoiceResponse { invoice } =
        ln_invoice(dave, Some(10_000_000), Some(&asset_id), Some(100), 900).await;
    let sent = send_payment_raw(alice, invoice).await;
    let payment =
        wait_for_rgb_mpp_payment(alice, &sent.payment_hash.unwrap(), HTLCStatus::Succeeded).await;
    let payment_info = lightning::rgb_utils::parse_rgb_payment_info(&PathBuf::from(format!(
        "{TEST_DIR_BASE}alice/.ldk/{}.outbound",
        payment.payment_hash
    )));
    assert_eq!(payment_info.amount, 100);
    assert!(!payment_info.swap_payment);

    for (node, channel_id, expected_local, expected_remote) in [
        (alice, alice_bob.channel_id.clone(), 0, 60),
        (alice, alice_carol.channel_id.clone(), 0, 40),
        (bob, alice_bob.channel_id.clone(), 60, 0),
        (bob, bob_dave.channel_id.clone(), 0, 60),
        (carol, alice_carol.channel_id.clone(), 40, 0),
        (carol, carol_dave.channel_id.clone(), 0, 40),
        (dave, bob_dave.channel_id.clone(), 60, 0),
        (dave, carol_dave.channel_id.clone(), 40, 0),
    ] {
        wait_for_rgb_channel_balances(node, &channel_id, expected_local, expected_remote, None)
            .await;
    }

    // Pay back along the reverse routes using MPP keysend, checking its shared
    // payment secret and spontaneous receive aggregation as well as RGB balances.
    let alice_pubkey = node_info(alice).await.pubkey;
    let returned = keysend_raw(
        dave,
        &alice_pubkey,
        Some(10_000_000),
        Some(&asset_id),
        Some(100),
    )
    .await;
    wait_for_rgb_mpp_payment(dave, &returned.payment_hash, HTLCStatus::Succeeded).await;
    for (node, channel_id, expected_local, expected_remote) in [
        (alice, &alice_bob.channel_id, 60, 0),
        (alice, &alice_carol.channel_id, 40, 0),
        (bob, &alice_bob.channel_id, 0, 60),
        (bob, &bob_dave.channel_id, 60, 0),
        (carol, &alice_carol.channel_id, 0, 40),
        (carol, &carol_dave.channel_id, 40, 0),
        (dave, &bob_dave.channel_id, 0, 60),
        (dave, &carol_dave.channel_id, 0, 40),
    ] {
        wait_for_rgb_channel_balances(node, channel_id, expected_local, expected_remote, None)
            .await;
    }

    shutdown(&[alice, bob, carol, dave]).await;
}

// Bob's outbound RGB channel is drained after Alice has learned its advertised total
// capacity. Alice can select both routes, but Bob cannot forward its 60-unit part.
#[serial_test::serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[traced_test]
async fn rgb_mpp_incomplete_part_never_settles() {
    initialize();

    let (alice, _) = start_node(
        &format!("{INCOMPLETE_TEST_DIR_BASE}alice"),
        NODE1_PEER_PORT,
        false,
    )
    .await;
    let (bob, _) = start_node(
        &format!("{INCOMPLETE_TEST_DIR_BASE}bob"),
        NODE2_PEER_PORT,
        false,
    )
    .await;
    let (carol, _) = start_node(
        &format!("{INCOMPLETE_TEST_DIR_BASE}carol"),
        NODE3_PEER_PORT,
        false,
    )
    .await;
    let (dave, _) = start_node(
        &format!("{INCOMPLETE_TEST_DIR_BASE}dave"),
        NODE4_PEER_PORT,
        false,
    )
    .await;

    for node in [alice, bob, carol, dave] {
        fund_and_create_utxos(node, None).await;
    }

    let asset_id = issue_asset_nia(alice).await.asset_id;
    for node in [bob, carol] {
        let recipient_id = rgb_invoice(node, None, false).await.recipient_id;
        send_asset(
            alice,
            &asset_id,
            Assignment::Fungible(300),
            recipient_id,
            None,
        )
        .await;
        mine(false);
        refresh_transfers(node).await;
        refresh_transfers(node).await;
        refresh_transfers(alice).await;
    }

    let bob_pubkey = node_info(bob).await.pubkey;
    let carol_pubkey = node_info(carol).await.pubkey;
    let dave_pubkey = node_info(dave).await.pubkey;

    *MOCK_FEE.lock().unwrap() = Some(3000);
    let alice_bob = open_channel(
        alice,
        &bob_pubkey,
        Some(NODE2_PEER_PORT),
        None,
        None,
        Some(60),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let alice_carol = open_channel(
        alice,
        &carol_pubkey,
        Some(NODE3_PEER_PORT),
        None,
        None,
        Some(40),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let bob_dave = open_channel(
        bob,
        &dave_pubkey,
        Some(NODE4_PEER_PORT),
        None,
        None,
        Some(60),
        Some(&asset_id),
    )
    .await;
    *MOCK_FEE.lock().unwrap() = Some(3000);
    let carol_dave = open_channel(
        carol,
        &dave_pubkey,
        Some(NODE4_PEER_PORT),
        None,
        None,
        Some(40),
        Some(&asset_id),
    )
    .await;

    let drained = keysend_raw(
        bob,
        &dave_pubkey,
        Some(3_000_000),
        Some(&asset_id),
        Some(60),
    )
    .await;
    wait_for_rgb_mpp_payment(bob, &drained.payment_hash, HTLCStatus::Succeeded).await;
    let bob_ldk_dir = PathBuf::from(format!("{INCOMPLETE_TEST_DIR_BASE}bob/.ldk"));
    wait_for_rgb_channel_balances(bob, &bob_dave.channel_id, 0, 60, Some(&bob_ldk_dir)).await;

    for keysend_payment in [false, true] {
        let payment_hash = if keysend_payment {
            keysend_raw(
                alice,
                &dave_pubkey,
                Some(10_000_000),
                Some(&asset_id),
                Some(100),
            )
            .await
            .payment_hash
        } else {
            let LNInvoiceResponse { invoice } =
                ln_invoice(dave, Some(10_000_000), Some(&asset_id), Some(100), 900).await;
            send_payment_raw(alice, invoice).await.payment_hash.unwrap()
        };

        // This checks that the surviving 40-unit HTLC reached Dave. Otherwise a route
        // lookup failure before any HTLC was sent could falsely pass this test.
        let dave_ldk_log = format!("{INCOMPLETE_TEST_DIR_BASE}dave/.ldk/logs/logs.txt");
        let started = OffsetDateTime::now_utc();
        loop {
            let log = std::fs::read_to_string(&dave_ldk_log).unwrap_or_default();
            if log.lines().any(|line| {
                line.contains("Received message UpdateAddHTLC")
                    && line.contains(&format!("payment_hash: {payment_hash}"))
                    && line.contains("amount_msat: 4000000")
                    && line.contains("rgb_payment: Some(")
            }) {
                break;
            }
            assert!(
                (OffsetDateTime::now_utc() - started).as_seconds_f32() < 60.0,
                "the surviving RGB MPP part did not reach Dave"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let inbound = list_payments(dave)
            .await
            .into_iter()
            .find(|payment| payment.payment_hash == payment_hash);
        if let Some(inbound) = inbound {
            assert_eq!(inbound.status, HTLCStatus::Pending);
            assert!(inbound.preimage.is_none());
        } else {
            assert!(keysend_payment, "invoice payment missing from the receiver");
        }

        // Wait for the receive processing and its MPPTimeout, advancing the receiver's
        // clock explicitly. This avoids relying on the speed of the commitment exchange.
        // The endpoint exists only in test builds.
        let timeout_started = OffsetDateTime::now_utc();
        loop {
            let log = std::fs::read_to_string(&dave_ldk_log).unwrap();
            assert!(
                !log.lines().any(|line| {
                    line.contains("Handling event PaymentClaimable") && line.contains(&payment_hash)
                }),
                "an incomplete RGB MPP became claimable"
            );
            if log
                .lines()
                .any(|line| line.contains("MPPTimeout") && line.contains(&payment_hash))
            {
                break;
            }
            assert!(
                (OffsetDateTime::now_utc() - timeout_started).as_seconds_f32() < 60.0,
                "the incomplete RGB MPP did not time out"
            );
            let response = reqwest::Client::new()
                .post(format!("http://{dave}/test/timertick"))
                .send()
                .await
                .unwrap();
            check_response_is_ok(response).await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        // Failure must traverse two commitment exchanges on each hop before the
        // sender's final event. Allow that propagation independently of Dave's timeout.
        let failed = wait_for_rgb_mpp_payment(alice, &payment_hash, HTLCStatus::Failed).await;
        assert!(failed.preimage.is_none());
        let inbound = list_payments(dave)
            .await
            .into_iter()
            .find(|payment| payment.payment_hash == payment_hash);
        if let Some(inbound) = inbound {
            assert_ne!(inbound.status, HTLCStatus::Succeeded);
            assert!(inbound.preimage.is_none());
        } else {
            assert!(keysend_payment, "invoice payment missing from the receiver");
        }

        for (node, channel_id, expected_local, expected_remote) in [
            (alice, &alice_bob.channel_id, 60, 0),
            (alice, &alice_carol.channel_id, 40, 0),
            (bob, &alice_bob.channel_id, 0, 60),
            (bob, &bob_dave.channel_id, 0, 60),
            (carol, &alice_carol.channel_id, 0, 40),
            (carol, &carol_dave.channel_id, 40, 0),
            (dave, &bob_dave.channel_id, 60, 0),
            (dave, &carol_dave.channel_id, 0, 40),
        ] {
            // Failed parts must also release the protocol's pending allocations,
            // so another payment can reuse the same channel liquidity.
            let node_name = if node == alice {
                "alice"
            } else if node == bob {
                "bob"
            } else if node == carol {
                "carol"
            } else {
                "dave"
            };
            let ldk_dir = PathBuf::from(format!("{INCOMPLETE_TEST_DIR_BASE}{node_name}/.ldk"));
            wait_for_rgb_channel_balances(
                node,
                channel_id,
                expected_local,
                expected_remote,
                Some(&ldk_dir),
            )
            .await;
        }
    }

    shutdown(&[alice, bob, carol, dave]).await;
}
