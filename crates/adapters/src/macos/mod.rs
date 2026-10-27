//! Apple Silicon telemetry adapter (`macos`).
//!
//! One native adapter covers the seven read groups the collectors consume,
//! mirroring the Linux adapters' shapes so no collector needs a platform
//! split:
//!
//! * `cpu`: Mach tick counters (`host_processor_info`) for the aggregate and
//!   every core — the Linux-only states stay at zero — plus load averages
//!   and task counts (`sysinfo`).
//! * `memory`: KiB gauges from `sysinfo` (Apple's own
//!   free+inactive+purgeable−compressor availability formula).
//! * `process`: one `process.cpu.ticks` counter per `ProcessKey`
//!   (`process:macos:PID:START`, start from `sysinfo` against the
//!   `kern.boottime` epoch) with `proc.*` tags; process ticks accumulate the
//!   `sysinfo` CPU share, an exact unit conversion of the same deltas.
//! * `net`: kernel-cumulative byte/packet/error counters per interface
//!   (`sysinfo`); drops have no macOS counter and stay absent, never zero.
//! * `capacity`: mount capacities via `statvfs` over the `sysinfo` mount
//!   table (exact `f_blocks`/`f_bfree`/`f_bavail`, never estimated).
//! * `gpu_fast` / `gpu_health`: Apple GPU utilization and unified memory
//!   from IOKit (`IOAccelerator`, no privileges needed) plus the core
//!   count; temperature, clocks and power need `powermetrics` as root, so
//!   those capabilities stay unsupported with an explicit reason.
//!
//! The adapter never fails construction: every subsystem degrades to
//! unavailable/unsupported records with a reason, and the GPU reads
//! are cached so the two GPU groups share one IOKit lookup per cycle.

mod catalog;
mod iokit;
mod mach_cpu;
mod sysctl;

use std::collections::BTreeMap;
use std::sync::Arc;

use fox_services_top_core::clock::TimeProvider;
use fox_services_top_core::identity::HostId;
use fox_services_top_core::metric::Scalar;
use fox_services_top_core::naming::{adapters, groups};
use fox_services_top_core::observation::{
    AdapterManifest, CatalogDelta, DiscoverRequest, Observation, ObservationBatch, ReadPlan,
    SourceDescriptor,
};
use fox_services_top_core::ports::TelemetryPort;
use fox_services_top_core::quality::{Availability, Quality, ReadStatus};
use sysinfo::{Disks, Networks, ProcessStatus, System};

use catalog::{
    CLK_TCK, DiscoveryData, LoadField, MacosFacts, MacosLocator, MacosMount, MacosNet,
    MacosProcess, MemField, MemoryData,
};
use catalog::{
    discover_all, fs_bytes, gpu_memory, interface_id, memory_kib, mount_id, net_counter,
    process_tags,
};

use crate::common::RecordFactory;
use crate::procfs::catalog::{CatalogBook, KeyContext, Selected};
use crate::procfs::mechanics::{
    GroupPagers, PlanTarget, ReadSnapshot, ReprobeGate, plan_target, rejected_discovery,
    rejected_plan,
};
use crate::procfs::records::{ReadWindow, failed_record, ok_record, removed_record};
use crate::registry::{AdapterOptions, kernel_file_manifest};

/// GPU reads are shared between the two GPU groups within one cycle.
const GPU_CACHE_TTL_NS: u64 = 2_000_000_000;
/// Process-list refreshes are shared between the `cpu` (tasks) and
/// `process` groups within one cycle.
const PROC_CACHE_TTL_NS: u64 = 900_000_000;

/// Static manifest with default options.
pub fn manifest() -> AdapterManifest {
    manifest_with(&AdapterOptions::default())
}

/// Manifest for `options` (source limit = min(max_sources, 2048)).
pub fn manifest_with(options: &AdapterOptions) -> AdapterManifest {
    kernel_file_manifest(
        adapters::MACOS,
        catalog::IMPLEMENTATION,
        catalog::MINIMUM_SAMPLE_MS,
        options,
    )
}

