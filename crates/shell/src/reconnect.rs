pub fn random_duration(min: std::time::Duration, max: std::time::Duration) -> std::time::Duration {
    let min_ms = min.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    if min_ms >= max_ms {
        return min;
    }
    let offset = rand::random::<u64>() % (max_ms - min_ms + 1);
    std::time::Duration::from_millis(min_ms + offset)
}
