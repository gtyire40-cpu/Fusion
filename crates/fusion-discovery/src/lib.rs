use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    pub observed_unix_ms: i64,
    pub cpu: CpuCapabilities,
    pub memory: MemoryCapabilities,
    pub network_interfaces: Vec<NetworkInterface>,
    pub primary_local_ip: Option<String>,
    pub master_latency_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuCapabilities {
    pub vendor: String,
    pub model: String,
    pub architecture: String,
    pub physical_cores: Option<u32>,
    pub logical_threads: u32,
    pub max_frequency_mhz: Option<u64>,
    pub utilization_percent: Option<f64>,
    pub temperature_celsius: Option<f64>,
    pub load_average_1: Option<f64>,
    pub load_average_5: Option<f64>,
    pub load_average_15: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryCapabilities {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub addresses: Vec<String>,
    pub connection_type: ConnectionType,
    pub speed_mbps: Option<u64>,
    pub is_up: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Ethernet,
    Wifi,
    Loopback,
    #[default]
    Other,
}

#[derive(Clone, Debug)]
struct SystemPaths {
    proc_root: PathBuf,
    sys_root: PathBuf,
}

impl Default for SystemPaths {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            sys_root: PathBuf::from("/sys"),
        }
    }
}

#[derive(Default)]
pub struct HardwareProbe {
    paths: SystemPaths,
    previous_cpu_times: Option<(u64, u64)>,
}

impl HardwareProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn collect(
        &mut self,
        master: Option<SocketAddr>,
        master_latency_ms: Option<f64>,
    ) -> Result<HardwareSnapshot> {
        let interfaces = collect_network_interfaces(&self.paths.sys_root)?;
        let primary_local_ip = local_ip_for_master(master);
        collect_from_sources(
            &self.paths,
            interfaces,
            primary_local_ip,
            master_latency_ms,
            &mut self.previous_cpu_times,
        )
    }
}

fn collect_from_sources(
    paths: &SystemPaths,
    network_interfaces: Vec<NetworkInterface>,
    primary_local_ip: Option<String>,
    master_latency_ms: Option<f64>,
    previous_cpu_times: &mut Option<(u64, u64)>,
) -> Result<HardwareSnapshot> {
    let cpu_info_path = paths.proc_root.join("cpuinfo");
    let cpu_info = fs::read_to_string(&cpu_info_path)
        .with_context(|| format!("reading {}", cpu_info_path.display()))?;
    let stat_path = paths.proc_root.join("stat");
    let stat = fs::read_to_string(&stat_path)
        .with_context(|| format!("reading {}", stat_path.display()))?;
    let meminfo_path = paths.proc_root.join("meminfo");
    let meminfo = fs::read_to_string(&meminfo_path)
        .with_context(|| format!("reading {}", meminfo_path.display()))?;

    let (total, idle) = parse_cpu_times(&stat)?;
    let utilization_percent = previous_cpu_times.and_then(|(old_total, old_idle)| {
        let total_delta = total.checked_sub(old_total)?;
        let idle_delta = idle.checked_sub(old_idle)?;
        if total_delta == 0 {
            return None;
        }
        Some((total_delta.saturating_sub(idle_delta) as f64 / total_delta as f64) * 100.0)
    });
    *previous_cpu_times = Some((total, idle));

    let mut cpu = parse_cpu_info(&cpu_info)?;
    cpu.utilization_percent = utilization_percent;
    cpu.max_frequency_mhz = read_frequency_mhz(&paths.sys_root);
    cpu.temperature_celsius = read_cpu_temperature(&paths.sys_root);
    (cpu.load_average_1, cpu.load_average_5, cpu.load_average_15) =
        read_load_average(&paths.proc_root);

    Ok(HardwareSnapshot {
        observed_unix_ms: unix_time_ms()?,
        cpu,
        memory: parse_memory_info(&meminfo)?,
        network_interfaces,
        primary_local_ip,
        master_latency_ms: master_latency_ms
            .filter(|latency| latency.is_finite() && *latency >= 0.0),
    })
}

