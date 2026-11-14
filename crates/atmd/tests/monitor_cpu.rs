use atmd::monitor::ProcessMonitor;

#[test]
fn test_monitor_cpu_measurement() {
    use std::time::Duration;

    let mut monitor = ProcessMonitor::new();

    // sysinfo 0.32.1 skips CPU calculation when both baseline CPU tick counts
    // are zero. Burn CPU before the baseline so a fresh process has used ticks.
    let warmup_start = std::time::Instant::now();
    let mut warmup_sum: u64 = 0;
    while warmup_start.elapsed() < Duration::from_millis(100) {
        for i in 0..100000 {
            warmup_sum = warmup_sum.wrapping_add(i);
        }
    }
    std::hint::black_box(warmup_sum);

    // First refresh establishes baseline
    let _ = monitor.refresh();

    // Do CPU work for 500ms
    let start = std::time::Instant::now();
    let mut sum: u64 = 0;
    while start.elapsed() < Duration::from_millis(500) {
        for i in 0..100000 {
            sum = sum.wrapping_add(i);
        }
    }
    std::hint::black_box(sum);

    // Second refresh should show CPU usage
    let metrics = monitor.refresh();

    // CPU should show ~100% for one core (we did 500ms of work in 500ms)
    // Allow some variance due to scheduling
    assert!(
        metrics.cpu_percent > 50.0,
        "Expected CPU > 50%, got {:.2}%",
        metrics.cpu_percent
    );
    assert!(
        metrics.cpu_percent < 200.0,
        "CPU seems too high: {:.2}%",
        metrics.cpu_percent
    );
}
