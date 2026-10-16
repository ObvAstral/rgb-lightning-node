use super::policy::Algorithm;
use lightning::ln::channelmanager::PaymentId;
use serde::Serialize;
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RoutingEvent {
    pub timestamp_ms: u128,
    pub payment_id: Option<String>,
    pub algorithm: Algorithm,
    pub kind: String,
    pub elapsed_us: u128,
    pub paths: usize,
    pub fee_msat: Option<u64>,
    pub amount_msat: Option<u64>,
    pub rgb_amount: Option<u64>,
    pub error: Option<String>,
}
impl RoutingEvent {
    pub fn new(id: Option<PaymentId>, algorithm: Algorithm, kind: &str) -> Self {
        Self {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            payment_id: id.map(|v| crate::utils::hex_str(&v.0)),
            algorithm,
            kind: kind.into(),
            elapsed_us: 0,
            paths: 0,
            fee_msat: None,
            amount_msat: None,
            rgb_amount: None,
            error: None,
        }
    }
}

pub(crate) struct Telemetry {
    inner: Mutex<(File, VecDeque<RoutingEvent>)>,
}
impl Telemetry {
    pub fn open(directory: &Path) -> io::Result<Self> {
        // The daemon's data directory also contains the existing payment history.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("routing-events.jsonl"))?;
        Ok(Self {
            inner: Mutex::new((file, VecDeque::new())),
        })
    }
    pub fn record(&self, event: RoutingEvent) {
        let mut inner = self.inner.lock().unwrap();
        let result = serde_json::to_writer(&mut inner.0, &event)
            .map_err(io::Error::other)
            .and_then(|_| inner.0.write_all(b"\n"));
        if let Err(e) = result {
            tracing::error!("Cannot persist routing telemetry: {e}");
        }
        if inner.1.len() == 1024 {
            inner.1.pop_front();
        }
        inner.1.push_back(event);
    }
    pub fn recent(&self) -> Vec<RoutingEvent> {
        self.inner.lock().unwrap().1.iter().cloned().collect()
    }
}