fn parse_cpu_info(contents: &str) -> Result<CpuCapabilities> {
    let mut vendor = None;
    let mut model = None;
    let mut logical_threads = 0_u32;
    let mut physical_cores = BTreeSet::new();
    let mut socket_core_counts = BTreeMap::<String, u32>::new();
    let mut fallback_core_count = None;

    for block in contents.split("\n\n") {
        let mut physical_id = None;
        let mut core_id = None;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "processor" => logical_threads = logical_threads.saturating_add(1),
                "vendor_id" | "CPU implementer" if vendor.is_none() => {
                    vendor = Some(value.to_owned())
                }
                "model name" | "Hardware" | "Processor" if model.is_none() => {
                    model = Some(value.to_owned())
                }
                "physical id" => physical_id = Some(value.to_owned()),
                "core id" => core_id = Some(value.to_owned()),
                "cpu cores" => {
                    if let Ok(count) = value.parse::<u32>() {
                        fallback_core_count = Some(
                            fallback_core_count.map_or(count, |current: u32| current.max(count)),
                        );
                    }
                }
                _ => {}
            }
        }
        if let (Some(socket), Some(core)) = (physical_id.as_ref(), core_id.as_ref()) {
            physical_cores.insert((socket.clone(), core.clone()));
        }
        if let Some(socket) = physical_id {
            if let Some(core_count) = fallback_core_count {
                socket_core_counts
                    .entry(socket)
                    .and_modify(|count| *count = (*count).max(core_count))
                    .or_insert(core_count);
            }
        }
    }

    if logical_threads == 0 {
        logical_threads = std::thread::available_parallelism()
            .context("CPU topology is unavailable")?
            .get()
            .try_into()
            .context("logical CPU count exceeds supported range")?;
    }
    if physical_cores.is_empty() && !socket_core_counts.is_empty() {
        let count = socket_core_counts.values().copied().sum::<u32>();
        if count > 0 {
            physical_cores.insert(("fallback".to_owned(), count.to_string()));
        }
    }
    let physical_cores = if physical_cores.is_empty() {
        fallback_core_count
    } else {
        Some(
            physical_cores
                .len()
                .try_into()
                .context("physical CPU core count exceeds supported range")?,
        )
    };

    Ok(CpuCapabilities {
        vendor: vendor.unwrap_or_else(|| "unknown".to_owned()),
        model: model.unwrap_or_else(|| "unknown".to_owned()),
        architecture: std::env::consts::ARCH.to_owned(),
        physical_cores,
        logical_threads,
        ..CpuCapabilities::default()
    })
}

fn parse_cpu_times(contents: &str) -> Result<(u64, u64)> {
    let line = contents
        .lines()
        .find(|line| line.starts_with("cpu "))
        .context("/proc/stat does not contain aggregate CPU counters")?;
    let counters = line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing aggregate CPU counters")?;
    if counters.len() < 4 {
        bail!("/proc/stat aggregate CPU counters are incomplete");
    }
    // Guest counters are already included in user/nice in /proc/stat.
    let total = counters
        .iter()
        .take(8)
        .copied()
        .fold(0_u64, u64::saturating_add);
    let idle = counters[3].saturating_add(counters.get(4).copied().unwrap_or_default());
    Ok((total, idle))
}

fn parse_memory_info(contents: &str) -> Result<MemoryCapabilities> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let kilobytes = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok());
            if let Some(kilobytes) = kilobytes {
                values.insert(key, kilobytes.saturating_mul(1024));
            }
        }
    }
    let total_bytes = *values
        .get("MemTotal")
        .context("/proc/meminfo does not contain MemTotal")?;
    let available_bytes = values
        .get("MemAvailable")
        .copied()
        .or_else(|| {
            let free = values.get("MemFree").copied()?;
            let buffers = values.get("Buffers").copied().unwrap_or_default();
            let cached = values.get("Cached").copied().unwrap_or_default();
            Some(free.saturating_add(buffers).saturating_add(cached))
        })
        .context("/proc/meminfo does not contain available memory data")?
        .min(total_bytes);
    let free_bytes = *values
        .get("MemFree")
        .context("/proc/meminfo does not contain MemFree")?
        .min(&total_bytes);
    Ok(MemoryCapabilities {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(available_bytes),
        available_bytes,
        free_bytes,
    })
}

fn read_frequency_mhz(sys_root: &Path) -> Option<u64> {
    let candidates = [
        sys_root.join("devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"),
        sys_root.join("devices/system/cpu/cpu0/cpufreq/scaling_max_freq"),
    ];
    candidates
        .iter()
        .find_map(|path| fs::read_to_string(path).ok()?.trim().parse::<u64>().ok())
        .map(|kilohertz| kilohertz / 1000)
}

