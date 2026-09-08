pub fn cpu_count() -> usize {
    // Walk fan-out: NVMe readdir scales to here, then thrashes (32
    // measured slower). Memory grows with threads, so this stays tight.
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .clamp(1, 16)
}

/// Rayon pool size. Measurement is lstat-bound over hundreds of thousands
/// of files and keeps scaling past the walk's sweet spot.
pub fn rayon_threads() -> usize {
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .clamp(1, 32)
}