/// Read-only Apple Silicon telemetry adapter.
pub struct MacosAdapter {
    clock: Arc<dyn TimeProvider>,
    host_id: HostId,
    options: AdapterOptions,
    manifest: AdapterManifest,
    book: CatalogBook<MacosLocator>,
    read_gate: ReprobeGate,
    pagers: GroupPagers,
    shut_down: bool,
    system: System,
    facts: Option<MacosFacts>,
    gpu_cache: Option<(u64, iokit::AppleGpu)>,
    proc_ticks: BTreeMap<i32, u64>,
    proc_refresh_ns: Option<u64>,
}

impl MacosAdapter {
    /// Never fails; every subsystem degrades gracefully when unreadable.
    pub fn new(clock: Arc<dyn TimeProvider>, host_id: HostId, options: AdapterOptions) -> Self {
        Self {
            clock,
            host_id,
            manifest: manifest_with(&options),
            book: catalog::new_book(&options),
            read_gate: ReprobeGate::new(options.unsupported_probe_ms),
            pagers: GroupPagers::default(),
            shut_down: false,
            system: System::new(),
            facts: None,
            gpu_cache: None,
            proc_ticks: BTreeMap::new(),
            proc_refresh_ns: None,
            options,
        }
    }

    /// Size of the catalog bookkeeping (boundedness tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn catalog_stats(&self) -> crate::procfs::CatalogStats {
        self.book.stats()
    }

    fn keys(&self) -> KeyContext {
        KeyContext {
            host_id: self.host_id.clone(),
            boot_id: self.clock.boot_id(),
        }
    }

    /// Static facts, read once; `None` when the host cannot be described.
    fn ensure_facts(&mut self) -> Option<&MacosFacts> {
        if self.facts.is_none() {
            self.facts = collect_facts();
        }
        self.facts.as_ref()
    }

    /// GPU data, shared between the two GPU groups within one cycle and
    /// throttled like an unsupported capability while IOKit fails.
    fn gpu_data(&mut self, now_boot_ns: u64) -> Option<iokit::AppleGpu> {
        if let Some((at, gpu)) = self.gpu_cache.as_ref()
            && now_boot_ns.saturating_sub(*at) < GPU_CACHE_TTL_NS
        {
            self.read_gate.record_success(GPU_REPROBE_KEY);
            return Some(gpu.clone());
        }
        if self
            .read_gate
            .deferred(GPU_REPROBE_KEY, now_boot_ns)
            .is_some()
        {
            return None;
        }
        match iokit::read_gpu() {
            Ok(gpu) => {
                self.read_gate.record_success(GPU_REPROBE_KEY);
                self.gpu_cache = Some((now_boot_ns, gpu.clone()));
                Some(gpu)
            }
            Err(reason) => {
                self.read_gate.record_failure(
                    GPU_REPROBE_KEY,
                    now_boot_ns,
                    ReadStatus::Unsupported,
                    reason,
                );
                None
            }
        }
    }

    /// Refreshes the process list at most once per [`PROC_CACHE_TTL_NS`];
    /// `sysinfo` settles per-process CPU shares across consecutive
    /// refreshes, so the first cycle only warms up and accumulates nothing.
    fn ensure_processes(&mut self, now_boot_ns: u64) {
        let fresh = self
            .proc_refresh_ns
            .is_none_or(|at| now_boot_ns.saturating_sub(at) >= PROC_CACHE_TTL_NS);
        if fresh {
            self.system.refresh_processes();
            self.proc_refresh_ns = Some(now_boot_ns);
        }
    }

    fn discover_snapshot(&mut self, request: &DiscoverRequest, now_boot_ns: u64) -> CatalogDelta {
        let keys = self.keys();
        let Some(facts) = self.ensure_facts().cloned() else {
            let mut builder = self.book.begin_snapshot(now_boot_ns);
            builder.error("MACOS_FACTS_UNREADABLE");
            return builder.finish(request, Availability::Unavailable);
        };
        self.ensure_processes(now_boot_ns);
        self.system.refresh_memory();
        let data = DiscoveryData {
            cpu: mach_cpu::cpu_times(),
            memory: read_memory(&self.system),
            load: read_load(),
            tasks: read_tasks(&self.system),
            processes: read_processes(&self.system, &facts),
            interfaces: read_network(),
            mounts: read_mounts(),
            gpu: self.gpu_data(now_boot_ns),
            facts,
        };
        let mut builder = self.book.begin_snapshot(now_boot_ns);
        let availability = discover_all(&mut builder, &keys, &data);
        builder.finish(request, availability)
    }

    fn read_group(
        &mut self,
        plan: &ReadPlan,
        target: &PlanTarget,
        now_boot_ns: u64,
    ) -> ObservationBatch {
        if !self.book.is_discovered() {
            return RecordFactory::failed_batch(plan, now_boot_ns, "CATALOG_NOT_DISCOVERED");
        }
        let page_size = self.options.effective_page_size();
        let Self {
            book,
            pagers,
            clock,
            system,
            facts,
            gpu_cache,
            proc_ticks,
            proc_refresh_ns,
            read_gate: gate,
            ..
        } = self;
        pagers.serve(plan, target, page_size, now_boot_ns, || {
            let selection = book.select(&target.group, &plan.source_slots);
            let mut context = GroupRead {
                plan,
                clock: &**clock,
                system,
                facts: facts.clone(),
                gpu_cache,
                proc_ticks,
                proc_refresh_ns,
                gate,
            };
            let mut snapshot = match target.group.as_str() {
                groups::CPU => context.read_cpu(&selection.sources),
                groups::MEMORY => context.read_memory(&selection.sources),
                groups::PROCESS => context.read_processes(&selection.sources),
                groups::NET => context.read_net(&selection.sources),
                groups::CAPACITY => context.read_capacity(&selection.sources),
                groups::GPU_FAST => context.read_gpu_fast(&selection.sources),
                groups::GPU_HEALTH => context.read_gpu_health(&selection.sources),
                _ => ReadSnapshot {
                    records: Vec::new(),
                    started_boot_ns: now_boot_ns,
                    finished_boot_ns: now_boot_ns,
                    final_error: Some("UNKNOWN_READ_GROUP".to_owned()),
                },
            };
            if selection.unknown_slots > 0 {
                snapshot.final_error = Some("UNKNOWN_SOURCE_SLOT".to_owned());
            }
            snapshot
        })
    }
}