fn read_cpu_temperature(sys_root: &Path) -> Option<f64> {
    let thermal_zones = sys_root.join("class/thermal");
    if let Some(zones) = sorted_entries(&thermal_zones) {
        for zone in zones {
            let Ok(kind) = fs::read_to_string(zone.join("type")) else {
                continue;
            };
            if is_cpu_sensor(&kind) {
                if let Some(value) = read_millidegrees(&zone.join("temp")) {
                    return Some(value);
                }
            }
        }
    }

    let hwmon_root = sys_root.join("class/hwmon");
    for hwmon in sorted_entries(&hwmon_root).unwrap_or_default() {
        for label in sorted_entries(&hwmon).unwrap_or_default() {
            let Some(name) = label.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(sensor) = name.strip_suffix("_label") else {
                continue;
            };
            let Ok(label_name) = fs::read_to_string(&label) else {
                continue;
            };
            if !is_cpu_sensor(&label_name) {
                continue;
            }
            let input = hwmon.join(format!("{sensor}_input"));
            if let Some(value) = read_millidegrees(&input) {
                return Some(value);
            }
        }
    }
    None
}

fn sorted_entries(path: &Path) -> Option<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();
    Some(entries)
}

fn read_millidegrees(path: &Path) -> Option<f64> {
    let milli_celsius = fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()?;
    let celsius = milli_celsius / 1000.0;
    (celsius.is_finite() && (-20.0..=150.0).contains(&celsius)).then_some(celsius)
}

fn is_cpu_sensor(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["cpu", "package", "pkg", "soc", "core", "tctl", "tdie"]
        .iter()
        .any(|needle| name.contains(needle))
}

fn read_load_average(proc_root: &Path) -> (Option<f64>, Option<f64>, Option<f64>) {
    let Ok(loadavg) = fs::read_to_string(proc_root.join("loadavg")) else {
        return (None, None, None);
    };
    let mut values = loadavg
        .split_whitespace()
        .take(3)
        .map(str::parse::<f64>)
        .map(|value| value.ok().filter(|value| value.is_finite()));
    (
        values.next().flatten(),
        values.next().flatten(),
        values.next().flatten(),
    )
}

fn collect_network_interfaces(sys_root: &Path) -> Result<Vec<NetworkInterface>> {
    let interfaces_path = sys_root.join("class/net");
    let mut interfaces = BTreeMap::<String, NetworkInterface>::new();
    for interface in if_addrs::get_if_addrs().context("enumerating network interfaces")? {
        let entry = interfaces
            .entry(interface.name.clone())
            .or_insert_with(|| NetworkInterface {
                name: interface.name.clone(),
                connection_type: classify_interface(&interface.name, &sys_root.join("class/net")),
                is_up: read_interface_state(&sys_root.join("class/net").join(&interface.name)),
                speed_mbps: read_interface_speed(&sys_root.join("class/net").join(&interface.name)),
                ..NetworkInterface::default()
            });
        entry.addresses.push(interface.ip().to_string());
    }
    if interfaces.is_empty() && interfaces_path.exists() {
        for path in sorted_entries(&interfaces_path).unwrap_or_default() {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            interfaces.insert(
                name.to_owned(),
                NetworkInterface {
                    name: name.to_owned(),
                    connection_type: classify_interface(name, &interfaces_path),
                    is_up: read_interface_state(&path),
                    speed_mbps: read_interface_speed(&path),
                    ..NetworkInterface::default()
                },
            );
        }
    }
    for interface in interfaces.values_mut() {
        interface.addresses.sort();
        interface.addresses.dedup();
    }
    Ok(interfaces.into_values().collect())
}

fn classify_interface(name: &str, interfaces_path: &Path) -> ConnectionType {
    let path = interfaces_path.join(name);
    if name == "lo"
        || fs::read_to_string(path.join("type")).is_ok_and(|value| value.trim() == "772")
    {
        ConnectionType::Loopback
    } else if name.starts_with("wl") || path.join("wireless").exists() {
        ConnectionType::Wifi
    } else if name.starts_with("en") || name.starts_with("eth") {
        ConnectionType::Ethernet
    } else {
        ConnectionType::Other
    }
}

fn read_interface_state(path: &Path) -> bool {
    fs::read_to_string(path.join("operstate"))
        .is_ok_and(|state| matches!(state.trim(), "up" | "unknown"))
}

