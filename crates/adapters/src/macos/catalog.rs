//! macOS catalog: entities and sources across the seven `macos` groups.
//!
//! The shapes mirror the Linux adapters so the collectors need no
//! platform split: Mach tick counters like `/proc/stat` columns (the four
//! Linux-only states as constant zero counters — this kernel has no
//! iowait/irq/softirq/steal), KiB memory gauges like `/proc/meminfo`,
//! one `process.cpu.ticks` counter per `ProcessKey` with `proc.*` tags,
//! sysinfo network/filesystem counters, and IOKit Apple GPU gauges.

use fox_services_top_core::digest::sha256_hex;
use fox_services_top_core::identity::{EntityId, HostId, Pid, PidNamespace, ProcessKey, Scope};
use fox_services_top_core::metric::{ids, native};
use fox_services_top_core::naming::{self, adapters, attributes, entity, groups};
use fox_services_top_core::observation::{Attribute, Capability, EntityKind};
use fox_services_top_core::process_stat::ProcessStat;
use fox_services_top_core::quality::{Availability, RawKind, Support};
use fox_services_top_core::text::{sanitize_display, truncate_utf8};

use super::iokit::AppleGpu;
use super::mach_cpu::CpuTimes;
use crate::procfs::catalog::{
    CatalogBook, CatalogConfig, EntityParts, KeyContext, SnapshotBuilder, SourceSpec, source_draft,
};
use crate::procfs::mechanics::invariant;
use crate::registry::{AdapterOptions, compile_time_adapter_id};

/// Profile of the native macOS telemetry.
pub(crate) const PROFILE_ID: &str = "macos-sysctl-mach-iokit";
pub(crate) const PROFILE_REVISION: u32 = 1;
pub(crate) const IMPLEMENTATION: &str = "MacosAdapter";
pub(crate) const MINIMUM_SAMPLE_MS: u32 = 1000;
/// Scheduler ticks per second, matching the Linux convention the
/// collectors and the `clk_tck` host attribute assume.
pub(crate) const CLK_TCK: u64 = 100;
const KIB: u64 = 1024;
const NET_NAMESPACE: &str = "macos";
const PID_NAMESPACE: &str = "macos";
const DRIVER: &str = "macos-native";
/// Entity attribute marking the loopback interface, like the Linux adapter.
pub(crate) const LOOPBACK_ATTRIBUTE: &str = "loopback";
const FAMILY_HOST: &str = "host";
const FAMILY_CPU: &str = "cpu";
const FAMILY_MEMORY: &str = "memory";
const FAMILY_PROCESS: &str = "process";
const FAMILY_INTERFACE: &str = "interface";
const FAMILY_MOUNT: &str = "mount";
const FAMILY_GPU: &str = "gpu";

/// Reason codes for telemetry this system cannot provide. Every one is a
/// deliberate omission, never a silent zero.
pub(crate) const FREQ_UNAVAILABLE: &str = "APPLE_FREQ_UNAVAILABLE";
pub(crate) const TEMP_UNAVAILABLE: &str = "APPLE_TEMP_REQUIRES_SMC";
pub(crate) const POWER_UNAVAILABLE: &str = "APPLE_POWER_REQUIRES_ROOT";
pub(crate) const GPU_TEMP_UNAVAILABLE: &str = "APPLE_GPU_SAMPLER_REQUIRES_ROOT";
pub(crate) const GPU_POWER_UNAVAILABLE: &str = "APPLE_GPU_SAMPLER_REQUIRES_ROOT";
pub(crate) const GPU_SERVICE_ABSENT: &str = "APPLE_GPU_SERVICE_ABSENT";
pub(crate) const MACH_TIMES_UNAVAILABLE: &str = "APPLE_MACH_TIMES_UNAVAILABLE";
pub(crate) const DROPS_ABSENT: &str = "APPLE_DROPS_ABSENT";

/// Static host facts, read once per adapter lifetime.
#[derive(Debug, Clone)]
pub(crate) struct MacosFacts {
    pub hostname: Option<String>,
    pub cpu_model: String,
    pub cpu_count: u32,
    pub mem_total_bytes: u64,
    pub boot_unix: u64,
}

/// Memory figures in bytes (sysinfo already applies Apple's
/// free+inactive+purgeable−compressor availability formula).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MemoryData {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// One process as the `process` group publishes it.
#[derive(Debug, Clone)]
pub(crate) struct MacosProcess {
    pub pid: i32,
    pub ppid: Option<i32>,
    pub name: String,
    pub command: String,
    pub uid: Option<u32>,
    pub state: char,
    pub rss_bytes: Option<u64>,
    pub start_ticks: u64,
}

/// One network interface as the `net` group publishes it (kernel
/// cumulative counters; drops have no macOS counter and stay absent).
#[derive(Debug, Clone)]
pub(crate) struct MacosNet {
    pub ifname: String,
    pub ifid: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub loopback: bool,
}