/// Everything one group read needs; the adapter's state is borrowed
/// mutably per group so `pagers.serve` keeps its single closure.
struct GroupRead<'a> {
    plan: &'a ReadPlan,
    clock: &'a dyn TimeProvider,
    system: &'a mut System,
    facts: Option<MacosFacts>,
    gpu_cache: &'a mut Option<(u64, iokit::AppleGpu)>,
    proc_ticks: &'a mut BTreeMap<i32, u64>,
    proc_refresh_ns: &'a mut Option<u64>,
    gate: &'a mut ReprobeGate,
}

/// Re-probe key of the IOKit GPU read: a missing I/O Kit service is
/// re-probed at most once per unsupported-probe interval, like the Linux
/// adapters' unsupported capabilities.
const GPU_REPROBE_KEY: &str = "iokit-gpu";

impl GroupRead<'_> {
    fn window(&self, started: u64, finished: u64) -> ReadWindow {
        ReadWindow {
            started_boot_ns: started,
            finished_boot_ns: finished,
        }
    }

    fn removed(&self, descriptor: &SourceDescriptor, now: u64) -> Option<Observation> {
        removed_record(self.plan, descriptor, now)
    }

    fn read_cpu(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        let times = mach_cpu::cpu_times();
        let (load_one, load_five, load_fifteen) = read_load();
        let load_values = [load_one, load_five, load_fifteen];
        self.ensure_proc_list(started);
        let tasks = read_tasks(self.system);
        let task_values = [tasks.0, tasks.1];
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), self.clock.now_boot_ns()));
                continue;
            };
            let record = match &source.locator {
                MacosLocator::CpuTime { core, field } => {
                    let ticks = times.as_ref().and_then(|(aggregate, per_cpu)| {
                        let view = match core {
                            None => aggregate,
                            Some(index) => per_cpu.get(usize::try_from(*index).ok()?)?,
                        };
                        Some(catalog::cpu_ticks(view, *field))
                    });
                    match ticks {
                        Some(value) => ok_record(
                            self.plan,
                            &source.descriptor,
                            Scalar::from_u64(value),
                            Quality::Verified,
                            self.window(started, self.clock.now_boot_ns()),
                            Vec::new(),
                        ),
                        None => failed_record(
                            self.plan,
                            &source.descriptor,
                            ReadStatus::Unsupported,
                            catalog::MACH_TIMES_UNAVAILABLE,
                            self.window(started, self.clock.now_boot_ns()),
                        ),
                    }
                }
                MacosLocator::Load(field) => {
                    let value = match field {
                        LoadField::One => Some(load_values[0]),
                        LoadField::Five => Some(load_values[1]),
                        LoadField::Fifteen => Some(load_values[2]),
                        LoadField::Running => Some(task_values[0] as f64),
                        LoadField::Total => Some(task_values[1] as f64),
                    };
                    match value.filter(|value| value.is_finite()) {
                        Some(value) => match Scalar::from_f64(value) {
                            Ok(scalar) => ok_record(
                                self.plan,
                                &source.descriptor,
                                scalar,
                                Quality::Verified,
                                self.window(started, self.clock.now_boot_ns()),
                                Vec::new(),
                            ),
                            Err(_) => failed_record(
                                self.plan,
                                &source.descriptor,
                                ReadStatus::Error,
                                "LOADAVG_UNPARSABLE",
                                self.window(started, self.clock.now_boot_ns()),
                            ),
                        },
                        None => failed_record(
                            self.plan,
                            &source.descriptor,
                            ReadStatus::Error,
                            "LOADAVG_UNAVAILABLE",
                            self.window(started, self.clock.now_boot_ns()),
                        ),
                    }
                }
                _ => failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    self.window(started, self.clock.now_boot_ns()),
                ),
            };
            records.extend(record);
        }
        let finished = self.clock.now_boot_ns();
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    fn read_memory(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        self.system.refresh_memory();
        let memory = read_memory(self.system);
        let finished = self.clock.now_boot_ns();
        let window = self.window(started, finished);
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), finished));
                continue;
            };
            let MacosLocator::Memory(field) = &source.locator else {
                records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    window,
                ));
                continue;
            };
            // Swap is absent (not zero) when the system reports none.
            if matches!(field, MemField::SwapTotal | MemField::SwapFree) && memory.swap_total == 0 {
                records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Unsupported,
                    "SWAP_ABSENT",
                    window,
                ));
                continue;
            }
            records.extend(ok_record(
                self.plan,
                &source.descriptor,
                Scalar::from_u64(memory_kib(&memory, *field)),
                Quality::Verified,
                window,
                Vec::new(),
            ));
        }
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    fn read_processes(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        // Capture the previous refresh first: the tick advancement below
        // converts the settled CPU share over exactly this interval.
        let previous = *self.proc_refresh_ns;
        self.ensure_proc_list(started);
        let Some(facts) = self.facts.clone() else {
            let finished = self.clock.now_boot_ns();
            return ReadSnapshot {
                records: selection
                    .iter()
                    .filter_map(|selected| {
                        failed_record(
                            self.plan,
                            selected.descriptor(),
                            ReadStatus::Error,
                            "MACOS_FACTS_UNREADABLE",
                            self.window(started, finished),
                        )
                    })
                    .collect(),
                started_boot_ns: started,
                finished_boot_ns: finished,
                final_error: None,
            };
        };
        // Advance the cumulative tick counters by the settled CPU share over
        // the elapsed time: an exact unit conversion of the same deltas.
        let now = self.clock.now_boot_ns();
        let elapsed_s = previous
            .and_then(|at| started.checked_sub(at))
            .map(|dt_ns| dt_ns as f64 / 1_000_000_000.0)
            .unwrap_or(0.0);
        let mut seen = BTreeMap::new();
        for (pid, process) in self.system.processes() {
            let pid_i32 = pid.as_u32().cast_signed();
            let usage = process.cpu_usage() as f64 / 100.0;
            // `usage` is a settled share in [0, ~NCPUS] and `elapsed_s` a
            // ~1 s interval, so the product fits in u64 by construction.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let advance = if elapsed_s > 0.0 && usage > 0.0 {
                (usage * elapsed_s * CLK_TCK as f64).round() as u64
            } else {
                0
            };
            let entry = self.proc_ticks.entry(pid_i32).or_insert(0);
            *entry = entry.saturating_add(advance);
            seen.insert(pid_i32, *entry);
        }
        // PIDs that exited no longer advance; prune them lazily.
        self.proc_ticks.retain(|pid, _| seen.contains_key(pid));
        let mut by_pid: BTreeMap<i32, MacosProcess> = BTreeMap::new();
        for process in read_processes(self.system, &facts) {
            by_pid.insert(process.pid, process);
        }
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), now));
                continue;
            };
            let MacosLocator::Process { pid, .. } = &source.locator else {
                records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    self.window(started, now),
                ));
                continue;
            };
            match by_pid.remove(pid) {
                Some(process) => {
                    let ticks = seen.get(pid).copied().unwrap_or(0);
                    records.extend(ok_record(
                        self.plan,
                        &source.descriptor,
                        Scalar::from_u64(ticks),
                        Quality::Verified,
                        self.window(started, now),
                        process_tags(&process),
                    ));
                }
                None => records.extend(self.removed(&source.descriptor, now)),
            }
        }
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: now,
            final_error: None,
        }
    }

    /// Refreshes the process list at most once per [`PROC_CACHE_TTL_NS`],
    /// sharing one refresh between the `cpu` (tasks) and `process` groups.
    fn ensure_proc_list(&mut self, now: u64) {
        let fresh = self
            .proc_refresh_ns
            .is_none_or(|at| now.saturating_sub(at) >= PROC_CACHE_TTL_NS);
        if fresh {
            self.system.refresh_processes();
            *self.proc_refresh_ns = Some(now);
        }
    }

    fn read_net(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        let interfaces = read_network();
        let finished = self.clock.now_boot_ns();
        let window = self.window(started, finished);
        let by_name: BTreeMap<&str, &MacosNet> = interfaces
            .iter()
            .map(|interface| (interface.ifname.as_str(), interface))
            .collect();
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), finished));
                continue;
            };
            let MacosLocator::Net {
                ifname, counter, ..
            } = &source.locator
            else {
                records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    window,
                ));
                continue;
            };
            match by_name.get(ifname.as_str()) {
                Some(interface) => records.extend(ok_record(
                    self.plan,
                    &source.descriptor,
                    Scalar::from_u64(net_counter(interface, *counter)),
                    Quality::Verified,
                    window,
                    Vec::new(),
                )),
                None => records.extend(self.removed(&source.descriptor, finished)),
            }
        }
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    fn read_capacity(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        let mounts = read_mounts();
        let finished = self.clock.now_boot_ns();
        let window = self.window(started, finished);
        let by_id: BTreeMap<u64, &MacosMount> =
            mounts.iter().map(|mount| (mount.mount_id, mount)).collect();
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), finished));
                continue;
            };
            let MacosLocator::Fs { mount_id, figure } = &source.locator else {
                records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    window,
                ));
                continue;
            };
            match by_id.get(mount_id) {
                Some(mount) => records.extend(ok_record(
                    self.plan,
                    &source.descriptor,
                    Scalar::from_u64(fs_bytes(mount, *figure)),
                    Quality::Verified,
                    window,
                    Vec::new(),
                )),
                None => records.extend(self.removed(&source.descriptor, finished)),
            }
        }
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    fn read_gpu_fast(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        let total = self.facts.as_ref().map(|facts| facts.mem_total_bytes);
        let gpu = self.gpu_for_read(started);
        let failure = gpu.as_ref().err().cloned();
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), started));
                continue;
            };
            let value = match (&gpu, total, &source.locator) {
                (Ok(gpu), _, MacosLocator::GpuUtil) => Some(gpu.device_util_pct),
                (Ok(gpu), Some(total), MacosLocator::GpuMem(field)) => {
                    Some(gpu_memory(gpu, total, *field))
                }
                _ => None,
            };
            match (value, &failure) {
                (Some(value), _) => records.extend(ok_record(
                    self.plan,
                    &source.descriptor,
                    Scalar::from_u64(value),
                    Quality::Verified,
                    self.window(started, self.clock.now_boot_ns()),
                    Vec::new(),
                )),
                (None, Some((status, reason))) => records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    *status,
                    reason,
                    self.window(started, self.clock.now_boot_ns()),
                )),
                (None, None) => records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    self.window(started, self.clock.now_boot_ns()),
                )),
            }
        }
        let finished = self.clock.now_boot_ns();
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    fn read_gpu_health(&mut self, selection: &[Selected<'_, MacosLocator>]) -> ReadSnapshot {
        let started = self.clock.now_boot_ns();
        let gpu = self.gpu_for_read(started);
        let failure = gpu.as_ref().err().cloned();
        let mut records = Vec::with_capacity(selection.len());
        for selected in selection {
            let Selected::Live(source) = selected else {
                records.extend(self.removed(selected.descriptor(), started));
                continue;
            };
            match (&gpu, &failure, &source.locator) {
                (Ok(gpu), _, MacosLocator::GpuCores) => records.extend(ok_record(
                    self.plan,
                    &source.descriptor,
                    Scalar::from_u64(gpu.core_count),
                    Quality::Verified,
                    self.window(started, self.clock.now_boot_ns()),
                    Vec::new(),
                )),
                (Err(_), Some((status, reason)), MacosLocator::GpuCores) => {
                    records.extend(failed_record(
                        self.plan,
                        &source.descriptor,
                        *status,
                        reason,
                        self.window(started, self.clock.now_boot_ns()),
                    ));
                }
                _ => records.extend(failed_record(
                    self.plan,
                    &source.descriptor,
                    ReadStatus::Error,
                    "WRONG_READ_GROUP",
                    self.window(started, self.clock.now_boot_ns()),
                )),
            }
        }
        let finished = self.clock.now_boot_ns();
        ReadSnapshot {
            records,
            started_boot_ns: started,
            finished_boot_ns: finished,
            final_error: None,
        }
    }

    /// Cached GPU read shared by the two GPU groups, throttled while
    /// IOKit fails. The error carries the sticky status and reason for
    /// the records.
    fn gpu_for_read(&mut self, now: u64) -> Result<iokit::AppleGpu, (ReadStatus, String)> {
        if let Some((at, gpu)) = self.gpu_cache.as_ref()
            && now.saturating_sub(*at) < GPU_CACHE_TTL_NS
        {
            self.gate.record_success(GPU_REPROBE_KEY);
            return Ok(gpu.clone());
        }
        if let Some(sticky) = self.gate.deferred(GPU_REPROBE_KEY, now) {
            return Err((sticky.status, sticky.reason));
        }
        match iokit::read_gpu() {
            Ok(gpu) => {
                self.gate.record_success(GPU_REPROBE_KEY);
                *self.gpu_cache = Some((now, gpu.clone()));
                Ok(gpu)
            }
            Err(reason) => {
                self.gate
                    .record_failure(GPU_REPROBE_KEY, now, ReadStatus::Unsupported, reason);
                Err((ReadStatus::Unsupported, reason.to_owned()))
            }
        }
    }
}

