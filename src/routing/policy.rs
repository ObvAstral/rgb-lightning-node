use bitcoin::secp256k1::PublicKey;
use lightning::ln::channelmanager::PaymentId;
use lightning::routing::router::{RouteParameters, RouteParametersConfig};
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    str::FromStr,
    sync::{Arc, Mutex},
};

/// Deliberately has no Default implementation or serde default.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Algorithm {
    LdkDijkstra,
    Yen,
    MinCostFlow,
    MultiObjective,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectiveWeights {
    pub fee: u32,
    pub reliability: u32,
    pub cltv: u32,
    pub hops: u32,
}

/// Algorithm is mandatory; defaults below only bound the execution of that algorithm.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoutingPolicy {
    pub algorithm: Algorithm,
    #[serde(default)]
    pub max_fee_msat: Option<u64>,
    #[serde(default)]
    pub max_paths: Option<u8>,
    #[serde(default)]
    pub max_cltv_delta: Option<u32>,
    #[serde(default)]
    pub excluded_nodes: Vec<String>,
    #[serde(default)]
    pub excluded_channels: Vec<u64>,
    #[serde(default = "candidates")]
    pub max_candidates: usize,
    #[serde(default = "expansions")]
    pub max_expansions: usize,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub weights: Option<ObjectiveWeights>,
}
fn candidates() -> usize {
    32
}
fn expansions() -> usize {
    100_000
}
fn timeout() -> u64 {
    1_000
}

impl RoutingPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_paths.is_some_and(|v| v == 0 || v > 32) {
            return Err("routing.max_paths must be between 1 and 32");
        }
        if self.max_cltv_delta.is_some_and(|v| v == 0 || v > 2016) {
            return Err("routing.max_cltv_delta must be between 1 and 2016");
        }
        if !(1..=256).contains(&self.max_candidates)
            || !(1..=2_000_000).contains(&self.max_expansions)
            || !(1..=10_000).contains(&self.timeout_ms)
        {
            return Err("routing search limits are out of range");
        }
        if self.excluded_nodes.len() > 256
            || self.excluded_channels.len() > 256
            || self
                .excluded_nodes
                .iter()
                .any(|pk| pk.len() != 66 || PublicKey::from_str(pk).is_err())
        {
            return Err("invalid routing exclusions");
        }
        match (&self.algorithm, &self.weights) {
            (Algorithm::MultiObjective, Some(w))
                if [w.fee, w.reliability, w.cltv, w.hops]
                    .iter()
                    .any(|v| *v > 0)
                    && [w.fee, w.reliability, w.cltv, w.hops]
                        .iter()
                        .all(|v| *v <= 1_000_000) => {}
            (Algorithm::MultiObjective, _) => {
                return Err("multi_objective requires nonzero objective weights (each <= 1000000)")
            }
            (_, Some(_)) => return Err("weights are only supported by multi_objective"),
            _ => {}
        }
        Ok(())
    }

    pub fn config(&self) -> RouteParametersConfig {
        let mut config = RouteParametersConfig::default();
        config.max_total_routing_fee_msat = self.max_fee_msat;
        if let Some(v) = self.max_paths {
            config.max_path_count = v;
        }
        if let Some(v) = self.max_cltv_delta {
            config.max_total_cltv_expiry_delta = v;
        }
        config
    }

    /// Tighten the caller's limits, including the remaining fee budget on a retry.
    pub fn constrain(&self, params: &RouteParameters) -> RouteParameters {
        let mut result = params.clone();
        if let Some(limit) = self.max_fee_msat {
            result.max_total_routing_fee_msat = Some(
                result
                    .max_total_routing_fee_msat
                    .map_or(limit, |v| v.min(limit)),
            );
        }
        if let Some(v) = self.max_paths {
            result.payment_params.max_path_count = result.payment_params.max_path_count.min(v);
        }
        if let Some(v) = self.max_cltv_delta {
            result.payment_params.max_total_cltv_expiry_delta =
                result.payment_params.max_total_cltv_expiry_delta.min(v);
        }
        result
            .payment_params
            .previously_failed_channels
            .extend(self.excluded_channels.iter().copied());
        result
    }

    pub fn excludes(&self, node: &PublicKey) -> bool {
        self.excluded_nodes
            .iter()
            .any(|v| PublicKey::from_str(v).as_ref() == Ok(node))
    }
}

/// Persisted before submission to LDK. An existing payment's policy is immutable.
pub(crate) struct PolicyStore {
    store: Arc<FilesystemStore>,
    cache: Mutex<HashMap<PaymentId, RoutingPolicy>>,
}
impl PolicyStore {
    pub fn new(store: Arc<FilesystemStore>) -> Self {
        Self {
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, id: PaymentId) -> Result<RoutingPolicy, &'static str> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(policy) = cache.get(&id) {
            return Ok(policy.clone());
        }
        let bytes = self
            .store
            .read("routing", "policies", &crate::utils::hex_str(&id.0))
            .map_err(|_| "Explicit routing policy missing or unreadable for payment")?;
        let policy: RoutingPolicy =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid persisted routing policy")?;
        policy.validate()?;
        if cache.len() >= 1024 && !cache.contains_key(&id) {
            if let Some(oldest) = cache.keys().next().copied() {
                cache.remove(&oldest);
            }
        }
        cache.insert(id, policy.clone());
        Ok(policy)
    }

    pub fn register(&self, id: PaymentId, policy: &RoutingPolicy) -> Result<(), io::Error> {
        policy.validate().map_err(io::Error::other)?;
        let mut cache = self.cache.lock().unwrap();
        let key = crate::utils::hex_str(&id.0);
        match self.store.read("routing", "policies", &key) {
            Ok(bytes) => {
                let existing: RoutingPolicy =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if existing != *policy {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Routing policy is already bound to this payment",
                    ));
                }
            }
            Err(e) if e.kind() == bitcoin::io::ErrorKind::NotFound => {
                self.store.write(
                    "routing",
                    "policies",
                    &key,
                    serde_json::to_vec(policy).map_err(io::Error::other)?,
                )?;
            }
            Err(e) => return Err(e.into()),
        }
        if cache.len() >= 1024 && !cache.contains_key(&id) {
            if let Some(oldest) = cache.keys().next().copied() {
                cache.remove(&oldest);
            }
        }
        cache.insert(id, policy.clone());
        Ok(())
    }
}
