//! Process monitoring for the ATM daemon.
//!
//! Tracks CPU and memory usage of the daemon process, providing:
//! - Periodic logging of resource usage
//! - Alerts when thresholds are exceeded
//! - Metrics for external monitoring integration
//!
//! # Panic-Free Guarantees
//!
//! All code follows CLAUDE.md panic-free policy:
//! - No `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, `todo!()`
//! - Uses pattern matching and `unwrap_or` for fallible operations

use std::process;
use std::time::Duration;

use sysinfo::{Pid, System};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Memory usage warning threshold in MB.
pub const HIGH_MEMORY_THRESHOLD_MB: u64 = 100;

/// CPU usage warning threshold (percentage).
pub const HIGH_CPU_THRESHOLD_PERCENT: f32 = 80.0;

/// How often to sample metrics.
pub const METRICS_INTERVAL: Duration = Duration::from_secs(60);

/// Current process metrics snapshot.
#[derive(Debug, Clone, Default)]
pub struct ProcessMetrics {
    /// Memory usage in bytes
    pub memory_bytes: u64,

    /// Memory usage in megabytes (convenience)
    pub memory_mb: u64,

    /// CPU usage as percentage (0.0 - 100.0+)
    pub cpu_percent: f32,

    /// Whether memory is above threshold
    pub memory_high: bool,

    /// Whether CPU is above threshold
    pub cpu_high: bool,
}

impl ProcessMetrics {
    /// Returns true if any metric is above its threshold.
    pub fn is_any_high(&self) -> bool {
        self.memory_high || self.cpu_high
    }
}

/// Process monitor for tracking daemon resource usage.
///
/// Uses the `sysinfo` crate to query process metrics.
/// The monitor must be refreshed before reading metrics.
pub struct ProcessMonitor {
    system: System,
    pid: Pid,
    memory_threshold_mb: u64,
    cpu_threshold_percent: f32,
}

impl ProcessMonitor {
    /// Creates a new process monitor for the current process.
    pub fn new() -> Self {
        Self::with_thresholds(HIGH_MEMORY_THRESHOLD_MB, HIGH_CPU_THRESHOLD_PERCENT)
    }

    /// Creates a process monitor with custom thresholds.
    pub fn with_thresholds(memory_threshold_mb: u64, cpu_threshold_percent: f32) -> Self {
        Self {
            system: System::new(),
            pid: Pid::from_u32(process::id()),
            memory_threshold_mb,
            cpu_threshold_percent,
        }
    }

    /// Refreshes process information and returns current metrics.
    ///
    /// Note: sysinfo requires two refresh calls with a delay to get accurate
    /// CPU usage. For periodic monitoring, the previous refresh serves as
    /// the baseline, so single refresh works after the first call.
    ///
    /// Important: We must call refresh_all() for CPU calculations to work
    /// correctly. Just refreshing a single process doesn't compute CPU%.
    pub fn refresh(&mut self) -> ProcessMetrics {
        // refresh_all() is required for CPU calculation to work
        // (refresh_processes with single PID doesn't compute CPU correctly)
        self.system.refresh_all();

        let (memory_bytes, cpu_percent) = self
            .system
            .process(self.pid)
            .map(|p| (p.memory(), p.cpu_usage()))
            .unwrap_or((0, 0.0));

        let memory_mb = memory_bytes / 1024 / 1024;
        let memory_high = memory_mb > self.memory_threshold_mb;
        let cpu_high = cpu_percent > self.cpu_threshold_percent;

        ProcessMetrics {
            memory_bytes,
            memory_mb,
            cpu_percent,
            memory_high,
            cpu_high,
        }
    }

    /// Returns the current memory threshold in MB.
    pub fn memory_threshold_mb(&self) -> u64 {
        self.memory_threshold_mb
    }