/// One mounted filesystem as the `capacity` group publishes it.
#[derive(Debug, Clone)]
pub(crate) struct MacosMount {
    pub mount_point: String,
    pub mount_id: u64,
    pub fs_type: String,
    pub device: String,
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

/// How one catalogued source is read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MacosLocator {
    CpuTime {
        core: Option<u32>,
        field: CpuField,
    },
    Load(LoadField),
    Memory(MemField),
    Process {
        pid: i32,
        start_ticks: u64,
    },
    Net {
        ifname: String,
        ifid: u32,
        counter: NetCounter,
    },
    Fs {
        mount_id: u64,
        figure: FsFigure,
    },
    GpuUtil,
    GpuMem(GpuMemField),
    GpuCores,
}

/// One Mach tick column; the order matches `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuField {
    User,
    Nice,
    System,
    Idle,
    IoWait,
    Irq,
    SoftIrq,
    Steal,
}

pub(crate) const CPU_FIELDS: [CpuField; 8] = [
    CpuField::User,
    CpuField::Nice,
    CpuField::System,
    CpuField::Idle,
    CpuField::IoWait,
    CpuField::Irq,
    CpuField::SoftIrq,
    CpuField::Steal,
];

impl CpuField {
    fn channel(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Nice => "nice",
            Self::System => "system",
            Self::Idle => "idle",
            Self::IoWait => "iowait",
            Self::Irq => "irq",
            Self::SoftIrq => "softirq",
            Self::Steal => "steal",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::User => ids::CPU_TIME_USER,
            Self::Nice => ids::CPU_TIME_NICE,
            Self::System => ids::CPU_TIME_SYSTEM,
            Self::Idle => ids::CPU_TIME_IDLE,
            Self::IoWait => ids::CPU_TIME_IOWAIT,
            Self::Irq => ids::CPU_TIME_IRQ,
            Self::SoftIrq => ids::CPU_TIME_SOFTIRQ,
            Self::Steal => ids::CPU_TIME_STEAL,
        }
    }
}

/// A `host`-entity gauge of the `cpu` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadField {
    One,
    Five,
    Fifteen,
    Running,
    Total,
}

impl LoadField {
    fn channel(self) -> &'static str {
        match self {
            Self::One => "1m",
            Self::Five => "5m",
            Self::Fifteen => "15m",
            Self::Running => "running",
            Self::Total => "total",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::One => ids::CPU_LOAD_1M,
            Self::Five => ids::CPU_LOAD_5M,
            Self::Fifteen => ids::CPU_LOAD_15M,
            Self::Running => ids::CPU_TASKS_RUNNING,
            Self::Total => ids::CPU_TASKS_TOTAL,
        }
    }
}

/// A `memory`-entity gauge of the `memory` group (KiB, like meminfo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemField {
    Total,
    Available,
    SwapTotal,
    SwapFree,
}

impl MemField {
    fn channel(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Available => "available",
            Self::SwapTotal => "swap_total",
            Self::SwapFree => "swap_free",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::Total => ids::MEMORY_TOTAL,
            Self::Available => ids::MEMORY_AVAILABLE,
            Self::SwapTotal => ids::MEMORY_SWAP_TOTAL,
            Self::SwapFree => ids::MEMORY_SWAP_FREE_RAW,
        }
    }
}

/// One `net`-group counter (drops are absent on macOS by design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetCounter {
    RxBytes,
    TxBytes,
    RxPackets,
    TxPackets,
    RxErrors,
    TxErrors,
}

pub(crate) const NET_COUNTERS: [NetCounter; 6] = [
    NetCounter::RxBytes,
    NetCounter::TxBytes,
    NetCounter::RxPackets,
    NetCounter::TxPackets,
    NetCounter::RxErrors,
    NetCounter::TxErrors,
];

impl NetCounter {
    fn channel(self) -> &'static str {
        match self {
            Self::RxBytes => "rx_bytes",
            Self::TxBytes => "tx_bytes",
            Self::RxPackets => "rx_packets",
            Self::TxPackets => "tx_packets",
            Self::RxErrors => "rx_errors",
            Self::TxErrors => "tx_errors",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::RxBytes => ids::NET_RX_BYTES,
            Self::TxBytes => ids::NET_TX_BYTES,
            Self::RxPackets => ids::NET_RX_PACKETS,
            Self::TxPackets => ids::NET_TX_PACKETS,
            Self::RxErrors => ids::NET_RX_ERRORS,
            Self::TxErrors => ids::NET_TX_ERRORS,
        }
    }

    fn native_unit(self) -> &'static str {
        match self {
            Self::RxBytes | Self::TxBytes => native::BYTES,
            _ => native::COUNT,
        }
    }
}

/// One `capacity`-group gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsFigure {
    Total,
    Free,
    Available,
}

pub(crate) const FS_FIGURES: [FsFigure; 3] = [FsFigure::Total, FsFigure::Free, FsFigure::Available];

