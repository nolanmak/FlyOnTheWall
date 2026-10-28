//! Content-free native observations for local-summary admission.

use fotw_summarize::local_model::{HardwareSnapshot, MemoryPressure};
use std::path::Path;

/// A fresh observation, never a model-quality or hardware qualification badge.
#[derive(Debug)]
pub struct DeviceProfile {
    /// Memory/platform input to the shared admission rules. Cold-load probes
    /// start with zero owned model bytes; a service lease supplies residency.
    pub memory: HardwareSnapshot,
    /// Logical CPU count, for compute planning independently of installed RAM.
    pub logical_cpus: Option<u32>,
    /// Performance-core count when this Mac reports one.
    pub performance_cpus: Option<u32>,
    /// Available disk bytes on the selected installation volume.
    pub available_disk_bytes: Option<u64>,
    /// OS version for runtime compatibility checks.
    pub macos_version: Option<String>,
}

#[derive(Default)]
struct RawMemory {
    arm64: bool,
    version: Option<String>,
    physical: Option<u64>,
    page_bytes: Option<u64>,
    free_pages: Option<u64>,
    inactive_pages: Option<u64>,
    pressure: Option<u32>,
}

fn snapshot(raw: &RawMemory) -> HardwareSnapshot {
    let physical = raw.physical.filter(|n| *n > 0);
    // HOST_VM_INFO64.free_count already includes speculative_count. Do not
    // add it again, nor add purgeable/compressor/RSS counters on top. Inactive
    // pages are an estimate of reclaimable headroom, not guaranteed free RAM;
    // the reserve, live pressure checks and measured qualification still apply.
    let available = raw
        .free_pages
        .zip(raw.inactive_pages)
        .and_then(|(free, inactive)| free.checked_add(inactive))
        .zip(raw.page_bytes.filter(|n| *n > 0))
        .and_then(|(pages, size)| pages.checked_mul(size))
        .filter(|bytes| physical.is_some_and(|physical| *bytes <= physical));
    let supported = raw.arm64 && raw.version.as_deref().is_some_and(supported_version);
    HardwareSnapshot {
        supported_platform: supported,
        physical_bytes: physical,
        available_bytes: available,
        owned_resident_bytes: 0,
        pressure: match raw.pressure {
            Some(1) => Some(MemoryPressure::Normal),
            Some(2) => Some(MemoryPressure::Warning),
            Some(4) => Some(MemoryPressure::Critical),
            _ => None,
        },
    }
}

fn supported_version(version: &str) -> bool {
    let parts: Option<Vec<u32>> = version.split('.').map(|part| part.parse().ok()).collect();
    parts.is_some_and(|p| p.len() >= 2 && p.len() <= 3 && (p[0], p[1]) >= (14, 4))
}

/// Observe system metadata and the installation volume. Missing/unknown fields
/// remain unknown and shared admission fails closed; no shell or network call.
#[must_use]
pub fn probe(installation_directory: &Path) -> DeviceProfile {
    let (raw, logical_cpus, performance_cpus) = native::observe();
    DeviceProfile {
        memory: snapshot(&raw),
        logical_cpus,
        performance_cpus,
        available_disk_bytes: disk_available(installation_directory),
        macos_version: raw.version,
    }
}

