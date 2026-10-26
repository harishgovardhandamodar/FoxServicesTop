//! Apple CPU tick counters from `host_processor_info(2)`.
//!
//! macOS has no `/proc/stat`: per-CPU user/nice/system/idle ticks come from
//! the Mach host interface instead. The counters are cumulative since boot
//! and in the kernel's own tick unit; only their deltas matter, so they feed
//! the CPU collector exactly like the Linux tick fields. The four
//! Linux-only states (iowait/irq/softirq/steal) are emitted as constant zero
//! counters by the catalog: this kernel has no such states.
//!
//! `unsafe` is contained in [`cpu_times`]; every block states why it is
//! sound. No Mach port is leaked: the info array is always `vm_deallocate`d.
#![allow(unsafe_code)]

/// User/nice/system/idle ticks of one CPU (or the aggregate), cumulative
/// since boot in the kernel tick unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
}

impl CpuTimes {
    fn accumulate(&mut self, other: &Self) {
        self.user = self.user.saturating_add(other.user);
        self.nice = self.nice.saturating_add(other.nice);
        self.system = self.system.saturating_add(other.system);
        self.idle = self.idle.saturating_add(other.idle);
    }
}

/// Aggregate plus per-CPU times, in kernel enumeration order (`cpu0` is
/// index 0). Returns `None` when the Mach call fails.
///
/// `libc` deprecates `mach_host_self` in favor of the `mach2` crate, but
/// `mach2` has no `host_processor_info` binding, so this module keeps `libc`
/// as its single Mach source instead of mixing two.
#[allow(deprecated)]
pub(crate) fn cpu_times() -> Option<(CpuTimes, Vec<CpuTimes>)> {
    // SAFETY: `mach_host_self` takes no arguments and returns a send right
    // to the host port owned by this task; no memory is shared.
    let host = unsafe { libc::mach_host_self() };
    let mut cpu_count: u32 = 0;
    let mut info: *mut libc::integer_t = std::ptr::null_mut();
    let mut info_count: u32 = 0;
    // SAFETY: all out-parameters point to live stack slots or a NULL pointer
    // the kernel allocates for (`info`); the flavor constant is valid and
    // the returned array is `vm_deallocate`d below before returning.
    let status = unsafe {
        libc::host_processor_info(
            host,
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut cpu_count,
            &mut info,
            &mut info_count,
        )
    };
    if status != libc::KERN_SUCCESS || info.is_null() || cpu_count == 0 {
        return None;
    }
    let per_cpu = CpuTimes::STRIDE
        .checked_mul(cpu_count as usize)
        .filter(|stride| u32::try_from(*stride).is_ok_and(|narrow| narrow <= info_count))
        .map(|stride| unsafe { CpuTimes::read_times(info, stride) });
    // SAFETY: `info` is the array this exact call allocated with
    // `info_count` entries of `integer_t`; deallocating it exactly once
    // balances the kernel allocation even on the parse-failure path.
    unsafe {
        libc::vm_deallocate(
            libc::mach_task_self(),
            info as libc::vm_address_t,
            info_count as libc::vm_size_t * size_of::<libc::integer_t>() as libc::vm_size_t,
        );
    }
    let per_cpu = per_cpu?;
    let mut aggregate = CpuTimes::default();
    for cpu in &per_cpu {
        aggregate.accumulate(cpu);
    }
    Some((aggregate, per_cpu))
}

impl CpuTimes {
    /// `integer_t` entries per CPU in the `PROCESSOR_CPU_LOAD_INFO` array.
    const STRIDE: usize = libc::CPU_STATE_MAX as usize;

    /// Copies `stride` CPUs starting at `base` (bounds checked by caller).
    ///
    /// # Safety
    ///
    /// `base` must point to at least `stride` readable `integer_t` entries
    /// of a live `host_processor_info` array.
    unsafe fn read_times(base: *mut libc::integer_t, stride: usize) -> Vec<Self> {
        // `CPU_STATE_USER/SYSTEM/IDLE/NICE` are the frozen Mach ABI order
        // 0/1/2/3 (SDK `mach/machine.h`, unchanged since 1988).
        let count = stride / Self::STRIDE;
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            // SAFETY: `index * STRIDE + state` stays inside the `stride`
            // entries the caller guarantees; the kernel wrote plain
            // `integer_t` ticks there, so aligned reads are sound.
            let ticks = unsafe {
                let at = base.add(index * Self::STRIDE);
                [
                    at.add(0).read(),
                    at.add(1).read(),
                    at.add(2).read(),
                    at.add(3).read(),
                ]
            };
            out.push(Self {
                user: non_negative(ticks[0]),
                system: non_negative(ticks[1]),
                idle: non_negative(ticks[2]),
                nice: non_negative(ticks[3]),
            });
        }
        out
    }
}

/// Tick counts are never negative; a corrupt read clamps to zero (the
/// counter logic treats the step as a gap, never as negative use).
fn non_negative(value: libc::integer_t) -> u64 {
    u64::try_from(value).unwrap_or(0)
}
