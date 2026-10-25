//! Size formatting, matching coreutils' human_readable() with the options
//! du uses: always round up, and for -h, powers of 1024 with one decimal
//! below 10.

const SUFFIXES: &[u8] = b"KMGTPEZYRQ";

/// Disk usage in 1024-byte blocks, du's default unit.
pub fn blocks(bytes: u64) -> String {
    bytes.div_ceil(1024).to_string()
}

/// Disk usage for -h, e.g. "0", "512", "4.0K", "12K", "1.5M".
///
/// The scale is the largest power of 1024 not exceeding `bytes`. Rounding up
/// can carry: 9.95K shows as "10K", and 1023.5K as "1.0M". Intermediates use
/// u128 so 10 * u64::MAX cannot overflow.
pub fn human(bytes: u64) -> String {
    let n = bytes as u128;
    let mut exp = 0;
    let mut scale: u128 = 1;
    while n >= scale * 1024 && exp < SUFFIXES.len() {
        scale *= 1024;
        exp += 1;
    }
    if exp == 0 {
        return bytes.to_string();
    }

    let suffix = |e: usize| SUFFIXES[e - 1] as char;
    if n / scale < 10 {
        let tenths = (n * 10).div_ceil(scale);
        if tenths < 100 {
            return format!("{}.{}{}", tenths / 10, tenths % 10, suffix(exp));
        }
        return format!("10{}", suffix(exp));
    }
    let whole = n.div_ceil(scale);
    if whole == 1024 && exp < SUFFIXES.len() {
        return format!("1.0{}", suffix(exp + 1));
    }
    format!("{}{}", whole, suffix(exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 1024;
    const M: u64 = K * K;

    #[test]
    fn human_sizes() {
        let cases = [
            ("zero", 0, "0"),
            ("below 1K prints bytes", 512, "512"),
            ("exactly 1K", K, "1.0K"),
            ("one page", 4 * K, "4.0K"),
            ("rounds tenths up", K + 1, "1.1K"),
            ("just under 10K carries to 10K", 10 * K - 1, "10K"),
            ("integer above 10", 12 * K, "12K"),
            ("integer rounds up", 12 * K + 1, "13K"),
            ("carry into next unit", 1024 * K - 1, "1.0M"),
            ("1.5M", 1536 * K, "1.5M"),
            ("gigabytes", 3 * M * K, "3.0G"),
            ("max u64", u64::MAX, "16E"),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(human(bytes), want, "{name}");
        }
    }

    #[test]
    fn block_sizes() {
        let cases = [("zero", 0, "0"), ("partial block rounds up", 1, "1"), ("exact", 4096, "4"), ("1.5M", 1536 * K, "1536")];
        for (name, bytes, want) in cases {
            assert_eq!(blocks(bytes), want, "{name}");
        }
    }
}
