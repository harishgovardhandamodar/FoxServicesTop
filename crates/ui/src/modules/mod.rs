//! The built-in display modules. Each renders exactly one core view type
//! from the immutable projection; a missing topic renders "no data" text.

mod cluster;
mod common;
mod cpu;
mod disk;
mod docker;
mod gpu;
mod history;
mod host_status;
mod memory;
mod network;
mod power;
mod process_memory;
mod processes;
mod settings;
mod tokens;
mod zombies;

use fox_services_top_core::identity::IdentityError;
use thiserror::Error;

pub use cluster::ClusterModule;
pub use cpu::{CPU_MODULE_ID, CpuModule};
pub use disk::DiskModule;
pub use docker::DockerModule;
pub use gpu::GpuModule;
pub use history::HISTORY_MODULE_ID;
pub use history::HistoryModule;
pub use memory::MEMORY_MODULE_ID;
pub use memory::MemoryModule;
pub use network::NetworkModule;
pub use power::PowerModule;
pub use processes::{PROCESSES_MODULE_ID, ProcessesModule};
pub use settings::SettingsModule;
pub use tokens::TokensModule;
pub use zombies::ZombiesModule;

use crate::registry::{DisplayModule, ManifestError};

/// A built-in module whose static manifest failed validation (a build defect).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModuleBuildError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

fn boxed<M: DisplayModule + 'static>(module: M) -> Box<dyn DisplayModule> {
    Box::new(module)
}

/// Constructs the built-ins in `config::BUILTIN_MODULES` order.
pub fn builtin_modules() -> Vec<Result<Box<dyn DisplayModule>, ModuleBuildError>> {
    vec![
        CpuModule::new().map(boxed),
        GpuModule::new().map(boxed),
        MemoryModule::new().map(boxed),
        DiskModule::new().map(boxed),
        NetworkModule::new().map(boxed),
        ProcessesModule::new().map(boxed),
        HistoryModule::new().map(boxed),
        SettingsModule::new().map(boxed),
        ClusterModule::new().map(boxed),
        DockerModule::new().map(boxed),
        TokensModule::new().map(boxed),
        PowerModule::new().map(boxed),
        ZombiesModule::new().map(boxed),
    ]
}
