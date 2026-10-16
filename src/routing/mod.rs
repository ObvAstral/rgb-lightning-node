//! Explicit, per-payment routing. No planner is selected implicitly.
pub(crate) mod adaptive_router;
pub(crate) mod algorithms;
pub(crate) mod policy;
pub(crate) mod telemetry;
#[cfg(test)]
mod tests;
pub(crate) mod validation;