impl FsFigure {
    fn channel(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Free => "free",
            Self::Available => "available",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::Total => ids::FS_TOTAL,
            Self::Free => ids::FS_FREE,
            Self::Available => ids::FS_AVAILABLE,
        }
    }
}

/// One unified-memory gauge of the `gpu_fast` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuMemField {
    Used,
    Free,
    Total,
}

pub(crate) const GPU_MEM_FIELDS: [GpuMemField; 3] =
    [GpuMemField::Used, GpuMemField::Free, GpuMemField::Total];

impl GpuMemField {
    fn channel(self) -> &'static str {
        match self {
            Self::Used => "fb_used",
            Self::Free => "fb_free",
            Self::Total => "fb_total",
        }
    }

    fn metric_id(self) -> &'static str {
        match self {
            Self::Used => ids::GPU_MEMORY_FB_USED,
            Self::Free => ids::GPU_MEMORY_FB_FREE,
            Self::Total => ids::GPU_MEMORY_FB_TOTAL,
        }
    }
}

/// Stable 32-bit interface id from the interface name (no `if_nametoindex`
/// dependency; the id only needs stability within and across boots).
pub(crate) fn interface_id(ifname: &str) -> u32 {
    let digest = sha256_hex(ifname.as_bytes());
    u32::from_be_bytes(digest.as_bytes()[..4].try_into().unwrap_or([0; 4]))
}

/// Stable 64-bit mount id from the mount point.
pub(crate) fn mount_id(mount_point: &str) -> u64 {
    let digest = sha256_hex(mount_point.as_bytes());
    u64::from_be_bytes(digest.as_bytes()[..8].try_into().unwrap_or([0; 8]))
}

/// Entity id of the Apple GPU (`gpu:APPLE-M2-MAX` style); the model slug is
/// already sanitized by the caller.
pub(crate) fn gpu_entity_id(slug: &str) -> Option<EntityId> {
    entity::gpu(&format!("APPLE-{slug}")).ok()
}

/// URL-safe model slug for the GPU entity (`Apple M2 Max` → `M2-MAX`).
pub(crate) fn gpu_model_slug(model: &str) -> String {
    let slug: String = model
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "GPU".to_owned()
    } else {
        truncate_utf8(&slug, 48).to_owned()
    }
}

/// `ProcessKey` of a macOS process; `None` when the pid or start time is
/// out of range.
pub(crate) fn process_key(
    host_id: &HostId,
    boot_id: &fox_services_top_core::identity::BootId,
    pid: i32,
    start_ticks: u64,
) -> Option<ProcessKey> {
    Some(ProcessKey {
        host_id: host_id.clone(),
        boot_id: boot_id.clone(),
        pid_namespace: PidNamespace::new(PID_NAMESPACE).ok()?,
        pid: Pid::new(i64::from(pid)).ok()?,
        start_time_ticks: start_ticks,
    })
}

/// Channel of a process record: display name plus command line, bounded to
/// the observation schema like the procfs adapter's channel.
pub(crate) fn process_channel(name: &str, command: &str) -> String {
    let clean_name = sanitize_display(name, 64);
    let clean_command = sanitize_display(command, 160);
    naming::process_channel(&clean_name, &clean_command)
}

/// `proc.*` tags of a process observation.
pub(crate) fn process_tags(process: &MacosProcess) -> Vec<String> {
    ProcessStat {
        rss_bytes: process.rss_bytes,
        uid: process.uid,
        ppid: process.ppid,
        state: Some(process.state),
        reaped_ticks: None,
    }
    .reason_codes()
}

fn attribute(key: &str, value: &str) -> Attribute {
    Attribute {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn host_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_HOST), entity::SCOPE_HOST)
}

fn cpu_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_HOST), entity::SCOPE_HOST)
}

fn core_scope(core: u32) -> Scope {
    invariant(Scope::new(format!("core:{core}")), "core scope")
}

fn system_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_SYSTEM), entity::SCOPE_SYSTEM)
}

fn process_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_PROCESS), entity::SCOPE_PROCESS)
}

fn interface_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_INTERFACE), entity::SCOPE_INTERFACE)
}

fn filesystem_scope() -> Scope {
    invariant(
        Scope::new(entity::SCOPE_FILESYSTEM),
        entity::SCOPE_FILESYSTEM,
    )
}

fn gpu_scope() -> Scope {
    invariant(Scope::new(entity::SCOPE_GPU), entity::SCOPE_GPU)
}

/// Fresh catalog book for the adapter.
pub(crate) fn new_book(options: &AdapterOptions) -> CatalogBook<MacosLocator> {
    let config = CatalogConfig {
        adapter_id: compile_time_adapter_id(adapters::MACOS),
        profile_id: invariant(
            fox_services_top_core::identity::ProfileId::new(PROFILE_ID),
            PROFILE_ID,
        ),
        profile_revision: PROFILE_REVISION,
        source_limit: usize::try_from(options.source_limit()).unwrap_or(usize::MAX),
        page_size: options.effective_page_size(),
    };
    CatalogBook::new(config)
}