/// Static host facts from sysctls; `None` when the essentials are missing.
fn collect_facts() -> Option<MacosFacts> {
    let cpu_model =
        sysctl::string("machdep.cpu.brand_string").or_else(|| sysctl::string("hw.model"))?;
    let cpu_count = sysctl::u64_value("hw.ncpu").and_then(|count| u32::try_from(count).ok())?;
    if cpu_count == 0 {
        return None;
    }
    let mem_total_bytes = sysctl::u64_value("hw.memsize").filter(|total| *total > 0)?;
    // `kern.boottime` as unix seconds anchors process start ticks.
    let boot_unix = System::boot_time();
    Some(MacosFacts {
        hostname: sysctl::string("kern.hostname"),
        cpu_model: sanitize_host_text(&cpu_model),
        cpu_count,
        mem_total_bytes,
        boot_unix,
    })
}

fn sanitize_host_text(text: &str) -> String {
    const LIMIT: usize = 128;
    let clean = fox_services_top_core::text::sanitize_display(text.trim(), LIMIT);
    if clean.is_empty() {
        "Apple Silicon".to_owned()
    } else {
        clean
    }
}

/// Memory figures in bytes from a refreshed `System`.
fn read_memory(system: &System) -> MemoryData {
    MemoryData {
        total: system.total_memory(),
        available: system.available_memory(),
        swap_total: system.total_swap(),
        swap_free: system.free_swap(),
    }
}

