//! Heap accounting helpers for the memory report.

/// Approximate heap bytes of a hashbrown table that can hold `capacity` items
/// of `entry` bytes each: power-of-two buckets at 7/8 load, one control byte
/// per bucket plus one SIMD group of trailing control bytes.
pub(crate) fn map_bytes(capacity: usize, entry: usize) -> u64 {
    if capacity == 0 {
        return 0;
    }
    let buckets = if capacity < 8 {
        (capacity + 1).next_power_of_two()
    } else {
        (capacity * 8 / 7).next_power_of_two()
    };
    (buckets * (entry + 1) + 16) as u64
}

/// Heap bytes of a `Vec<T>` by capacity.
pub(crate) fn vec_bytes<T>(v: &Vec<T>) -> u64 {
    (v.capacity() * std::mem::size_of::<T>()) as u64
}