/// Everything discovery publishes; reads re-gather their own subsystem.
pub(crate) struct DiscoveryData {
    pub facts: MacosFacts,
    pub cpu: Option<(CpuTimes, Vec<CpuTimes>)>,
    pub memory: MemoryData,
    pub load: (f64, f64, f64),
    pub tasks: (u64, u64),
    pub processes: Vec<MacosProcess>,
    pub interfaces: Vec<MacosNet>,
    pub mounts: Vec<MacosMount>,
    pub gpu: Option<AppleGpu>,
}

/// Adds every entity of a discovery snapshot; returns the availability.
pub(crate) fn discover_all(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    data: &DiscoveryData,
) -> Availability {
    let mut availability = Availability::Available;
    add_host(builder, keys, &data.facts, &data.load, &data.tasks);
    match data.cpu.as_ref() {
        Some(cpu) => add_cpu(builder, keys, &data.facts, cpu),
        None => {
            builder.carry_over_family(FAMILY_CPU);
            builder.error(MACH_TIMES_UNAVAILABLE);
            availability = Availability::Unavailable;
        }
    }
    add_memory(builder, keys, &data.memory);
    add_processes(builder, keys, data);
    add_network(builder, keys, &data.interfaces);
    add_filesystems(builder, keys, &data.mounts);
    match data.gpu.as_ref() {
        Some(gpu) => add_gpu(builder, keys, gpu),
        None => builder.error(GPU_SERVICE_ABSENT),
    }
    add_capabilities(builder, data.cpu.is_some(), data.gpu.is_some());
    availability
}

fn add_host(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    facts: &MacosFacts,
    load: &(f64, f64, f64),
    tasks: &(u64, u64),
) {
    let entity_id = invariant(EntityId::new(entity::HOST), entity::HOST);
    let Some(incarnation) = builder.incarnation(&entity_id, entity::HOST) else {
        return;
    };
    let scope = host_scope();
    let values = [
        (LoadField::One, load.0),
        (LoadField::Five, load.1),
        (LoadField::Fifteen, load.2),
    ];
    let mut sources = Vec::new();
    for (field, value) in values {
        if !value.is_finite() {
            continue;
        }
        let origin = format!("macos:sysctl:loadavg:{}", field.channel());
        let spec = SourceSpec {
            device_id: "loadavg",
            channel: field.channel(),
            entity: &entity_id,
            scope: &scope,
            metric_id: field.metric_id(),
            group: groups::CPU,
            native_unit: native::COUNT,
            raw_kind: RawKind::Gauge,
            driver: DRIVER,
            device_path: "sysctl:vm.loadavg",
            origin: &origin,
            update_interval_ms: Some(5_000),
            label: None,
            rank: 0,
        };
        if let Some(draft) = source_draft(builder.config(), keys, &spec, MacosLocator::Load(field))
        {
            sources.push(draft);
        }
    }
    for (field, value) in [(LoadField::Running, tasks.0), (LoadField::Total, tasks.1)] {
        let _ = value;
        let origin = format!("macos:sysinfo:tasks:{}", field.channel());
        let spec = SourceSpec {
            device_id: "tasks",
            channel: field.channel(),
            entity: &entity_id,
            scope: &scope,
            metric_id: field.metric_id(),
            group: groups::CPU,
            native_unit: native::COUNT,
            raw_kind: RawKind::Gauge,
            driver: DRIVER,
            device_path: "sysinfo:processes",
            origin: &origin,
            update_interval_ms: None,
            label: None,
            rank: 0,
        };
        if let Some(draft) = source_draft(builder.config(), keys, &spec, MacosLocator::Load(field))
        {
            sources.push(draft);
        }
    }
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::Host,
        parent_id: None,
        display_name: facts
            .hostname
            .clone()
            .unwrap_or_else(|| entity::HOST.to_owned()),
        attributes: vec![
            attribute(attributes::CLK_TCK, &CLK_TCK.to_string()),
            attribute(attributes::CPU_MODEL, &facts.cpu_model),
        ],
    };
    builder.add(FAMILY_HOST, parts, incarnation, sources);
}