/// Load averages; always finite on a live kernel.
fn read_load() -> (f64, f64, f64) {
    let load = System::load_average();
    (load.one, load.five, load.fifteen)
}

/// `(running, total)` processes from a refreshed `System`.
fn read_tasks(system: &System) -> (u64, u64) {
    let mut running = 0u64;
    let mut total = 0u64;
    for process in system.processes().values() {
        total += 1;
        if process.status() == ProcessStatus::Run {
            running += 1;
        }
    }
    (running, total)
}

/// Process list with boot-anchored start ticks.
fn read_processes(system: &System, facts: &MacosFacts) -> Vec<MacosProcess> {
    let mut processes = Vec::with_capacity(system.processes().len());
    for process in system.processes().values() {
        let pid = process.pid().as_u32().cast_signed();
        if pid <= 0 {
            continue;
        }
        let start_ticks = process
            .start_time()
            .saturating_sub(facts.boot_unix)
            .saturating_mul(CLK_TCK);
        let name = process.name().to_owned();
        let command = process.cmd().join(" ");
        processes.push(MacosProcess {
            pid,
            ppid: process.parent().map(|parent| parent.as_u32().cast_signed()),
            name,
            command,
            uid: process.user_id().map(|uid| **uid),
            state: process_state(process.status()),
            rss_bytes: Some(process.memory()),
            start_ticks,
        });
    }
    processes.sort_by_key(|process| process.pid);
    processes
}

