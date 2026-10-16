//! Owned snapshot shared by planners and the final validator. Remote capacities are estimates.
use super::super::policy::RoutingPolicy;
use super::SearchBudget;
use crate::{
    ldk::{NetworkGraph, Scorer},
    rgb::get_rgb_channel_info_optional,
};
use bitcoin::secp256k1::PublicKey;
use lightning::{
    ln::channel_state::ChannelDetails,
    routing::{
        gossip::{NodeId, RoutingFees, UNKNOWN_CHANNEL_CAPACITY_MSAT},
        router::{CandidateRouteHop, InFlightHtlcs, Payee, PublicHopCandidate, RouteParameters},
        scoring::{ChannelUsage, ProbabilisticScoringFeeParameters, ScoreLookUp},
    },
    types::features::{ChannelFeatures, NodeFeatures},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Debug)]
pub(crate) struct Edge {
    pub source: usize,
    pub target: usize,
    pub scid: u64,
    pub capacity_msat: u64,
    pub htlc_max_msat: u64,
    pub htlc_min_msat: u64,
    pub capacity_rgb: u64,
    pub fees: RoutingFees,
    pub cltv: u32,
    /// LDK penalty in msat, not a measured RGB success probability.
    pub penalty: u64,
    pub features: ChannelFeatures,
    pub announced: bool,
}
impl Edge {
    pub fn fee(&self, amount: u64) -> Result<u64, &'static str> {
        let proportional = (amount as u128 * self.fees.proportional_millionths as u128) / 1_000_000;
        u64::try_from(proportional + self.fees.base_msat as u128)
            .map_err(|_| "Routing fee overflow")
    }
    pub fn cost(&self, amount: u64) -> u128 {
        self.fee(amount).unwrap_or(u64::MAX) as u128 + self.penalty as u128
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Graph {
    pub nodes: Vec<PublicKey>,
    pub features: Vec<NodeFeatures>,
    pub edges: Vec<Edge>,
    pub outgoing: Vec<Vec<usize>>,
    pub source: usize,
    pub target: usize,
    pub final_cltv: u32,
    pub supports_mpp: bool,
    index: HashMap<PublicKey, usize>,
}
impl Graph {
    pub fn new(payer: PublicKey, payee: PublicKey, final_cltv: u32, supports_mpp: bool) -> Self {
        let mut g = Self {
            nodes: vec![],
            features: vec![],
            edges: vec![],
            outgoing: vec![],
            source: 0,
            target: 0,
            final_cltv,
            supports_mpp,
            index: HashMap::new(),
        };
        g.source = g.node(payer, NodeFeatures::empty());
        g.target = g.node(payee, NodeFeatures::empty());
        g
    }
    pub fn node(&mut self, pk: PublicKey, features: NodeFeatures) -> usize {
        if let Some(i) = self.index.get(&pk) {
            return *i;
        }
        let i = self.nodes.len();
        self.nodes.push(pk);
        self.features.push(features);
        self.outgoing.push(vec![]);
        self.index.insert(pk, i);
        i
    }
    pub fn add(&mut self, edge: Edge) {
        if edge.capacity_msat == 0 || edge.source == edge.target {
            return;
        }
        if self.outgoing[edge.source]
            .iter()
            .any(|i| self.edges[*i].target == edge.target && self.edges[*i].scid == edge.scid)
        {
            return;
        }
        self.outgoing[edge.source].push(self.edges.len());
        self.edges.push(edge);
    }
    pub fn edge(&self, source: &PublicKey, target: &PublicKey, scid: u64) -> Option<usize> {
        let s = *self.index.get(source)?;
        let t = *self.index.get(target)?;
        self.outgoing[s]
            .iter()
            .copied()
            .find(|i| self.edges[*i].target == t && self.edges[*i].scid == scid)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn snapshot(
        payer: PublicKey,
        params: &RouteParameters,
        first_hops: Option<&[&ChannelDetails]>,
        inflight: &InFlightHtlcs,
        network: &NetworkGraph,
        scorer: &Scorer,
        scoring: &ProbabilisticScoringFeeParameters,
        policy: &RoutingPolicy,
        ldk_dir: &Path,
        budget: &mut SearchBudget,
    ) -> Result<Self, &'static str> {
        let (payee, hints, invoice_features, final_cltv) = match &params.payment_params.payee {
            Payee::Clear { node_id, route_hints, features, final_cltv_expiry_delta } => (*node_id, route_hints, features, *final_cltv_expiry_delta),
            Payee::Blinded { .. } => return Err("This planner only supports clear recipients; explicitly select ldk_dijkstra for blinded payments"),
        };
        if payer == payee || policy.excludes(&payer) || policy.excludes(&payee) {
            return Err("Invalid or excluded payment endpoints");
        }
        let excluded: HashSet<PublicKey> = policy
            .excluded_nodes
            .iter()
            .filter_map(|v| v.parse().ok())
            .collect();
        let net = network.read_only();
        let payee_id = NodeId::from_pubkey(&payee);
        let mpp = invoice_features
            .as_ref()
            .is_some_and(|f| f.supports_basic_mpp())
            || net
                .nodes()
                .get(&payee_id)
                .and_then(|n| n.announcement_info.as_ref())
                .is_some_and(|i| i.features().supports_basic_mpp());
        let mut g = Self::new(payer, payee, final_cltv, mpp);
        for (id, info) in net.nodes().unordered_iter() {
            budget.step()?;
            let Ok(pk) = id.as_pubkey() else { continue };
            let features = info
                .announcement_info
                .as_ref()
                .map(|i| i.features().clone())
                .unwrap_or_else(NodeFeatures::empty);
            let idx = g.node(pk, features.clone());
            g.features[idx] = features;
        }
        if let Some(hops) = first_hops {
            for hop in hops {
                budget.step()?;
                let features = hop.counterparty.features.to_context();
                let index = g.node(hop.counterparty.node_id, features.clone());
                g.features[index] = features;
            }
        }
        if let Some(features) = invoice_features {
            g.features[g.target] = features.to_context();
        }
        let mut add = |source: PublicKey,
                       target: PublicKey,
                       scid: u64,
                       capacity: u64,
                       max: u64,
                       min: u64,
                       rgb: u64,
                       fees: RoutingFees,
                       cltv: u32,
                       penalty: u64,
                       features: ChannelFeatures,
                       announced: bool,
                       local: bool| {
            if excluded.contains(&source)
                || excluded.contains(&target)
                || params
                    .payment_params
                    .previously_failed_channels
                    .contains(&scid)
                || features.requires_unknown_bits()
                || penalty == u64::MAX
                || (target == payer)
                || (source == payer && first_hops.is_some() && !local)
            {
                return;
            }
            if params.rgb_payment.is_some() && rgb == 0 {
                return;
            }
            let s = g.node(source, NodeFeatures::empty());
            let t = g.node(target, NodeFeatures::empty());
            if g.features[s].requires_unknown_bits() || g.features[t].requires_unknown_bits() {
                return;
            }
            let used = if local {
                0
            } else {
                inflight
                    .used_liquidity_msat(
                        &NodeId::from_pubkey(&source),
                        &NodeId::from_pubkey(&target),
                        scid,
                    )
                    .unwrap_or(0)
            };
            let used_rgb = if local {
                0
            } else {
                inflight
                    .used_liquidity_rgb(
                        &NodeId::from_pubkey(&source),
                        &NodeId::from_pubkey(&target),
                        scid,
                    )
                    .unwrap_or(0)
            };
            g.add(Edge {
                source: s,
                target: t,
                scid,
                capacity_msat: capacity.saturating_sub(used),
                htlc_max_msat: max,
                htlc_min_msat: min,
                capacity_rgb: rgb.saturating_sub(used_rgb),
                fees: if source == payer {
                    RoutingFees {
                        base_msat: 0,
                        proportional_millionths: 0,
                    }
                } else {
                    fees
                },
                cltv: if source == payer { 0 } else { cltv },
                penalty,
                features,
                announced,
            });
        };
        if let Some(hops) = first_hops {
            for hop in hops {
                budget.step()?;
                if !hop.is_usable || hop.counterparty.features.requires_unknown_bits() {
                    continue;
                }
                if [
                    hop.short_channel_id,
                    hop.outbound_scid_alias,
                    hop.inbound_scid_alias,
                ]
                .into_iter()
                .flatten()
                .any(|scid| {
                    params
                        .payment_params
                        .previously_failed_channels
                        .contains(&scid)
                }) {
                    continue;
                }

                if let Some((contract, _)) = params.rgb_payment {
                    if !get_rgb_channel_info_optional(&hop.channel_id, ldk_dir, false)
                        .is_some_and(|(i, _)| i.contract_id == contract)
                    {
                        continue;
                    }
                }
                let Some(scid) = hop.get_outbound_payment_scid() else {
                    continue;
                };
                add(
                    payer,
                    hop.counterparty.node_id,
                    scid,
                    hop.outbound_capacity_msat
                        .min(hop.next_outbound_htlc_limit_msat),
                    hop.next_outbound_htlc_limit_msat,
                    hop.next_outbound_htlc_minimum_msat,
                    hop.next_outbound_htlc_limit_rgb,
                    RoutingFees {
                        base_msat: 0,
                        proportional_millionths: 0,
                    },
                    0,
                    0,
                    ChannelFeatures::empty(),
                    hop.is_announced,
                    true,
                );
            }
        }
        for (scid, channel) in net.channels().unordered_iter() {
            budget.step()?;
            if params
                .rgb_payment
                .is_some_and(|(cid, _)| channel.contract_id != Some(cid))
            {
                continue;
            }
            for (source, target, direction) in [
                (channel.node_one, channel.node_two, &channel.one_to_two),
                (channel.node_two, channel.node_one, &channel.two_to_one),
            ] {
                let Some(d) = direction.as_ref().filter(|d| d.enabled) else {
                    continue;
                };
                let (Ok(source_pk), Ok(target_pk)) = (source.as_pubkey(), target.as_pubkey())
                else {
                    continue;
                };
                let capacity = channel
                    .capacity_sats
                    .and_then(|v| v.checked_mul(1000))
                    .unwrap_or(d.htlc_maximum_msat)
                    .min(d.htlc_maximum_msat);
                let penalty = channel
                    .as_directed_from(&source)
                    .map(|(info, _)| {
                        let effective_capacity = info.effective_capacity();
                        scorer.channel_penalty_msat(
                            &CandidateRouteHop::PublicHop(PublicHopCandidate {
                                info,
                                short_channel_id: *scid,
                            }),
                            ChannelUsage {
                                amount_msat: params.final_value_msat.min(capacity),
                                inflight_htlc_msat: inflight
                                    .used_liquidity_msat(&source, &target, *scid)
                                    .unwrap_or(0),
                                effective_capacity,
                            },
                            scoring,
                        )
                    })
                    .unwrap_or(10_000);
                add(
                    source_pk,
                    target_pk,
                    *scid,
                    capacity,
                    d.htlc_maximum_msat,
                    d.htlc_minimum_msat,
                    d.htlc_maximum_rgb,
                    d.fees,
                    d.cltv_expiry_delta as u32,
                    penalty,
                    channel.features.clone(),
                    true,
                    false,
                );
            }
        }
        for hint in hints {
            budget.step()?;
            for (i, hop) in hint.0.iter().enumerate() {
                budget.step()?;
                let target = hint.0.get(i + 1).map_or(payee, |next| next.src_node_id);
                let known = net.channels().get(&hop.short_channel_id).filter(|c| {
                    let source = NodeId::from_pubkey(&hop.src_node_id);
                    let target = NodeId::from_pubkey(&target);
                    (c.node_one == source && c.node_two == target)
                        || (c.node_two == source && c.node_one == target)
                });
                if params
                    .rgb_payment
                    .is_some_and(|(cid, _)| known.is_some_and(|c| c.contract_id != Some(cid)))
                {
                    continue;
                }
                let max = hop
                    .htlc_maximum_msat
                    .unwrap_or(UNKNOWN_CHANNEL_CAPACITY_MSAT);
                let rgb = hop.htlc_maximum_rgb.unwrap_or_else(|| {
                    known
                        .and_then(|c| {
                            let source = NodeId::from_pubkey(&hop.src_node_id);
                            if c.node_one == source {
                                c.one_to_two.as_ref()
                            } else if c.node_two == source {
                                c.two_to_one.as_ref()
                            } else {
                                None
                            }
                        })
                        .map_or(0, |d| d.htlc_maximum_rgb)
                });
                add(
                    hop.src_node_id,
                    target,
                    hop.short_channel_id,
                    max,
                    max,
                    hop.htlc_minimum_msat.unwrap_or(0),
                    rgb,
                    hop.fees,
                    hop.cltv_expiry_delta as u32,
                    10_000,
                    ChannelFeatures::empty(),
                    known.is_some(),
                    false,
                );
            }
        }
        // Stable tie breaking makes experiments reproducible across graph hash seeds.
        g.edges
            .sort_by_key(|e| (g.nodes[e.source], g.nodes[e.target], e.scid));
        for list in &mut g.outgoing {
            list.clear();
        }
        for (i, edge) in g.edges.iter().enumerate() {
            g.outgoing[edge.source].push(i);
        }
        Ok(g)
    }
}