fn add_cpu(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    facts: &MacosFacts,
    cpu: &(CpuTimes, Vec<CpuTimes>),
) {
    let (aggregate, per_cpu) = cpu;
    let scope = cpu_scope();
    let origin_of = |core: Option<u32>, field: CpuField| {
        format!(
            "macos:mach:cpu:{}:{}",
            core.map_or("aggregate".to_owned(), |core| format!("core{core}")),
            field.channel()
        )
    };
    // Aggregate entity.
    let entity_id = invariant(EntityId::new("cpu"), "cpu");
    let fingerprint = format!("cpu-aggregate-{}-{}", facts.cpu_model, facts.cpu_count);
    if let Some(incarnation) = builder.incarnation(&entity_id, &fingerprint) {
        let device = "cpu".to_owned();
        let sources = CPU_FIELDS
            .into_iter()
            .filter_map(|field| {
                let spec = SourceSpec {
                    device_id: &device,
                    channel: field.channel(),
                    entity: &entity_id,
                    scope: &scope,
                    metric_id: field.metric_id(),
                    group: groups::CPU,
                    native_unit: native::TICKS,
                    raw_kind: RawKind::Counter,
                    driver: DRIVER,
                    device_path: "mach:host_processor_info",
                    origin: &origin_of(None, field),
                    update_interval_ms: None,
                    label: None,
                    rank: 0,
                };
                source_draft(
                    builder.config(),
                    keys,
                    &spec,
                    MacosLocator::CpuTime { core: None, field },
                )
            })
            .collect();
        let parts = EntityParts {
            entity_id,
            kind: EntityKind::Host,
            parent_id: Some(invariant(EntityId::new(entity::HOST), entity::HOST)),
            display_name: "All CPUs".to_owned(),
            attributes: Vec::new(),
        };
        builder.add(FAMILY_CPU, parts, incarnation, sources);
    }
    // Per-core entities in kernel enumeration order.
    for (index, _) in per_cpu.iter().enumerate() {
        let Ok(core) = u32::try_from(index) else {
            break;
        };
        let Ok(entity_id) = entity::cpu_core(core) else {
            builder.error("CPU_CORE_IDENTITY_INVALID");
            continue;
        };
        let fingerprint = format!("cpu-core-{core}-{}", facts.cpu_model);
        let Some(incarnation) = builder.incarnation(&entity_id, &fingerprint) else {
            continue;
        };
        let scope = core_scope(core);
        let device = format!("cpu{core}");
        let sources = CPU_FIELDS
            .into_iter()
            .filter_map(|field| {
                let spec = SourceSpec {
                    device_id: &device,
                    channel: field.channel(),
                    entity: &entity_id,
                    scope: &scope,
                    metric_id: field.metric_id(),
                    group: groups::CPU,
                    native_unit: native::TICKS,
                    raw_kind: RawKind::Counter,
                    driver: DRIVER,
                    device_path: "mach:host_processor_info",
                    origin: &origin_of(Some(core), field),
                    update_interval_ms: None,
                    label: Some(&device),
                    rank: 2,
                };
                source_draft(
                    builder.config(),
                    keys,
                    &spec,
                    MacosLocator::CpuTime {
                        core: Some(core),
                        field,
                    },
                )
            })
            .collect();
        let parts = EntityParts {
            entity_id,
            kind: EntityKind::CpuCore,
            parent_id: None,
            display_name: format!("CPU {core}"),
            attributes: Vec::new(),
        };
        builder.add(FAMILY_CPU, parts, incarnation, sources);
    }
    let _ = aggregate;
}

fn add_memory(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    memory: &MemoryData,
) {
    let entity_id = invariant(EntityId::new(entity::MEMORY), entity::MEMORY);
    let Some(incarnation) = builder.incarnation(&entity_id, entity::MEMORY) else {
        return;
    };
    let scope = system_scope();
    let sources = [
        MemField::Total,
        MemField::Available,
        MemField::SwapTotal,
        MemField::SwapFree,
    ]
    .into_iter()
    .filter_map(|field| {
        // Swap is absent (not zero) when the system reports none.
        if matches!(field, MemField::SwapTotal | MemField::SwapFree) && memory.swap_total == 0 {
            return None;
        }
        let origin = format!("macos:sysinfo:memory:{}", field.channel());
        let spec = SourceSpec {
            device_id: "memory",
            channel: field.channel(),
            entity: &entity_id,
            scope: &scope,
            metric_id: field.metric_id(),
            group: groups::MEMORY,
            native_unit: native::KIB,
            raw_kind: RawKind::Gauge,
            driver: DRIVER,
            device_path: "sysinfo:memory",
            origin: &origin,
            update_interval_ms: None,
            label: None,
            rank: 0,
        };
        source_draft(builder.config(), keys, &spec, MacosLocator::Memory(field))
    })
    .collect();
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::SystemMemory,
        parent_id: None,
        display_name: "Memory".to_owned(),
        attributes: Vec::new(),
    };
    builder.add(FAMILY_MEMORY, parts, incarnation, sources);
}

