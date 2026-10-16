use super::{
    algorithms::{self, graph::Graph, SearchBudget},
    policy::{Algorithm, PolicyStore, RoutingPolicy},
    telemetry::{RoutingEvent, Telemetry},
    validation,
};
use crate::{
    disk::FilesystemLogger,
    ldk::{NetworkGraph, Scorer},
};
use bitcoin::secp256k1::{self, PublicKey, Secp256k1};
use lightning::{
    blinded_path::payment::{BlindedPaymentPath, ReceiveTlvs},
    ln::{channel_state::ChannelDetails, channelmanager::PaymentId},
    routing::{
        gossip::NodeId,
        router::{
            add_random_cltv_offset, DefaultRouter, InFlightHtlcs, Payee, Route, RouteParameters,
            Router,
        },
        scoring::ProbabilisticScoringFeeParameters,
    },
    sign::{EntropySource, KeysManager},
    types::payment::PaymentHash,
};
use lightning_persister::fs_store::FilesystemStore;
use std::{
    io,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Instant,
};

/// Concrete Router used by the ChannelManager for initial sends and automatic retries.
pub(crate) struct AdaptiveRouter {
    network: Arc<NetworkGraph>,
    logger: Arc<FilesystemLogger>,
    keys: Arc<KeysManager>,
    scorer: Arc<RwLock<Scorer>>,
    policies: PolicyStore,
    telemetry: Telemetry,
    ldk_dir: PathBuf,
}
impl AdaptiveRouter {
    pub fn new(
        network: Arc<NetworkGraph>,
        logger: Arc<FilesystemLogger>,
        keys: Arc<KeysManager>,
        scorer: Arc<RwLock<Scorer>>,
        store: Arc<FilesystemStore>,
        ldk_dir: PathBuf,
    ) -> io::Result<Self> {
        Ok(Self {
            network,
            logger,
            keys,
            scorer,
            policies: PolicyStore::new(store),
            telemetry: Telemetry::open(&ldk_dir)?,
            ldk_dir,
        })
    }
    pub fn register(
        &self,
        id: PaymentId,
        policy: &RoutingPolicy,
    ) -> Result<(), crate::error::APIError> {
        self.policies.register(id, policy).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                crate::error::APIError::InvalidRequest(e.to_string())
            } else {
                crate::error::APIError::IO(e)
            }
        })
    }
    pub fn recent_events(&self) -> Vec<RoutingEvent> {
        self.telemetry.recent()
    }
    pub fn outcome(&self, id: PaymentId, kind: &str, fee_msat: Option<u64>, error: Option<String>) {
        if let Ok(policy) = self.policies.get(id) {
            let mut event = RoutingEvent::new(Some(id), policy.algorithm, kind);
            event.fee_msat = fee_msat;
            event.error = error;
            self.telemetry.record(event);
        }
    }
    pub fn for_policy(
        &self,
        payer: &PublicKey,
        params: &RouteParameters,
        first_hops: Option<&[&ChannelDetails]>,
        inflight: InFlightHtlcs,
        policy: &RoutingPolicy,
    ) -> Result<Route, &'static str> {
        self.route(payer, params, first_hops, inflight, policy, None)
    }
    fn route(
        &self,
        payer: &PublicKey,
        original: &RouteParameters,
        first_hops: Option<&[&ChannelDetails]>,
        inflight: InFlightHtlcs,
        policy: &RoutingPolicy,
        id: Option<PaymentId>,
    ) -> Result<Route, &'static str> {
        let started = Instant::now();
        let result = (|| {
            policy.validate()?;
            let params = policy.constrain(original);
            if params.final_value_msat > 2_100_000_000_000_000_000
                || params.final_value_msat == 0
                || params.rgb_payment.is_some_and(|(_, r)| r == 0)
                || params.payment_params.max_path_count == 0
            {
                return Err("Invalid payment routing amounts or limits");
            }
            let mut budget = SearchBudget::new(policy);
            let mut scoring = ProbabilisticScoringFeeParameters::default();
            for node in &policy.excluded_nodes {
                let pk = node
                    .parse::<PublicKey>()
                    .map_err(|_| "Invalid excluded node")?;
                scoring.add_banned(&NodeId::from_pubkey(&pk));
            }
            let graph = if matches!(params.payment_params.payee, Payee::Clear { .. }) {
                let scorer = self.scorer.read().unwrap();
                Some(Graph::snapshot(
                    *payer,
                    &params,
                    first_hops,
                    &inflight,
                    &self.network,
                    &scorer,
                    &scoring,
                    policy,
                    &self.ldk_dir,
                    &mut budget,
                )?)
            } else {
                None
            };
            let mut route = match policy.algorithm {
                Algorithm::LdkDijkstra => DefaultRouter::new(
                    self.network.clone(),
                    self.logger.clone(),
                    self.keys.clone(),
                    self.scorer.clone(),
                    scoring,
                )
                .find_route(payer, &params, first_hops, inflight)?,
                _ => algorithms::plan(
                    graph
                        .as_ref()
                        .ok_or("Selected algorithm does not support blinded recipients")?,
                    &params,
                    policy,
                    &mut budget,
                )?,
            };
            if policy.algorithm != Algorithm::LdkDijkstra {
                // materialize() checks nonempty paths and the CLTV bound required by this helper.
                add_random_cltv_offset(
                    &mut route,
                    &params.payment_params,
                    &self.network.read_only(),
                    &self.keys.get_secure_random_bytes(),
                );
            }
            validation::validate(payer, &params, policy, &route, graph.as_ref())?;
            // LDK correlates the returned Route with its exact request, especially on retries.
            route.route_params = Some(original.clone());
            Ok(route)
        })();
        let mut event = RoutingEvent::new(
            id,
            policy.algorithm,
            if result.is_ok() {
                "route_planned"
            } else {
                "route_failed"
            },
        );
        event.elapsed_us = started.elapsed().as_micros();
        event.amount_msat = Some(original.final_value_msat);
        event.rgb_amount = original.rgb_payment.map(|(_, r)| r);
        match &result {
            Ok(route) => {
                event.paths = route.paths.len();
                event.fee_msat = Some(route.get_total_fees());
            }
            Err(error) => event.error = Some((*error).into()),
        }
        self.telemetry.record(event);
        result
    }
}
impl Router for AdaptiveRouter {
    fn find_route(
        &self,
        _payer: &PublicKey,
        _params: &RouteParameters,
        _first_hops: Option<&[&ChannelDetails]>,
        _inflight: InFlightHtlcs,
    ) -> Result<Route, &'static str> {
        Err("An explicit payment routing policy is required; use find_route_with_id")
    }
    fn find_route_with_id(
        &self,
        payer: &PublicKey,
        params: &RouteParameters,
        first_hops: Option<&[&ChannelDetails]>,
        inflight: InFlightHtlcs,
        _hash: PaymentHash,
        id: PaymentId,
    ) -> Result<Route, &'static str> {
        let policy = self.policies.get(id)?;
        self.route(payer, params, first_hops, inflight, &policy, Some(id))
    }
    fn create_blinded_payment_paths<T: secp256k1::Signing + secp256k1::Verification>(
        &self,
        recipient: PublicKey,
        first_hops: Vec<ChannelDetails>,
        tlvs: ReceiveTlvs,
        amount_msats: Option<u64>,
        secp_ctx: &Secp256k1<T>,
    ) -> Result<Vec<BlindedPaymentPath>, ()> {
        // Recipient hint construction does not select a payment planner.
        DefaultRouter::new(
            self.network.clone(),
            self.logger.clone(),
            self.keys.clone(),
            self.scorer.clone(),
            ProbabilisticScoringFeeParameters::default(),
        )
        .create_blinded_payment_paths(recipient, first_hops, tlvs, amount_msats, secp_ctx)
    }
}
