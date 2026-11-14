//! Physical-host capacity facts. The coordinator admits against declared
//! capacity summed per `physical_host_id` (VMs fold onto their host there);
//! this module keeps the declaration honest locally: a worker never advertises
//! more CPU, memory or disk than the machine it observes, and never starts an
//! assignment that would exceed its declaration alongside running attempts.
use anyhow::{Context, Result};
use dacs_protocol::Resources;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Observed locally from the kernel; never taken from coordinator messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacity {
    /// Schedulable logical CPUs for this process (respects affinity/cgroups).
    pub logical_cpus: u32,
    pub memory_mb: u64,
    /// Total size of the filesystem holding the workspaces.
    pub disk_mb: u64,
}

pub fn observe(workspaces: &Path) -> Result<HostCapacity> {
    let logical_cpus = std::thread::available_parallelism()
        .context("cannot observe host CPU count")?
        .get();
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let memory = system.total_memory();
    anyhow::ensure!(memory > 0, "cannot observe host memory");
    let disk = fs2::total_space(workspaces).context("cannot observe workspace disk")?;
    Ok(HostCapacity {
        logical_cpus: u32::try_from(logical_cpus).unwrap_or(u32::MAX),
        memory_mb: memory / (1024 * 1024),
        disk_mb: disk / (1024 * 1024),
    })
}

/// Refuse a declaration the physical host cannot back. Agent slots have no
/// physical unit; every agent's OMP, tools and builds draw on the CPU, memory
/// and disk checked here.
pub fn check_declared(declared: &Resources, host: &HostCapacity) -> Result<()> {
    let cpu_millis = u64::from(host.logical_cpus) * 1000;
    let mut excess = Vec::new();
    if u64::from(declared.cpu_millis) > cpu_millis {
        excess.push(format!(
            "cpu_millis {} > {cpu_millis} ({} logical CPUs)",
            declared.cpu_millis, host.logical_cpus
        ));
    }
    if declared.memory_mb > host.memory_mb {
        excess.push(format!(
            "memory_mb {} > {}",
            declared.memory_mb, host.memory_mb
        ));
    }
    if declared.disk_mb > host.disk_mb {
        excess.push(format!(
            "disk_mb {} > {} (workspace filesystem)",
            declared.disk_mb, host.disk_mb
        ));
    }
    anyhow::ensure!(
        excess.is_empty(),
        "declared worker capacity exceeds the observed physical host: {}",
        excess.join(", ")
    );
    Ok(())
}

/// Whether `next` fits beside the reservations already executing locally.
/// Overflow is treated as not fitting.
pub fn fits<'a>(
    running: impl IntoIterator<Item = &'a Resources>,
    next: &Resources,
    declared: &Resources,
) -> bool {
    let mut total = [
        u64::from(next.agent_slots),
        u64::from(next.cpu_millis),
        next.memory_mb,
        next.disk_mb,
    ];
    for held in running {
        for (sum, value) in total.iter_mut().zip([
            u64::from(held.agent_slots),
            u64::from(held.cpu_millis),
            held.memory_mb,
            held.disk_mb,
        ]) {
            match sum.checked_add(value) {
                Some(value) => *sum = value,
                None => return false,
            }
        }
    }
    total
        .into_iter()
        .zip([
            u64::from(declared.agent_slots),
            u64::from(declared.cpu_millis),
            declared.memory_mb,
            declared.disk_mb,
        ])
        .all(|(used, limit)| used <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resources(agent_slots: u32, cpu_millis: u32, memory_mb: u64, disk_mb: u64) -> Resources {
        Resources {
            agent_slots,
            cpu_millis,
            memory_mb,
            disk_mb,
        }
    }

    #[test]
    fn declarations_beyond_the_physical_host_are_refused() {
        let host = HostCapacity {
            logical_cpus: 8,
            memory_mb: 16_384,
            disk_mb: 500_000,
        };
        assert!(check_declared(&resources(4, 8000, 16_384, 500_000), &host).is_ok());
        for declared in [
            resources(1, 8001, 1, 1),
            resources(1, 1000, 16_385, 1),
            resources(1, 1000, 1, 500_001),
        ] {
            let error = check_declared(&declared, &host).unwrap_err().to_string();
            assert!(
                error.contains("exceeds the observed physical host"),
                "{error}"
            );
        }
    }

    #[test]
    fn observed_host_is_nonzero() {
        let directory = tempfile::tempdir().unwrap();
        let host = observe(directory.path()).unwrap();
        assert!(host.logical_cpus > 0 && host.memory_mb > 0 && host.disk_mb > 0);
    }

    #[test]
    fn local_fit_counts_every_running_reservation() {
        let declared = resources(2, 2000, 1024, 1024);
        let one = resources(1, 1000, 512, 512);
        assert!(fits([], &one, &declared));
        assert!(fits([&one], &one, &declared));
        assert!(!fits([&one, &one], &one, &declared));
        assert!(!fits([], &resources(1, 2001, 1, 1), &declared));
        assert!(!fits([&resources(0, 0, u64::MAX, 0)], &one, &declared));
    }
}