fn add_processes(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    data: &DiscoveryData,
) {
    let scope = process_scope();
    for process in &data.processes {
        let Some(key) = process_key(
            &keys.host_id,
            &keys.boot_id,
            process.pid,
            process.start_ticks,
        ) else {
            builder.error("PROCESS_IDENTITY_INVALID");
            continue;
        };
        let Ok(entity_id) = key.entity_id() else {
            builder.error("PROCESS_IDENTITY_INVALID");
            continue;
        };
        let fingerprint = format!("{}:{}", process.pid, process.start_ticks);
        let Some(incarnation) = builder.incarnation(&entity_id, &fingerprint) else {
            continue;
        };
        let channel = process_channel(&process.name, &process.command);
        let origin = format!("macos:sysinfo:process:{}", process.pid);
        let device = format!("pid{}:{}", process.pid, process.start_ticks);
        let spec = SourceSpec {
            device_id: &device,
            // The descriptor channel carries the display name plus command
            // line, exactly like the procfs adapter's overridden channel.
            channel: &channel,
            entity: &entity_id,
            scope: &scope,
            metric_id: ids::PROCESS_CPU_TICKS,
            group: groups::PROCESS,
            native_unit: native::TICKS,
            raw_kind: RawKind::Counter,
            driver: DRIVER,
            device_path: &format!("sysinfo:pid:{}", process.pid),
            origin: &origin,
            update_interval_ms: None,
            label: None,
            rank: 0,
        };
        let draft = match source_draft(
            builder.config(),
            keys,
            &spec,
            MacosLocator::Process {
                pid: process.pid,
                start_ticks: process.start_ticks,
            },
        ) {
            Some(draft) => draft,
            None => continue,
        };
        let parts = EntityParts {
            entity_id,
            kind: EntityKind::Process,
            parent_id: None,
            display_name: sanitize_display(&process.name, 64),
            attributes: Vec::new(),
        };
        builder.add(FAMILY_PROCESS, parts, incarnation, vec![draft]);
    }
}

fn add_network(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    interfaces: &[MacosNet],
) {
    for interface in interfaces {
        let Ok(entity_id) = entity::network_interface(NET_NAMESPACE, interface.ifid) else {
            builder.error("INTERFACE_IDENTITY_INVALID");
            continue;
        };
        let fingerprint = format!("{}#{}", interface.ifname, interface.ifid);
        let Some(incarnation) = builder.incarnation(&entity_id, &fingerprint) else {
            continue;
        };
        let scope = interface_scope();
        let device = format!("netif.{}", interface.ifid);
        let sources = NET_COUNTERS
            .into_iter()
            .filter_map(|counter| {
                let origin = format!(
                    "macos:sysinfo:net:{}:{}",
                    interface.ifname,
                    counter.channel()
                );
                let spec = SourceSpec {
                    device_id: &device,
                    channel: counter.channel(),
                    entity: &entity_id,
                    scope: &scope,
                    metric_id: counter.metric_id(),
                    group: groups::NET,
                    native_unit: counter.native_unit(),
                    raw_kind: RawKind::Counter,
                    driver: DRIVER,
                    device_path: &format!("sysinfo:net:{}", interface.ifname),
                    origin: &origin,
                    update_interval_ms: None,
                    label: Some(&interface.ifname),
                    rank: 0,
                };
                source_draft(
                    builder.config(),
                    keys,
                    &spec,
                    MacosLocator::Net {
                        ifname: interface.ifname.clone(),
                        ifid: interface.ifid,
                        counter,
                    },
                )
            })
            .collect();
        let parts = EntityParts {
            entity_id,
            kind: EntityKind::NetworkInterface,
            parent_id: Some(invariant(EntityId::new(entity::HOST), entity::HOST)),
            display_name: interface.ifname.clone(),
            attributes: vec![
                attribute(attributes::IFNAME, &interface.ifname),
                attribute(LOOPBACK_ATTRIBUTE, &interface.loopback.to_string()),
            ],
        };
        builder.add(FAMILY_INTERFACE, parts, incarnation, sources);
    }
}

fn add_filesystems(
    builder: &mut SnapshotBuilder<'_, MacosLocator>,
    keys: &KeyContext,
    mounts: &[MacosMount],
) {
    let scope = filesystem_scope();
    for mount in mounts {
        let Ok(entity_id) = entity::filesystem(NET_NAMESPACE, mount.mount_id) else {
            builder.error("MOUNT_IDENTITY_INVALID");
            continue;
        };
        let fingerprint = format!("{}:{}:{}", mount.device, mount.mount_point, mount.fs_type);
        let Some(incarnation) = builder.incarnation(&entity_id, &fingerprint) else {
            continue;
        };
        let device = format!("mount.{}", mount.mount_id);
        let sources = FS_FIGURES
            .into_iter()
            .filter_map(|figure| {
                let origin = format!(
                    "macos:sysinfo:fs:{}:{}",
                    mount.mount_point,
                    figure.channel()
                );
                let spec = SourceSpec {
                    device_id: &device,
                    channel: figure.channel(),
                    entity: &entity_id,
                    scope: &scope,
                    metric_id: figure.metric_id(),
                    group: groups::CAPACITY,
                    native_unit: native::BYTES,
                    raw_kind: RawKind::Gauge,
                    driver: DRIVER,
                    device_path: &mount.mount_point,
                    origin: &origin,
                    update_interval_ms: None,
                    label: Some(&mount.mount_point),
                    rank: 0,
                };
                source_draft(
                    builder.config(),
                    keys,
                    &spec,
                    MacosLocator::Fs {
                        mount_id: mount.mount_id,
                        figure,
                    },
                )
            })
            .collect();
        let parts = EntityParts {
            entity_id,
            kind: EntityKind::Filesystem,
            parent_id: None,
            display_name: mount.mount_point.clone(),
            attributes: vec![
                attribute(attributes::MOUNT_POINT, &mount.mount_point),
                attribute(attributes::DEVICE, &mount.device),
                attribute(attributes::FS_TYPE, &mount.fs_type),
            ],
        };
        builder.add(FAMILY_MOUNT, parts, incarnation, sources);
    }
}