/// `R`/`S`/… state letter from the `sysinfo` status. Exotic states have no
/// Linux letter and read as unknown rather than a wrong one.
fn process_state(status: ProcessStatus) -> char {
    match status {
        ProcessStatus::Run => 'R',
        ProcessStatus::Sleep => 'S',
        ProcessStatus::Idle => 'I',
        ProcessStatus::Zombie => 'Z',
        ProcessStatus::Stop => 'T',
        ProcessStatus::Tracing => 't',
        ProcessStatus::Dead => 'X',
        _ => '?',
    }
}

/// Interface list with kernel-cumulative counters.
fn read_network() -> Vec<MacosNet> {
    let networks = Networks::new_with_refreshed_list();
    let mut interfaces: Vec<MacosNet> = networks
        .list()
        .iter()
        .map(|(ifname, data)| MacosNet {
            ifid: interface_id(ifname),
            ifname: ifname.clone(),
            rx_bytes: data.total_received(),
            tx_bytes: data.total_transmitted(),
            rx_packets: data.total_packets_received(),
            tx_packets: data.total_packets_transmitted(),
            rx_errors: data.total_errors_on_received(),
            tx_errors: data.total_errors_on_transmitted(),
            loopback: ifname == "lo0",
        })
        .collect();
    interfaces.sort_by(|left, right| left.ifname.cmp(&right.ifname));
    interfaces
}

