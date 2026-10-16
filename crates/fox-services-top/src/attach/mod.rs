//! Attach-over-IPC between a headless collector (`--daemon`) and a viewer.
//!
//! The collector runs the ordinary pipeline (collection, arbitration,
//! persistence) with no terminal, pre-opens one session and serves it to
//! one viewer at a time over a Unix socket in the state directory. A
//! `fox-services-top` started without `--daemon` (and without `--no-attach`)
//! connects to that socket when one is present and runs the terminal UI
//! against a [`client::RemoteSession`] — a [`SessionPort`] whose projections
//! come from the collector and whose commands it forwards. The collector
//! keeps collecting whether or not a viewer is attached, so history
//! accumulates across viewer sessions.
//!
//! [`SessionPort`]: fox_services_top_core::ports::SessionPort
//! [`client::RemoteSession`]: client::RemoteSession

pub(crate) mod client;
pub(crate) mod protocol;
pub(crate) mod server;