fn add_gpu(builder: &mut SnapshotBuilder<'_, MacosLocator>, keys: &KeyContext, gpu: &AppleGpu) {
    let slug = gpu_model_slug(&gpu.model);
    let Some(entity_id) = gpu_entity_id(&slug) else {
        builder.error("GPU_IDENTITY_INVALID");
        return;
    };
    let Some(incarnation) = builder.incarnation(&entity_id, &format!("apple-gpu-{slug}")) else {
        return;
    };
    let scope = gpu_scope();
    let device = format!("gpu.{slug}");
    let mut sources = Vec::new();
    // Utilization gauge.
    let origin = format!("macos:iokit:{slug}:utilization");
    let spec = SourceSpec {
        device_id: &device,
        channel: "utilization",
        entity: &entity_id,
        scope: &scope,
        metric_id: ids::GPU_UTILIZATION_COMPUTE,
        group: groups::GPU_FAST,
        native_unit: native::PERCENT,
        raw_kind: RawKind::Gauge,
        driver: DRIVER,
        device_path: "iokit:IOAccelerator",
        origin: &origin,
        update_interval_ms: None,
        label: Some(&gpu.model),
        rank: 0,
    };
    if let Some(draft) = source_draft(builder.config(), keys, &spec, MacosLocator::GpuUtil) {
        sources.push(draft);
    }
    // Unified memory gauges.
    for field in GPU_MEM_FIELDS {
        let origin = format!("macos:iokit:{slug}:{}", field.channel());
        let spec = SourceSpec {
            device_id: &device,
            channel: field.channel(),
            entity: &entity_id,
            scope: &scope,
            metric_id: field.metric_id(),
            group: groups::GPU_FAST,
            native_unit: native::BYTES,
            raw_kind: RawKind::Gauge,
            driver: DRIVER,
            device_path: "iokit:IOAccelerator",
            origin: &origin,
            update_interval_ms: None,
            label: Some(&gpu.model),
            rank: 0,
        };
        if let Some(draft) =
            source_draft(builder.config(), keys, &spec, MacosLocator::GpuMem(field))
        {
            sources.push(draft);
        }
    }
    // Core count (static fact, refreshed with the slow group).
    let origin = format!("macos:iokit:{slug}:cores");
    let spec = SourceSpec {
        device_id: &device,
        channel: "cores",
        entity: &entity_id,
        scope: &scope,
        metric_id: ids::GPU_CORES,
        group: groups::GPU_HEALTH,
        native_unit: native::COUNT,
        raw_kind: RawKind::Static,
        driver: DRIVER,
        device_path: "iokit:IOAccelerator",
        origin: &origin,
        update_interval_ms: None,
        label: Some(&gpu.model),
        rank: 0,
    };
    if let Some(draft) = source_draft(builder.config(), keys, &spec, MacosLocator::GpuCores) {
        sources.push(draft);
    }
    let parts = EntityParts {
        entity_id,
        kind: EntityKind::Gpu,
        parent_id: None,
        display_name: gpu.model.clone(),
        attributes: vec![
            attribute(attributes::UUID, &format!("APPLE-{slug}")),
            attribute(attributes::BRAND, "Apple"),
            attribute(attributes::ARCHITECTURE, "Apple Silicon"),
            attribute(attributes::MEMORY_MODEL, "unified"),
        ],
    };
    builder.add(FAMILY_GPU, parts, incarnation, sources);
}