fn read_interface_speed(path: &Path) -> Option<u64> {
    fs::read_to_string(path.join("speed"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|speed| *speed > 0)
}

fn local_ip_for_master(master: Option<SocketAddr>) -> Option<String> {
    let master = master?;
    let wildcard = match master.ip() {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(wildcard).ok()?;
    socket.connect(master).ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn unix_time_ms() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .context("system clock timestamp exceeds supported range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_ram_and_optional_values_from_simulated_proc_and_sys() -> Result<()> {
        let root = tempfile::tempdir()?;
        let proc_root = root.path().join("proc");
        let sys_root = root.path().join("sys");
        fs::create_dir_all(&proc_root)?;
        fs::create_dir_all(sys_root.join("devices/system/cpu/cpu0/cpufreq"))?;
        fs::create_dir_all(sys_root.join("class/thermal/thermal_zone0"))?;
        fs::write(
            proc_root.join("cpuinfo"),
            "processor : 0\nvendor_id : GenuineIntel\nmodel name : Simulated CPU\nphysical id : 0\ncore id : 0\ncpu cores : 2\n\nprocessor : 1\nvendor_id : GenuineIntel\nmodel name : Simulated CPU\nphysical id : 0\ncore id : 1\ncpu cores : 2\n",
        )?;
        fs::write(proc_root.join("stat"), "cpu 100 0 50 850 0 0 0 0 100 100\n")?;
        fs::write(
            proc_root.join("meminfo"),
            "MemTotal: 1048576 kB\nMemFree: 262144 kB\nMemAvailable: 524288 kB\n",
        )?;
        fs::write(proc_root.join("loadavg"), "1.25 0.75 0.50 1/100 10\n")?;
        fs::write(
            sys_root.join("devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"),
            "3200000\n",
        )?;
        fs::write(
            sys_root.join("class/thermal/thermal_zone0/type"),
            "x86_pkg_temp\n",
        )?;
        fs::write(sys_root.join("class/thermal/thermal_zone0/temp"), "45000\n")?;

        let paths = SystemPaths {
            proc_root,
            sys_root,
        };
        let mut previous = None;
        let network = vec![NetworkInterface {
            name: "eth-test".to_owned(),
            addresses: vec!["192.0.2.10".to_owned()],
            connection_type: ConnectionType::Ethernet,
            speed_mbps: Some(1000),
            is_up: true,
        }];
        let first = collect_from_sources(
            &paths,
            network.clone(),
            Some("192.0.2.10".to_owned()),
            Some(2.5),
            &mut previous,
        )?;
        assert_eq!(first.cpu.vendor, "GenuineIntel");
        assert_eq!(first.cpu.model, "Simulated CPU");
        assert_eq!(first.cpu.physical_cores, Some(2));
        assert_eq!(first.cpu.logical_threads, 2);
        assert_eq!(first.cpu.max_frequency_mhz, Some(3200));
        assert_eq!(first.cpu.utilization_percent, None);
        assert_eq!(first.cpu.temperature_celsius, Some(45.0));
        assert_eq!(first.memory.total_bytes, 1_073_741_824);
        assert_eq!(first.memory.available_bytes, 536_870_912);
        assert_eq!(first.memory.used_bytes, 536_870_912);
        assert_eq!(first.cpu.load_average_1, Some(1.25));
        assert_eq!(first.network_interfaces, network);
        assert_eq!(first.primary_local_ip.as_deref(), Some("192.0.2.10"));

        fs::write(
            paths.proc_root.join("stat"),
            "cpu 200 0 100 900 0 0 0 0 200 200\n",
        )?;
        let second = collect_from_sources(&paths, Vec::new(), None, None, &mut previous)?;
        assert_eq!(second.cpu.utilization_percent, Some(75.0));
        assert_eq!(second.cpu.temperature_celsius, Some(45.0));
        Ok(())
    }

    #[test]
    fn unavailable_optional_hardware_data_stays_absent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let proc_root = root.path().join("proc");
        let sys_root = root.path().join("sys");
        fs::create_dir_all(&proc_root)?;
        fs::create_dir_all(&sys_root)?;
        fs::write(
            proc_root.join("cpuinfo"),
            "processor : 0\nmodel name : minimal\n",
        )?;
        fs::write(proc_root.join("stat"), "cpu 10 0 10 80\n")?;
        fs::write(
            proc_root.join("meminfo"),
            "MemTotal: 1000 kB\nMemFree: 100 kB\n",
        )?;

        let mut previous = None;
        let snapshot = collect_from_sources(
            &SystemPaths {
                proc_root,
                sys_root,
            },
            Vec::new(),
            None,
            Some(f64::NAN),
            &mut previous,
        )?;
        assert_eq!(snapshot.cpu.max_frequency_mhz, None);
        assert_eq!(snapshot.cpu.temperature_celsius, None);
        assert_eq!(snapshot.cpu.load_average_1, None);
        assert_eq!(snapshot.master_latency_ms, None);
        assert_eq!(snapshot.memory.available_bytes, snapshot.memory.free_bytes);
        Ok(())
    }
}
