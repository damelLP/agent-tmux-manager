use atmd::monitor::ProcessMonitor;

#[test]
fn test_monitor_cpu_measurement() {
    let mut monitor = ProcessMonitor::new();

    // Fresh and repeated samples must be valid without a workload or warm-up.
    // Rate accuracy belongs to sysinfo; ATM's policy uses injected samples.
    for _ in 0..2 {
        let metrics = monitor.refresh();
        assert!(metrics.cpu_percent.is_finite());
        assert!(metrics.cpu_percent >= 0.0);
    }
}