fn add_capabilities(builder: &mut SnapshotBuilder<'_, MacosLocator>, cpu_times: bool, gpu: bool) {
    let supported = |id: &str| Capability {
        capability_id: id.to_owned(),
        entity_id: None,
        support: Support::Supported,
        reason: None,
        min_interval_ms: None,
    };
    let unsupported = |id: &str, reason: &str| Capability {
        capability_id: id.to_owned(),
        entity_id: None,
        support: Support::Unsupported,
        reason: Some(reason.to_owned()),
        min_interval_ms: None,
    };
    // CPU times are the only supported CPU source; frequency, temperature
    // and power need drivers macOS does not expose to unprivileged readers.
    builder.capability(if cpu_times {
        supported("macos.cpu.times")
    } else {
        unsupported("macos.cpu.times", MACH_TIMES_UNAVAILABLE)
    });
    builder.capability(unsupported("macos.cpu.frequency", FREQ_UNAVAILABLE));
    builder.capability(unsupported("macos.cpu.temperature", TEMP_UNAVAILABLE));
    builder.capability(unsupported("macos.cpu.power", POWER_UNAVAILABLE));
    builder.capability(supported("macos.memory"));
    builder.capability(supported("macos.process.list"));
    builder.capability(supported("macos.network.statistics"));
    builder.capability(unsupported("macos.network.drops", DROPS_ABSENT));
    builder.capability(supported("macos.filesystem.capacity"));
    builder.capability(if gpu {
        supported("macos.gpu.compute")
    } else {
        unsupported("macos.gpu.compute", GPU_SERVICE_ABSENT)
    });
    builder.capability(unsupported("macos.gpu.temperature", GPU_TEMP_UNAVAILABLE));
    builder.capability(unsupported("macos.gpu.power", GPU_POWER_UNAVAILABLE));
}

/// Bytes of a memory field, converted to KiB like the meminfo adapter.
pub(crate) fn memory_kib(memory: &MemoryData, field: MemField) -> u64 {
    let bytes = match field {
        MemField::Total => memory.total,
        MemField::Available => memory.available,
        MemField::SwapTotal => memory.swap_total,
        MemField::SwapFree => memory.swap_free,
    };
    bytes / KIB
}

/// Unified-memory view of an IOKit read: total is system memory, free is
/// what the GPU has not allocated, saturating instead of underflowing.
pub(crate) fn gpu_memory(gpu: &AppleGpu, total_bytes: u64, field: GpuMemField) -> u64 {
    match field {
        GpuMemField::Used => gpu.mem_used_bytes.min(total_bytes),
        GpuMemField::Free => total_bytes.saturating_sub(gpu.mem_used_bytes.min(total_bytes)),
        GpuMemField::Total => total_bytes,
    }
}

/// Ticks of one Mach field; the Linux-only states stay at zero.
pub(crate) fn cpu_ticks(times: &CpuTimes, field: CpuField) -> u64 {
    match field {
        CpuField::User => times.user,
        CpuField::Nice => times.nice,
        CpuField::System => times.system,
        CpuField::Idle => times.idle,
        CpuField::IoWait | CpuField::Irq | CpuField::SoftIrq | CpuField::Steal => 0,
    }
}

/// Counter of one network field from a fresh interface read.
pub(crate) fn net_counter(interface: &MacosNet, counter: NetCounter) -> u64 {
    match counter {
        NetCounter::RxBytes => interface.rx_bytes,
        NetCounter::TxBytes => interface.tx_bytes,
        NetCounter::RxPackets => interface.rx_packets,
        NetCounter::TxPackets => interface.tx_packets,
        NetCounter::RxErrors => interface.rx_errors,
        NetCounter::TxErrors => interface.tx_errors,
    }
}

/// Bytes of one capacity figure from a fresh mount read.
pub(crate) fn fs_bytes(mount: &MacosMount, figure: FsFigure) -> u64 {
    match figure {
        FsFigure::Total => mount.total,
        FsFigure::Free => mount.free,
        FsFigure::Available => mount.available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_slugs_are_bounded_and_safe() {
        assert_eq!(gpu_model_slug("Apple M2 Max"), "APPLE-M2-MAX");
        assert_eq!(gpu_model_slug("   "), "GPU");
        let slug = gpu_model_slug("Apple M4 Pro (12-core)");
        assert!(slug.len() <= 48);
        assert!(!slug.contains(' '));
    }

    #[test]
    fn interface_and_mount_ids_are_stable() {
        assert_eq!(interface_id("en0"), interface_id("en0"));
        assert_ne!(interface_id("en0"), interface_id("lo0"));
        assert_eq!(mount_id("/"), mount_id("/"));
    }

    #[test]
    fn gpu_free_memory_saturates() {
        let gpu = AppleGpu {
            model: "Apple M2 Max".to_owned(),
            mem_used_bytes: u64::MAX,
            ..AppleGpu::default()
        };
        assert_eq!(gpu_memory(&gpu, 1024, GpuMemField::Free), 0);
        assert_eq!(gpu_memory(&gpu, 1024, GpuMemField::Used), 1024);
    }

    #[test]
    fn linux_only_cpu_states_read_zero() {
        let times = CpuTimes {
            user: 10,
            nice: 1,
            system: 5,
            idle: 100,
        };
        assert_eq!(cpu_ticks(&times, CpuField::User), 10);
        assert_eq!(cpu_ticks(&times, CpuField::IoWait), 0);
        assert_eq!(cpu_ticks(&times, CpuField::Steal), 0);
    }
}
