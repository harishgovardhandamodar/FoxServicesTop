//! Fox-services cluster monitoring for the dashboard's cluster, service and token widgets.
//!
//! The composition root polls the statically configured nodes' HTTP APIs
//! (`/api/hardware/status`, `/api/llm/live-map`, `/api/usage/history`,
//! `/api/ollama/stats`, `/api/mesh/peers`) on a background thread
//! ([`ClusterMonitor`]) and hands the Manager a read-only [`ClusterSource`].
//! Networking lives only here: collectors, the Manager and the Viewer
//! never touch sockets.

mod client;
mod http;
mod monitor;
mod services;
mod tokens;

const U64_EXCLUSIVE_MAX_AS_F64: f64 = 18_446_744_073_709_551_616.0;

pub(super) fn nonnegative_f64_to_u64(value: f64) -> u64 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    if value >= U64_EXCLUSIVE_MAX_AS_F64 {
        return u64::MAX;
    }
    // The range check makes the cast safe; fractional API values intentionally truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value as u64
    }
}

pub(super) fn nonnegative_f64_to_u32(value: f64) -> u32 {
    u32::try_from(nonnegative_f64_to_u64(value)).unwrap_or(u32::MAX)
}

pub(super) fn nonnegative_f64_to_usize(value: f64) -> usize {
    usize::try_from(nonnegative_f64_to_u64(value)).unwrap_or(usize::MAX)
}

pub use client::{FoxServicesClient, NodePoll};
pub use http::{HttpError, get_json};
pub use monitor::ClusterMonitor;
pub use services::{NodeServices, ServicePoll, merge as merge_services};
pub use tokens::{NodeTokens, TokenPoll, merge as merge_tokens};

#[cfg(test)]
mod tests {
    use super::{nonnegative_f64_to_u32, nonnegative_f64_to_u64, nonnegative_f64_to_usize};

    #[test]
    fn nonnegative_float_conversions_clamp_and_truncate() {
        assert_eq!(nonnegative_f64_to_u64(-1.0), 0);
        assert_eq!(nonnegative_f64_to_u64(12.9), 12);
        assert_eq!(nonnegative_f64_to_u64(f64::INFINITY), u64::MAX);
        assert_eq!(nonnegative_f64_to_u64(f64::NAN), 0);
        assert_eq!(nonnegative_f64_to_u64(u64::MAX as f64), u64::MAX);
        assert_eq!(nonnegative_f64_to_u32(u32::MAX as f64 + 10.0), u32::MAX);
        assert_eq!(nonnegative_f64_to_usize(f64::MAX), usize::MAX);
    }
}
