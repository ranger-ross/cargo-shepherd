pub fn cpu_count() -> usize {
    // Measured sweet spot on 16 cores: walk and measure are both
    // syscall-bound, and past 12 threads only adds scheduling noise.
    // Memory grows with threads, so this stays tight.
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .clamp(1, 12)
}