/// Mount table with exact `statvfs` capacities.
fn read_mounts() -> Vec<MacosMount> {
    let disks = Disks::new_with_refreshed_list();
    let mut mounts = Vec::with_capacity(disks.list().len());
    for disk in disks.list() {
        let mount_point = disk.mount_point().to_string_lossy().into_owned();
        if mount_point.is_empty() {
            continue;
        }
        let (total, free, available) = statvfs_capacity(&mount_point).unwrap_or((
            disk.total_space(),
            disk.total_space().saturating_sub(disk.available_space()),
            disk.available_space(),
        ));
        mounts.push(MacosMount {
            mount_id: mount_id(&mount_point),
            mount_point,
            fs_type: disk.file_system().to_string_lossy().into_owned(),
            device: disk.name().to_string_lossy().into_owned(),
            total,
            free,
            available,
        });
    }
    mounts
        .sort_by(|left: &MacosMount, right: &MacosMount| left.mount_point.cmp(&right.mount_point));
    mounts
}

/// Exact capacities from `statvfs(2)`; `None` when the mount cannot be
/// stated (the caller falls back to the `sysinfo` figures).
fn statvfs_capacity(mount_point: &str) -> Option<(u64, u64, u64)> {
    let stats = nix::sys::statvfs::statvfs(mount_point.as_bytes()).ok()?;
    let block = stats.fragment_size();
    Some((
        u64::from(stats.blocks()).saturating_mul(block),
        u64::from(stats.blocks_free()).saturating_mul(block),
        u64::from(stats.blocks_available()).saturating_mul(block),
    ))
}