    /// Returns the current CPU threshold as percentage.
    pub fn cpu_threshold_percent(&self) -> f32 {
        self.cpu_threshold_percent
    }
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawns the metrics monitoring task.
///
/// This task periodically logs resource usage and warns when thresholds
/// are exceeded. Uses cooperative shutdown via CancellationToken.
///
/// # Arguments
///
/// * `cancel_token` - Token for graceful shutdown
///
/// # Returns
///
/// A join handle for the spawned task.
pub fn spawn_monitor_task(cancel_token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = ProcessMonitor::new();
        let mut tick = interval(METRICS_INTERVAL);

        // Initial refresh to establish baseline for CPU calculation
        let _ = monitor.refresh();

        info!(
            memory_threshold_mb = monitor.memory_threshold_mb(),
            cpu_threshold_percent = monitor.cpu_threshold_percent(),
            interval_secs = METRICS_INTERVAL.as_secs(),
            "Process monitor started"
        );

        loop {
            tokio::select! {
                biased;

                _ = cancel_token.cancelled() => {
                    info!("Process monitor shutting down");
                    break;
                }

                _ = tick.tick() => {
                    let metrics = monitor.refresh();
                    log_metrics(&metrics, &monitor);
                }
            }
        }

        debug!("Process monitor task completed");
    })
}

/// Logs current metrics, warning if thresholds are exceeded.
fn log_metrics(metrics: &ProcessMetrics, monitor: &ProcessMonitor) {
    if metrics.memory_high {
        warn!(
            memory_mb = metrics.memory_mb,
            threshold_mb = monitor.memory_threshold_mb(),
            cpu_percent = format!("{:.1}", metrics.cpu_percent),
            "HIGH MEMORY: Daemon memory usage above threshold"
        );
    } else if metrics.cpu_high {
        warn!(
            memory_mb = metrics.memory_mb,
            cpu_percent = format!("{:.1}", metrics.cpu_percent),
            threshold_percent = monitor.cpu_threshold_percent(),
            "HIGH CPU: Daemon CPU usage above threshold"
        );
    } else {
        info!(
            memory_mb = metrics.memory_mb,
            cpu_percent = format!("{:.1}", metrics.cpu_percent),
            "Daemon resource usage"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_metrics_default() {
        let metrics = ProcessMetrics::default();
        assert_eq!(metrics.memory_bytes, 0);
        assert_eq!(metrics.memory_mb, 0);
        assert_eq!(metrics.cpu_percent, 0.0);
        assert!(!metrics.memory_high);
        assert!(!metrics.cpu_high);
        assert!(!metrics.is_any_high());
    }

    #[test]
    fn test_process_metrics_high_memory() {
        let metrics = ProcessMetrics {
            memory_bytes: 200 * 1024 * 1024,
            memory_mb: 200,
            cpu_percent: 10.0,
            memory_high: true,
            cpu_high: false,
        };
        assert!(metrics.is_any_high());
    }

    #[test]
    fn test_process_metrics_high_cpu() {
        let metrics = ProcessMetrics {
            memory_bytes: 50 * 1024 * 1024,
            memory_mb: 50,
            cpu_percent: 95.0,
            memory_high: false,
            cpu_high: true,
        };
        assert!(metrics.is_any_high());
    }

    #[test]
    fn test_monitor_creation() {
        let monitor = ProcessMonitor::new();
        assert_eq!(monitor.memory_threshold_mb(), HIGH_MEMORY_THRESHOLD_MB);
        assert_eq!(monitor.cpu_threshold_percent(), HIGH_CPU_THRESHOLD_PERCENT);
    }

    #[test]
    fn test_monitor_custom_thresholds() {
        let monitor = ProcessMonitor::with_thresholds(50, 50.0);
        assert_eq!(monitor.memory_threshold_mb(), 50);
        assert_eq!(monitor.cpu_threshold_percent(), 50.0);
    }

    #[test]
    fn test_monitor_refresh_returns_metrics() {
        let mut monitor = ProcessMonitor::new();
        let metrics = monitor.refresh();

        // We should get some memory usage (process is running)
        assert!(metrics.memory_bytes > 0);
        assert!(metrics.memory_mb > 0 || metrics.memory_bytes < 1024 * 1024);

        // CPU might be 0.0 on first call (no baseline yet)
        // Just verify it's a valid number
        assert!(metrics.cpu_percent >= 0.0);
    }

    #[test]
    fn test_constants() {
        assert_eq!(HIGH_MEMORY_THRESHOLD_MB, 100);
        assert_eq!(HIGH_CPU_THRESHOLD_PERCENT, 80.0);
        assert_eq!(METRICS_INTERVAL, Duration::from_secs(60));
    }
}
