//! Every planner, including LDK, passes the same checks before its Route is returned.
use super::{algorithms::graph::Graph, policy::RoutingPolicy};
use bitcoin::secp256k1::PublicKey;
use lightning::routing::router::{Payee, Route, RouteParameters, MAX_PATH_LENGTH_ESTIMATE};
use std::collections::HashSet;

pub(crate) fn validate(
    payer: &PublicKey,
    params: &RouteParameters,
    policy: &RoutingPolicy,
    route: &Route,
    graph: Option<&Graph>,
) -> Result<(), &'static str> {
    if route.route_params.as_ref() != Some(params)
        || params.final_value_msat == 0
        || route.paths.is_empty()
        || route.paths.len() > params.payment_params.max_path_count as usize
    {
        return Err("Invalid route parameters or path count");
    }
    if policy.excludes(payer) {
        return Err("Payer is excluded by routing policy");
    }
    let (payee, invoice_mpp, final_cltv) = match &params.payment_params.payee {
        Payee::Clear {
            node_id,
            features,
            final_cltv_expiry_delta,
            ..
        } => (
            Some(*node_id),
            features.as_ref().is_some_and(|f| f.supports_basic_mpp()),
            Some(*final_cltv_expiry_delta),
        ),
        Payee::Blinded { features, .. } => (
            None,
            features.as_ref().is_some_and(|f| f.supports_basic_mpp()),
            None,
        ),
    };
    if payee.is_none()
        && (!policy.excluded_nodes.is_empty()
            || !policy.excluded_channels.is_empty()
            || params.rgb_payment.is_some())
    {
        return Err("RGB and hard exclusions cannot be validated inside blinded paths");
    }
    if route.paths.len() > 1 && !graph.map_or(invoice_mpp, |g| g.supports_mpp) {
        return Err("Recipient does not support MPP");
    }
    let mut total_msat = 0u128;
    let mut total_rgb = 0u128;
    let mut total_fee = 0u128;
    let mut used_msat = graph.map(|g| vec![0u128; g.edges.len()]);
    let mut used_rgb = graph.map(|g| vec![0u128; g.edges.len()]);
    for path in &route.paths {
        if path.hops.is_empty()
            || path.hops.len()
                > params
                    .payment_params
                    .max_path_length
                    .min(MAX_PATH_LENGTH_ESTIMATE) as usize
        {
            return Err("Invalid route path length");
        }
        let value = path.final_value_msat();
        if value == 0 {
            return Err("Zero-value HTLC path");
        }
        total_msat += value as u128;
        let cltv: u64 = path.hops.iter().map(|h| h.cltv_expiry_delta as u64).sum();
        if cltv > params.payment_params.max_total_cltv_expiry_delta as u64 {
            return Err("Route exceeds CLTV budget");
        }
        if let Some(expected) = final_cltv {
            if path.blinded_tail.is_some()
                || path.hops.last().unwrap().pubkey != payee.unwrap()
                || path.hops.last().unwrap().cltv_expiry_delta < expected
            {
                return Err("Route has wrong recipient or final CLTV");
            }
        } else if path.blinded_tail.is_none() {
            return Err("Missing blinded tail");
        }
        let part_rgb = match (params.rgb_payment, path.hops.last().unwrap().rgb_payment) {
            (Some((expected, _)), Some((contract, amount)))
                if expected == contract && amount > 0 =>
            {
                amount
            }
            (None, None) => 0,
            _ => return Err("Invalid RGB contract or part amount"),
        };
        total_rgb += part_rgb as u128;
        let mut seen = HashSet::from([*payer]);
        for hop in &path.hops {
            if !seen.insert(hop.pubkey)
                || policy.excludes(&hop.pubkey)
                || policy.excluded_channels.contains(&hop.short_channel_id)
                || params
                    .payment_params
                    .previously_failed_channels
                    .contains(&hop.short_channel_id)
            {
                return Err("Route contains a cycle, excluded node or excluded/failed channel");
            }
            if hop.node_features.requires_unknown_bits()
                || hop.channel_features.requires_unknown_bits()
                || hop.payment_amount != value
                || hop.rgb_payment != params.rgb_payment.map(|(c, _)| (c, part_rgb))
            {
                return Err("Inconsistent route hop metadata");
            }
        }
        if path.blinded_tail.is_none() {
            total_fee += path.hops[..path.hops.len() - 1]
                .iter()
                .map(|h| h.fee_msat as u128)
                .sum::<u128>();
        } else {
            total_fee += path.hops.iter().map(|h| h.fee_msat as u128).sum::<u128>();
        }
        if let Some(g) = graph {
            let mut incoming = 0u128;
            for i in (0..path.hops.len()).rev() {
                let hop = &path.hops[i];
                incoming += hop.fee_msat as u128;
                let source = if i == 0 {
                    payer
                } else {
                    &path.hops[i - 1].pubkey
                };
                let edge_id = g
                    .edge(source, &hop.pubkey, hop.short_channel_id)
                    .ok_or("Route uses a channel absent from the usable snapshot")?;
                let edge = &g.edges[edge_id];
                if incoming < edge.htlc_min_msat as u128 || incoming > edge.htlc_max_msat as u128 {
                    return Err("Route violates an HTLC amount limit");
                }
                if i > 0
                    && (path.hops[i - 1].fee_msat
                        < edge.fee(u64::try_from(incoming).map_err(|_| "HTLC amount overflow")?)?
                        || path.hops[i - 1].cltv_expiry_delta < edge.cltv)
                {
                    return Err("Route underpays a forwarding fee or CLTV delta");
                }
                let msat = used_msat.as_mut().unwrap();
                msat[edge_id] += incoming;
                let rgb = used_rgb.as_mut().unwrap();
                rgb[edge_id] += part_rgb as u128;
                if msat[edge_id] > edge.capacity_msat as u128
                    || rgb[edge_id] > edge.capacity_rgb as u128
                {
                    return Err("Route exceeds shared BTC/RGB channel capacity");
                }
            }
        }
    }
    if total_msat != params.final_value_msat as u128
        || total_rgb != params.rgb_payment.map_or(0, |(_, amount)| amount as u128)
    {
        return Err("Route does not conserve requested BTC/RGB totals");
    }
    if total_fee > u64::MAX as u128
        || params
            .max_total_routing_fee_msat
            .is_some_and(|limit| total_fee > limit as u128)
    {
        return Err("Route exceeds remaining fee budget");
    }
    Ok(())
}
