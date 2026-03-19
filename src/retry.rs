pub fn jitter(base_ms: u64, ratio: f64) -> u64 {
    if ratio <= 0.0 {
        return base_ms;
    }
    let spread = (base_ms as f64 * ratio).round() as i64;
    let delta = (rand::random::<f64>() * (spread as f64 * 2.0)).round() as i64 - spread;
    (base_ms as i64 + delta).max(0) as u64
}