impl TelemetryPort for MacosAdapter {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let now_boot_ns = self.clock.now_boot_ns();
        let adapter_id = &self.manifest.adapter_id;
        if let Some(code) = rejected_discovery(request, adapter_id, self.shut_down, now_boot_ns) {
            return self.book.unavailable_page(request, code);
        }
        if request.page > 0 {
            return self.book.serve_page(request);
        }
        self.discover_snapshot(request, now_boot_ns)
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let now_boot_ns = self.clock.now_boot_ns();
        if let Some(rejected) =
            rejected_plan(plan, &self.manifest.adapter_id, self.shut_down, now_boot_ns)
        {
            return rejected;
        }
        if now_boot_ns >= plan.deadline_boot_ns {
            return RecordFactory::failed_batch(plan, now_boot_ns, "MACOS_DEADLINE_EXPIRED");
        }
        let target = plan_target(plan);
        if !is_macos_group(&target.group) {
            return RecordFactory::failed_batch(plan, now_boot_ns, "MACOS_UNKNOWN_READ_GROUP");
        }
        self.read_group(plan, &target, now_boot_ns)
    }

    fn shutdown(&mut self) {
        self.shut_down = true;
        self.pagers.clear();
        self.book.clear_pending();
    }
}

/// Whether `group` is one of the seven `macos` groups.
fn is_macos_group(group: &str) -> bool {
    matches!(
        group,
        groups::CPU
            | groups::MEMORY
            | groups::PROCESS
            | groups::NET
            | groups::CAPACITY
            | groups::GPU_FAST
            | groups::GPU_HEALTH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_cover_every_collector() {
        for group in [
            groups::CPU,
            groups::MEMORY,
            groups::PROCESS,
            groups::NET,
            groups::CAPACITY,
            groups::GPU_FAST,
            groups::GPU_HEALTH,
        ] {
            assert!(is_macos_group(group), "{group} is not routed");
        }
        assert!(!is_macos_group("fast"));
    }

    #[test]
    fn process_states_have_letters() {
        assert_eq!(process_state(ProcessStatus::Run), 'R');
        assert_eq!(process_state(ProcessStatus::Sleep), 'S');
        assert_eq!(process_state(ProcessStatus::Zombie), 'Z');
        assert_eq!(process_state(ProcessStatus::Dead), 'X');
    }
}
