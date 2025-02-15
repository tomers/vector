use crate::{
    config::{DataType, SourceConfig, SourceContext, SourceDescription},
    event::{
        metric::{Metric, MetricKind, MetricValue},
        Event,
    },
    internal_events::HostMetricsEventReceived,
    shutdown::ShutdownSignal,
    Pipeline,
};
use chrono::{DateTime, Utc};
use futures::{stream, SinkExt, StreamExt};
use glob::{Pattern, PatternError};
#[cfg(target_os = "macos")]
use heim::memory::os::macos::MemoryExt;
#[cfg(not(target_os = "windows"))]
use heim::memory::os::SwapExt;
#[cfg(target_os = "windows")]
use heim::net::os::windows::IoCountersExt;
#[cfg(not(target_os = "windows"))]
use heim::units::ratio::ratio;
#[cfg(target_os = "linux")]
use heim::{
    cpu::os::linux::CpuTimeExt, memory::os::linux::MemoryExt, net::os::linux::IoCountersExt,
};
use heim::{
    units::{information::byte, time::second},
    Error,
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use shared::btreemap;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tokio::time;
use tokio_stream::wrappers::IntervalStream;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Collector {
    Cpu,
    Disk,
    Filesystem,
    Load,
    Host,
    Memory,
    Network,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FilterList {
    includes: Option<Vec<PatternWrapper>>,
    excludes: Option<Vec<PatternWrapper>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DiskConfig {
    #[serde(default)]
    devices: FilterList,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct FilesystemConfig {
    #[serde(default)]
    devices: FilterList,
    #[serde(default)]
    filesystems: FilterList,
    #[serde(default)]
    mountpoints: FilterList,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct NetworkConfig {
    #[serde(default)]
    devices: FilterList,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Namespace(Option<String>);

impl Default for Namespace {
    fn default() -> Self {
        Self(Some("host".into()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostMetricsConfig {
    #[serde(default = "default_scrape_interval")]
    scrape_interval_secs: u64,

    collectors: Option<Vec<Collector>>,
    #[serde(default)]
    namespace: Namespace,

    #[serde(default)]
    disk: DiskConfig,
    #[serde(default)]
    filesystem: FilesystemConfig,
    #[serde(default)]
    network: NetworkConfig,
}

const fn default_scrape_interval() -> u64 {
    15
}

inventory::submit! {
    SourceDescription::new::<HostMetricsConfig>("host_metrics")
}

impl_generate_config_from_default!(HostMetricsConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "host_metrics")]
impl SourceConfig for HostMetricsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        init_roots();

        let mut config = self.clone();
        config.namespace.0 = config.namespace.0.filter(|namespace| !namespace.is_empty());

        Ok(Box::pin(config.run(cx.out, cx.shutdown)))
    }

    fn output_type(&self) -> DataType {
        DataType::Metric
    }

    fn source_type(&self) -> &'static str {
        "host_metrics"
    }
}

impl HostMetricsConfig {
    async fn run(self, out: Pipeline, shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut out =
            out.sink_map_err(|error| error!(message = "Error sending host metrics.", %error));

        let duration = time::Duration::from_secs(self.scrape_interval_secs);
        let mut interval = IntervalStream::new(time::interval(duration)).take_until(shutdown);
        while interval.next().await.is_some() {
            let metrics = self.capture_metrics().await;
            out.send_all(&mut stream::iter(metrics).map(Ok)).await?;
        }

        Ok(())
    }

    fn has_collector(&self, collector: Collector) -> bool {
        match &self.collectors {
            None => true,
            Some(collectors) => collectors.iter().any(|&c| c == collector),
        }
    }

    async fn capture_metrics(&self) -> impl Iterator<Item = Event> {
        let hostname = crate::get_hostname();
        let mut metrics = Vec::new();
        if self.has_collector(Collector::Cpu) {
            metrics.extend(add_collector("cpu", self.cpu_metrics().await));
        }
        if self.has_collector(Collector::Disk) {
            metrics.extend(add_collector("disk", self.disk_metrics().await));
        }
        if self.has_collector(Collector::Filesystem) {
            metrics.extend(add_collector("filesystem", self.filesystem_metrics().await));
        }
        if self.has_collector(Collector::Load) {
            metrics.extend(add_collector("load", self.loadavg_metrics().await));
        }
        if self.has_collector(Collector::Host) {
            metrics.extend(add_collector("host", self.host_metrics().await));
        }
        if self.has_collector(Collector::Memory) {
            metrics.extend(add_collector("memory", self.memory_metrics().await));
            metrics.extend(add_collector("memory", self.swap_metrics().await));
        }
        if self.has_collector(Collector::Network) {
            metrics.extend(add_collector("network", self.network_metrics().await));
        }
        if let Ok(hostname) = &hostname {
            for metric in &mut metrics {
                metric.insert_tag("host".into(), hostname.into());
            }
        }
        emit!(HostMetricsEventReceived {
            count: metrics.len()
        });
        metrics.into_iter().map(Into::into)
    }

    pub async fn cpu_metrics(&self) -> Vec<Metric> {
        match heim::cpu::times().await {
            Ok(times) => {
                times
                    .filter_map(|result| filter_result(result, "Failed to load/parse CPU time."))
                    .enumerate()
                    .map(|(index, times)| {
                        let timestamp = Utc::now();
                        let name = "cpu_seconds_total";
                        stream::iter(
                            vec![
                                self.counter(
                                    name,
                                    timestamp,
                                    times.idle().get::<second>(),
                                    btreemap! { "mode" => "idle", "cpu" => index.to_string() },
                                ),
                                #[cfg(target_os = "linux")]
                                self.counter(
                                    name,
                                    timestamp,
                                    times.nice().get::<second>(),
                                    btreemap! { "mode" => "nice", "cpu" => index.to_string() },
                                ),
                                self.counter(
                                    name,
                                    timestamp,
                                    times.system().get::<second>(),
                                    btreemap! { "mode" => "system", "cpu" => index.to_string() },
                                ),
                                self.counter(
                                    name,
                                    timestamp,
                                    times.user().get::<second>(),
                                    btreemap! { "mode" => "user", "cpu" => index.to_string() },
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load CPU times.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    pub async fn memory_metrics(&self) -> Vec<Metric> {
        match heim::memory::memory().await {
            Ok(memory) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "memory_total_bytes",
                        timestamp,
                        memory.total().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "memory_free_bytes",
                        timestamp,
                        memory.free().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "memory_available_bytes",
                        timestamp,
                        memory.available().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    self.gauge(
                        "memory_active_bytes",
                        timestamp,
                        memory.active().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_buffers_bytes",
                        timestamp,
                        memory.buffers().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_cached_bytes",
                        timestamp,
                        memory.cached().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_shared_bytes",
                        timestamp,
                        memory.shared().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "linux")]
                    self.gauge(
                        "memory_used_bytes",
                        timestamp,
                        memory.used().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "macos")]
                    self.gauge(
                        "memory_inactive_bytes",
                        timestamp,
                        memory.inactive().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(target_os = "macos")]
                    self.gauge(
                        "memory_wired_bytes",
                        timestamp,
                        memory.wire().get::<byte>() as f64,
                        btreemap! {},
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load memory info.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    pub async fn swap_metrics(&self) -> Vec<Metric> {
        match heim::memory::swap().await {
            Ok(swap) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "memory_swap_free_bytes",
                        timestamp,
                        swap.free().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "memory_swap_total_bytes",
                        timestamp,
                        swap.total().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "memory_swap_used_bytes",
                        timestamp,
                        swap.used().get::<byte>() as f64,
                        btreemap! {},
                    ),
                    #[cfg(not(target_os = "windows"))]
                    self.counter(
                        "memory_swapped_in_bytes_total",
                        timestamp,
                        swap.sin().map(|swap| swap.get::<byte>()).unwrap_or(0) as f64,
                        btreemap! {},
                    ),
                    #[cfg(not(target_os = "windows"))]
                    self.counter(
                        "memory_swapped_out_bytes_total",
                        timestamp,
                        swap.sout().map(|swap| swap.get::<byte>()).unwrap_or(0) as f64,
                        btreemap! {},
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load swap info.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    pub async fn loadavg_metrics(&self) -> Vec<Metric> {
        #[cfg(unix)]
        let result = match heim::cpu::os::unix::loadavg().await {
            Ok(loadavg) => {
                let timestamp = Utc::now();
                vec![
                    self.gauge(
                        "load1",
                        timestamp,
                        loadavg.0.get::<ratio>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "load5",
                        timestamp,
                        loadavg.1.get::<ratio>() as f64,
                        btreemap! {},
                    ),
                    self.gauge(
                        "load15",
                        timestamp,
                        loadavg.2.get::<ratio>() as f64,
                        btreemap! {},
                    ),
                ]
            }
            Err(error) => {
                error!(message = "Failed to load load average info.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        };
        #[cfg(not(unix))]
        let result = vec![];

        result
    }

    pub async fn host_metrics(&self) -> Vec<Metric> {
        let mut metrics = Vec::new();
        match heim::host::uptime().await {
            Ok(time) => {
                let timestamp = Utc::now();
                metrics.push(self.gauge(
                    "uptime",
                    timestamp,
                    time.get::<second>() as f64,
                    BTreeMap::default(),
                ));
            }
            Err(error) => {
                error!(message = "Failed to load host uptime info.", %error, internal_log_rate_secs = 60);
            }
        }

        match heim::host::boot_time().await {
            Ok(time) => {
                let timestamp = Utc::now();
                metrics.push(self.gauge(
                    "boot_time",
                    timestamp,
                    time.get::<second>() as f64,
                    BTreeMap::default(),
                ));
            }
            Err(error) => {
                error!(message = "Failed to load host boot time info.", %error, internal_log_rate_secs = 60);
            }
        }

        metrics
    }

    pub async fn network_metrics(&self) -> Vec<Metric> {
        match heim::net::io_counters().await {
            Ok(counters) => {
                counters
                    .filter_map(|result| {
                        filter_result(result, "Failed to load/parse network data.")
                    })
                    // The following pair should be possible to do in one
                    // .filter_map, but it results in a strange "one type is
                    // more general than the other" error.
                    .map(|counter| {
                        self.network
                            .devices
                            .contains_str(Some(counter.interface()))
                            .then(|| counter)
                    })
                    .filter_map(|counter| async { counter })
                    .map(|counter| {
                        let timestamp = Utc::now();
                        let interface = counter.interface();
                        stream::iter(
                            vec![
                                self.counter(
                                    "network_receive_bytes_total",
                                    timestamp,
                                    counter.bytes_recv().get::<byte>() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                self.counter(
                                    "network_receive_errs_total",
                                    timestamp,
                                    counter.errors_recv() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                self.counter(
                                    "network_receive_packets_total",
                                    timestamp,
                                    counter.packets_recv() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                self.counter(
                                    "network_transmit_bytes_total",
                                    timestamp,
                                    counter.bytes_sent().get::<byte>() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                self.counter(
                                    "network_transmit_errs_total",
                                    timestamp,
                                    counter.errors_sent() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                #[cfg(any(target_os = "linux", target_os = "windows"))]
                                self.counter(
                                    "network_transmit_packets_drop_total",
                                    timestamp,
                                    counter.drop_sent() as f64,
                                    btreemap! { "device" => interface },
                                ),
                                #[cfg(any(target_os = "linux", target_os = "windows"))]
                                self.counter(
                                    "network_transmit_packets_total",
                                    timestamp,
                                    counter.packets_sent() as f64,
                                    btreemap! { "device" => interface },
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load network I/O counters.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    pub async fn filesystem_metrics(&self) -> Vec<Metric> {
        match heim::disk::partitions().await {
            Ok(partitions) => {
                partitions
                    .filter_map(|result| {
                        filter_result(result, "Failed to load/parse partition data.")
                    })
                    // Filter on configured mountpoints
                    .map(|partition| {
                        self.filesystem
                            .mountpoints
                            .contains_path(Some(partition.mount_point()))
                            .then(|| partition)
                    })
                    .filter_map(|partition| async { partition })
                    // Filter on configured devices
                    .map(|partition| {
                        self.filesystem
                            .devices
                            .contains_path(partition.device().map(|d| d.as_ref()))
                            .then(|| partition)
                    })
                    .filter_map(|partition| async { partition })
                    // Filter on configured filesystems
                    .map(|partition| {
                        self.filesystem
                            .filesystems
                            .contains_str(Some(partition.file_system().as_str()))
                            .then(|| partition)
                    })
                    .filter_map(|partition| async { partition })
                    // Load usage from the partition mount point
                    .filter_map(|partition| async {
                        heim::disk::usage(partition.mount_point())
                            .await
                            .map_err(|error| {
                                error!(
                                    message = "Failed to load partition usage data.",
                                    mount_point = ?partition.mount_point(),
                                    %error,
                                    internal_log_rate_secs = 60,
                                )
                            })
                            .map(|usage| (partition, usage))
                            .ok()
                    })
                    .map(|(partition, usage)| {
                        let timestamp = Utc::now();
                        let fs = partition.file_system();
                        let mut tags = btreemap! {
                            "filesystem" => fs.as_str(),
                            "mountpoint" => partition.mount_point().to_string_lossy()
                        };
                        if let Some(device) = partition.device() {
                            tags.insert("device".into(), device.to_string_lossy().into());
                        }
                        stream::iter(
                            vec![
                                self.gauge(
                                    "filesystem_free_bytes",
                                    timestamp,
                                    usage.free().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.gauge(
                                    "filesystem_total_bytes",
                                    timestamp,
                                    usage.total().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.gauge(
                                    "filesystem_used_bytes",
                                    timestamp,
                                    usage.used().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                #[cfg(not(target_os = "windows"))]
                                self.gauge(
                                    "filesystem_used_ratio",
                                    timestamp,
                                    usage.ratio().get::<ratio>() as f64,
                                    tags,
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load partitions info", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    pub async fn disk_metrics(&self) -> Vec<Metric> {
        match heim::disk::io_counters().await {
            Ok(counters) => {
                counters
                    .filter_map(|result| {
                        filter_result(result, "Failed to load/parse disk I/O data.")
                    })
                    .map(|counter| {
                        self.disk
                            .devices
                            .contains_path(Some(counter.device_name().as_ref()))
                            .then(|| counter)
                    })
                    .filter_map(|counter| async { counter })
                    .map(|counter| {
                        let timestamp = Utc::now();
                        let tags = btreemap! {
                            "device" => counter.device_name().to_string_lossy()
                        };
                        stream::iter(
                            vec![
                                self.counter(
                                    "disk_read_bytes_total",
                                    timestamp,
                                    counter.read_bytes().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_reads_completed_total",
                                    timestamp,
                                    counter.read_count() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_written_bytes_total",
                                    timestamp,
                                    counter.write_bytes().get::<byte>() as f64,
                                    tags.clone(),
                                ),
                                self.counter(
                                    "disk_writes_completed_total",
                                    timestamp,
                                    counter.write_count() as f64,
                                    tags,
                                ),
                            ]
                            .into_iter(),
                        )
                    })
                    .flatten()
                    .collect::<Vec<_>>()
                    .await
            }
            Err(error) => {
                error!(message = "Failed to load disk I/O info.", %error, internal_log_rate_secs = 60);
                vec![]
            }
        }
    }

    fn counter(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric::new(name, MetricKind::Absolute, MetricValue::Counter { value })
            .with_namespace(self.namespace.0.clone())
            .with_tags(Some(tags))
            .with_timestamp(Some(timestamp))
    }

    fn gauge(
        &self,
        name: &str,
        timestamp: DateTime<Utc>,
        value: f64,
        tags: BTreeMap<String, String>,
    ) -> Metric {
        Metric::new(name, MetricKind::Absolute, MetricValue::Gauge { value })
            .with_namespace(self.namespace.0.clone())
            .with_tags(Some(tags))
            .with_timestamp(Some(timestamp))
    }
}

async fn filter_result<T>(result: Result<T, Error>, message: &'static str) -> Option<T> {
    result
        .map_err(|error| error!(message, %error, internal_log_rate_secs = 60))
        .ok()
}

fn add_collector(collector: &str, mut metrics: Vec<Metric>) -> Vec<Metric> {
    for metric in &mut metrics {
        metric.insert_tag("collector".into(), collector.into());
    }
    metrics
}

fn init_roots() {
    #[cfg(target_os = "linux")]
    {
        use std::sync::Once;

        static INIT: Once = Once::new();

        INIT.call_once(|| {
            match std::env::var_os("PROCFS_ROOT") {
                Some(procfs_root) => {
                    info!(
                        message = "PROCFS_ROOT is set in envvars. Using custom for procfs.",
                        custom = ?procfs_root
                    );
                    heim::os::linux::set_procfs_root(std::path::PathBuf::from(&procfs_root));
                }
                None => info!("PROCFS_ROOT is unset. Using default '/proc' for procfs root."),
            };

            match std::env::var_os("SYSFS_ROOT") {
                Some(sysfs_root) => {
                    info!(
                        message = "SYSFS_ROOT is set in envvars. Using custom for sysfs.",
                        custom = ?sysfs_root
                    );
                    heim::os::linux::set_sysfs_root(std::path::PathBuf::from(&sysfs_root));
                }
                None => info!("SYSFS_ROOT is unset. Using default '/sys' for sysfs root."),
            }
        });
    };
}

impl FilterList {
    fn contains<T, M>(&self, value: &Option<T>, matches: M) -> bool
    where
        M: Fn(&PatternWrapper, &T) -> bool,
    {
        (match (&self.includes, value) {
            // No includes list includes everything
            (None, _) => true,
            // Includes list matched against empty value returns false
            (Some(_), None) => false,
            // Otherwise find the given value
            (Some(includes), Some(value)) => includes.iter().any(|pattern| matches(pattern, value)),
        }) && match (&self.excludes, value) {
            // No excludes, list excludes nothing
            (None, _) => true,
            // No value, never excluded
            (Some(_), None) => true,
            // Otherwise find the given value
            (Some(excludes), Some(value)) => {
                !excludes.iter().any(|pattern| matches(pattern, value))
            }
        }
    }

    fn contains_str(&self, value: Option<&str>) -> bool {
        self.contains(&value, |pattern, s| pattern.matches_str(s))
    }

    fn contains_path(&self, value: Option<&Path>) -> bool {
        self.contains(&value, |pattern, path| pattern.matches_path(path))
    }

    #[cfg(test)]
    fn contains_test(&self, value: Option<&str>) -> bool {
        let result = self.contains_str(value);
        assert_eq!(
            result,
            self.contains_path(value.map(|value| std::path::Path::new(value)))
        );
        result
    }
}

// Pattern doesn't implement Deserialize or Serialize, and we can't
// implement them ourselves due the orphan rules, so make a wrapper.
#[derive(Clone, Debug)]
struct PatternWrapper(Pattern);

impl PatternWrapper {
    fn new(pattern: impl AsRef<str>) -> Result<PatternWrapper, PatternError> {
        Ok(PatternWrapper(Pattern::new(pattern.as_ref())?))
    }

    fn matches_str(&self, s: &str) -> bool {
        self.0.matches(s)
    }

    fn matches_path(&self, p: &Path) -> bool {
        self.0.matches_path(p)
    }
}

impl<'de> Deserialize<'de> for PatternWrapper {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(PatternVisitor)
    }
}

struct PatternVisitor;

impl<'de> Visitor<'de> for PatternVisitor {
    type Value = PatternWrapper;

    fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "a string")
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
        PatternWrapper::new(s).map_err(de::Error::custom)
    }
}

impl Serialize for PatternWrapper {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::future::Future;

    #[test]
    fn filterlist_default_includes_everything() {
        let filters = FilterList::default();
        assert!(filters.contains_test(Some("anything")));
        assert!(filters.contains_test(Some("should")));
        assert!(filters.contains_test(Some("work")));
        assert!(filters.contains_test(None));
    }

    #[test]
    fn filterlist_includes_works() {
        let filters = FilterList {
            includes: Some(vec![
                PatternWrapper::new("sda").unwrap(),
                PatternWrapper::new("dm-*").unwrap(),
            ]),
            excludes: None,
        };
        assert!(!filters.contains_test(Some("sd")));
        assert!(filters.contains_test(Some("sda")));
        assert!(!filters.contains_test(Some("sda1")));
        assert!(filters.contains_test(Some("dm-")));
        assert!(filters.contains_test(Some("dm-5")));
        assert!(!filters.contains_test(Some("xda")));
        assert!(!filters.contains_test(None));
    }

    #[test]
    fn filterlist_excludes_works() {
        let filters = FilterList {
            includes: None,
            excludes: Some(vec![
                PatternWrapper::new("sda").unwrap(),
                PatternWrapper::new("dm-*").unwrap(),
            ]),
        };
        assert!(filters.contains_test(Some("sd")));
        assert!(!filters.contains_test(Some("sda")));
        assert!(filters.contains_test(Some("sda1")));
        assert!(!filters.contains_test(Some("dm-")));
        assert!(!filters.contains_test(Some("dm-5")));
        assert!(filters.contains_test(Some("xda")));
        assert!(filters.contains_test(None));
    }

    #[test]
    fn filterlist_includes_and_excludes_works() {
        let filters = FilterList {
            includes: Some(vec![
                PatternWrapper::new("sda").unwrap(),
                PatternWrapper::new("dm-*").unwrap(),
            ]),
            excludes: Some(vec![PatternWrapper::new("dm-5").unwrap()]),
        };
        assert!(!filters.contains_test(Some("sd")));
        assert!(filters.contains_test(Some("sda")));
        assert!(!filters.contains_test(Some("sda1")));
        assert!(filters.contains_test(Some("dm-")));
        assert!(filters.contains_test(Some("dm-1")));
        assert!(!filters.contains_test(Some("dm-5")));
        assert!(!filters.contains_test(Some("xda")));
        assert!(!filters.contains_test(None));
    }

    #[tokio::test]
    async fn filters_on_collectors() {
        let all_metrics_count = HostMetricsConfig::default().capture_metrics().await.count();

        for collector in &[
            Collector::Cpu,
            Collector::Disk,
            Collector::Filesystem,
            Collector::Load,
            Collector::Host,
            Collector::Memory,
            Collector::Network,
        ] {
            let some_metrics = HostMetricsConfig {
                collectors: Some(vec![*collector]),
                ..Default::default()
            }
            .capture_metrics()
            .await;

            assert!(
                all_metrics_count > some_metrics.count(),
                "collector={:?}",
                collector
            );
        }
    }

    #[tokio::test]
    async fn are_taged_with_hostname() {
        let mut metrics = HostMetricsConfig::default().capture_metrics().await;
        let hostname = crate::get_hostname().expect("Broken hostname");
        assert!(!metrics.any(|event| event
            .into_metric()
            .tags()
            .expect("Missing tags")
            .get("host")
            .expect("Missing \"host\" tag")
            != &hostname));
    }

    #[tokio::test]
    async fn uses_custom_namespace() {
        let mut metrics = HostMetricsConfig {
            namespace: Namespace(Some("other".into())),
            ..Default::default()
        }
        .capture_metrics()
        .await;

        assert!(metrics.all(|event| event.into_metric().namespace() == Some("other")));
    }

    #[tokio::test]
    async fn uses_default_namespace() {
        let mut metrics = HostMetricsConfig::default().capture_metrics().await;

        assert!(metrics.all(|event| event.into_metric().namespace() == Some("host")));
    }

    #[tokio::test]
    async fn generates_cpu_metrics() {
        let metrics = HostMetricsConfig::default().cpu_metrics().await;
        assert!(!metrics.is_empty());
        assert!(all_counters(&metrics));

        // They should all be named cpu_seconds_total
        assert_eq!(metrics.len(), count_name(&metrics, "cpu_seconds_total"));

        // They should all have a "mode" tag
        assert_eq!(count_tag(&metrics, "mode"), metrics.len());
    }

    #[tokio::test]
    async fn generates_disk_metrics() {
        let metrics = HostMetricsConfig::default().disk_metrics().await;
        // The Windows test runner doesn't generate any disk metrics on the VM.
        #[cfg(not(target_os = "windows"))]
        assert!(!metrics.is_empty());
        assert!(metrics.len() % 4 == 0);
        assert!(all_counters(&metrics));

        // There are exactly four disk_* names
        for name in &[
            "disk_read_bytes_total",
            "disk_reads_completed_total",
            "disk_written_bytes_total",
            "disk_writes_completed_total",
        ] {
            assert_eq!(
                count_name(&metrics, name),
                metrics.len() / 4,
                "name={}",
                name
            );
        }

        // They should all have a "device" tag
        assert_eq!(count_tag(&metrics, "device"), metrics.len());
    }

    #[tokio::test]
    async fn filters_disk_metrics_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                disk: DiskConfig { devices },
                ..Default::default()
            }
            .disk_metrics()
            .await
        })
        .await;
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn generates_filesystem_metrics() {
        let metrics = HostMetricsConfig::default().filesystem_metrics().await;
        assert!(!metrics.is_empty());
        assert!(metrics.len() % 4 == 0);
        assert!(all_gauges(&metrics));

        // There are exactly three filesystem_* names
        for name in &[
            "filesystem_free_bytes",
            "filesystem_total_bytes",
            "filesystem_used_bytes",
            "filesystem_used_ratio",
        ] {
            assert_eq!(
                count_name(&metrics, name),
                metrics.len() / 4,
                "name={}",
                name
            );
        }

        // They should all have "filesystem" and "mountpoint" tags
        assert_eq!(count_tag(&metrics, "filesystem"), metrics.len());
        assert_eq!(count_tag(&metrics, "mountpoint"), metrics.len());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn generates_filesystem_metrics() {
        let metrics = HostMetricsConfig::default().filesystem_metrics().await;
        assert!(!metrics.is_empty());
        assert!(metrics.len() % 3 == 0);
        assert!(all_gauges(&metrics));

        // There are exactly three filesystem_* names
        for name in &[
            "filesystem_free_bytes",
            "filesystem_total_bytes",
            "filesystem_used_bytes",
        ] {
            assert_eq!(
                count_name(&metrics, name),
                metrics.len() / 3,
                "name={}",
                name
            );
        }

        // They should all have "filesystem" and "mountpoint" tags
        assert_eq!(count_tag(&metrics, "filesystem"), metrics.len());
        assert_eq!(count_tag(&metrics, "mountpoint"), metrics.len());
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    devices,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_filesystem() {
        assert_filtered_metrics("filesystem", |filesystems| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    filesystems,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    #[tokio::test]
    async fn filesystem_metrics_filters_on_mountpoint() {
        assert_filtered_metrics("mountpoint", |mountpoints| async {
            HostMetricsConfig {
                filesystem: FilesystemConfig {
                    mountpoints,
                    ..Default::default()
                },
                ..Default::default()
            }
            .filesystem_metrics()
            .await
        })
        .await;
    }

    // The Windows CI environment produces zero network metrics, causing
    // this to always fail.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn generates_network_metrics() {
        let metrics = HostMetricsConfig::default().network_metrics().await;
        assert!(!metrics.is_empty());
        assert!(all_counters(&metrics));

        // All metrics are named network_*
        assert!(!metrics
            .iter()
            .any(|metric| !metric.name().starts_with("network_")));

        // They should all have a "device" tag
        assert_eq!(count_tag(&metrics, "device"), metrics.len());
    }

    // The Windows CI environment produces zero network metrics, causing
    // this to always fail.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn network_metrics_filters_on_device() {
        assert_filtered_metrics("device", |devices| async {
            HostMetricsConfig {
                network: NetworkConfig { devices },
                ..Default::default()
            }
            .network_metrics()
            .await
        })
        .await;
    }

    // Windows does not produce load average metrics.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn generates_loadavg_metrics() {
        let metrics = HostMetricsConfig::default().loadavg_metrics().await;
        assert_eq!(metrics.len(), 3);
        assert!(all_gauges(&metrics));

        // All metrics are named load*
        assert!(!metrics
            .iter()
            .any(|metric| !metric.name().starts_with("load")));
    }

    #[tokio::test]
    async fn generates_host_metrics() {
        let metrics = HostMetricsConfig::default().host_metrics().await;
        assert_eq!(metrics.len(), 2);
        assert!(all_gauges(&metrics));
    }

    fn all_counters(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value(), &MetricValue::Counter { .. }))
    }

    fn all_gauges(metrics: &[Metric]) -> bool {
        !metrics
            .iter()
            .any(|metric| !matches!(metric.value(), &MetricValue::Gauge { .. }))
    }

    fn all_tags_match(metrics: &[Metric], tag: &str, matches: impl Fn(&str) -> bool) -> bool {
        !metrics.iter().any(|metric| {
            metric
                .tags()
                .unwrap()
                .get(tag)
                .map(|value| !matches(value))
                .unwrap_or(false)
        })
    }

    fn count_name(metrics: &[Metric], name: &str) -> usize {
        metrics
            .iter()
            .filter(|metric| metric.name() == name)
            .count()
    }

    fn count_tag(metrics: &[Metric], tag: &str) -> usize {
        metrics
            .iter()
            .filter(|metric| {
                metric
                    .tags()
                    .expect("Metric is missing tags")
                    .contains_key(tag)
            })
            .count()
    }

    fn collect_tag_values(metrics: &[Metric], tag: &str) -> HashSet<String> {
        metrics
            .iter()
            .filter_map(|metric| metric.tags().unwrap().get(tag).cloned())
            .collect::<HashSet<_>>()
    }

    // Run a series of tests using filters to ensure they are obeyed
    async fn assert_filtered_metrics<'a, Get, Fut>(tag: &str, get_metrics: Get)
    where
        Get: Fn(FilterList) -> Fut,
        Fut: Future<Output = Vec<Metric>>,
    {
        let all_metrics = get_metrics(FilterList::default()).await;
        let keys = collect_tag_values(&all_metrics, tag);
        // Pick an arbitrary key value
        if let Some(key) = keys.into_iter().next() {
            let key_prefix = &key[..key.len() - 1];

            let filtered_metrics_with = get_metrics(FilterList {
                includes: Some(vec![PatternWrapper::new(&key).unwrap()]),
                excludes: None,
            })
            .await;

            assert!(filtered_metrics_with.len() <= all_metrics.len());
            assert!(!filtered_metrics_with.is_empty());
            assert!(all_tags_match(&filtered_metrics_with, tag, |s| s == key));

            let filtered_metrics_with_match = get_metrics(FilterList {
                includes: Some(vec![
                    PatternWrapper::new(&format!("{}*", key_prefix)).unwrap()
                ]),
                excludes: None,
            })
            .await;

            assert!(filtered_metrics_with_match.len() >= filtered_metrics_with.len());
            assert!(all_tags_match(&filtered_metrics_with_match, tag, |s| {
                s.starts_with(key_prefix)
            }));

            let filtered_metrics_without = get_metrics(FilterList {
                includes: None,
                excludes: Some(vec![PatternWrapper::new(&key).unwrap()]),
            })
            .await;

            assert!(filtered_metrics_without.len() <= all_metrics.len());
            assert!(all_tags_match(&filtered_metrics_without, tag, |s| s != key));

            let filtered_metrics_without_match = get_metrics(FilterList {
                includes: None,
                excludes: Some(vec![
                    PatternWrapper::new(&format!("{}*", key_prefix)).unwrap()
                ]),
            })
            .await;

            assert!(filtered_metrics_without_match.len() <= filtered_metrics_without.len());
            assert!(all_tags_match(&filtered_metrics_without_match, tag, |s| {
                !s.starts_with(key_prefix)
            }));

            assert!(
                filtered_metrics_with.len() + filtered_metrics_without.len() <= all_metrics.len()
            );
        }
    }
}