pub(super) fn disk_available(directory: &Path) -> Option<u64> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)
        .ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    if stats.f_frsize == 0 {
        return None;
    }
    u64::try_from(u128::from(stats.f_bavail).checked_mul(u128::from(stats.f_frsize))?).ok()
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::RawMemory;
    pub(super) fn observe() -> (RawMemory, Option<u32>, Option<u32>) {
        (RawMemory::default(), None, None)
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::RawMemory;
    use std::ffi::CStr;

    // Public Darwin declarations from mach_init.h/mach_port.h. libc omits
    // these functions and deprecates its mach_host_self wrapper; keep the ABI
    // narrow here instead of importing an unrelated full Mach abstraction.
    unsafe extern "C" {
        fn mach_host_self() -> libc::mach_port_t;
        fn host_page_size(
            host: libc::mach_port_t,
            size: *mut libc::vm_size_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
        static mach_task_self_: libc::mach_port_t;
    }

    fn number<T: Copy + Default>(name: &CStr) -> Option<T> {
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>();
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (result == 0 && size == std::mem::size_of::<T>()).then_some(value)
    }
    fn version() -> Option<String> {
        let mut bytes = [0_u8; 64];
        let mut size = bytes.len();
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || size == 0 || size > bytes.len() || bytes[size - 1] != 0 {
            return None;
        }
        std::str::from_utf8(&bytes[..size - 1])
            .ok()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
    fn pages() -> Option<(u64, u64, u64)> {
        let host = unsafe { mach_host_self() };
        if host == 0 {
            return None;
        }
        let mut page: libc::vm_size_t = 0;
        // A newer SDK/libc can describe more fields than this kernel returns.
        // Zero the all-integer record and consume only its stable first three
        // natural_t counters; never assume the whole latest record was filled.
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let result = unsafe {
            let size_result = host_page_size(host, &raw mut page);
            let stats_result = libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&raw mut stats).cast(),
                &raw mut count,
            );
            mach_port_deallocate(mach_task_self_, host);
            (size_result, stats_result)
        };
        if result != (libc::KERN_SUCCESS, libc::KERN_SUCCESS)
            || !(3..=libc::HOST_VM_INFO64_COUNT).contains(&count)
            || page == 0
        {
            return None;
        }
        Some((
            page as u64,
            u64::from(stats.free_count),
            u64::from(stats.inactive_count),
        ))
    }
    pub(super) fn observe() -> (RawMemory, Option<u32>, Option<u32>) {
        let pages = pages();
        let raw = RawMemory {
            arm64: number::<u32>(c"hw.optional.arm64") == Some(1),
            version: version(),
            physical: number(c"hw.memsize"),
            page_bytes: pages.map(|p| p.0),
            free_pages: pages.map(|p| p.1),
            inactive_pages: pages.map(|p| p.2),
            pressure: number(c"kern.memorystatus_vm_pressure_level"),
        };
        (
            raw,
            number::<u32>(c"hw.logicalcpu").filter(|n| *n > 0),
            number::<u32>(c"hw.perflevel0.physicalcpu").filter(|n| *n > 0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fotw_summarize::local_model::{AdmissionError, GIB, memory_budget};
    fn valid() -> RawMemory {
        RawMemory {
            arm64: true,
            version: Some("14.4.1".into()),
            physical: Some(16 * GIB),
            page_bytes: Some(16384),
            free_pages: Some(2 * GIB / 16384),
            inactive_pages: Some(3 * GIB / 16384),
            pressure: Some(1),
        }
    }
    #[test]
    fn counts_raw_free_once_and_excludes_other_vm_categories() {
        let observed = snapshot(&valid());
        assert_eq!(observed.available_bytes, Some(5 * GIB));
        assert_eq!(observed.physical_bytes, Some(16 * GIB));
        assert!(observed.supported_platform);
        assert_eq!(observed.owned_resident_bytes, 0);
    }
    #[test]
    fn pressure_is_not_inferred_from_free_ram() {
        let mut raw = valid();
        for (value, expected) in [
            (1, Some(MemoryPressure::Normal)),
            (2, Some(MemoryPressure::Warning)),
            (4, Some(MemoryPressure::Critical)),
            (0, None),
            (3, None),
            (99, None),
        ] {
            raw.pressure = Some(value);
            let result = snapshot(&raw);
            assert_eq!(result.pressure, expected);
            if value == 2 || value == 4 {
                assert_eq!(
                    memory_budget(result, None),
                    Err(AdmissionError::MemoryPressure)
                );
            }
        }
    }
    #[test]
    fn missing_inconsistent_and_overflowed_counters_remain_unknown() {
        let mut raw = valid();
        raw.page_bytes = None;
        assert!(snapshot(&raw).available_bytes.is_none());
        raw.page_bytes = Some(16384);
        raw.free_pages = Some(u64::MAX);
        assert!(snapshot(&raw).available_bytes.is_none());
        raw.free_pages = Some(17 * GIB / 16384);
        assert!(snapshot(&raw).available_bytes.is_none());
        raw = valid();
        raw.physical = Some(0);
        assert!(snapshot(&raw).physical_bytes.is_none());
    }
    #[test]
    fn platform_check_uses_architecture_and_os_version_not_ram_tier() {
        let mut raw = valid();
        raw.physical = Some(512 * GIB);
        raw.version = Some("26.4".into());
        assert!(snapshot(&raw).supported_platform);
        raw.arm64 = false;
        assert!(!snapshot(&raw).supported_platform);
        raw.arm64 = true;
        raw.version = Some("14.3.9".into());
        assert!(!snapshot(&raw).supported_platform);
        raw.version = Some("garbage".into());
        assert!(!snapshot(&raw).supported_platform);
    }
    #[test]
    fn disk_observation_requires_an_existing_regular_installation_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(disk_available(root.path()).is_some_and(|n| n > 0));
        assert!(disk_available(&root.path().join("missing")).is_none());
        let file = root.path().join("file");
        std::fs::write(&file, b"synthetic").unwrap();
        assert!(disk_available(&file).is_none());
        let link = root.path().join("link");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        assert!(disk_available(&link).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_native_probe_reads_metadata_without_runtime_or_credentials() {
        let result = probe(Path::new("/private/tmp"));
        assert!(result.memory.physical_bytes.is_some_and(|n| n > 0));
        assert!(
            result.memory.available_bytes.is_some(),
            "supported Mach VM prefix must yield headroom"
        );
        assert!(result.logical_cpus.is_some_and(|n| n > 0));
        assert!(result.available_disk_bytes.is_some_and(|n| n > 0));
        assert!(result.macos_version.is_some());
    }
}
