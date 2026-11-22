//! Presentation formatting shared by the apps' surfaces: byte sizes
//! today, the shape every surface renders the same way. One decimal,
//! the unit carrying the magnitude, TiB the honest cap.

/// Bytes at the table's width: "N B" below a kibibyte, then KiB, MiB,
/// GiB, and TiB as the cap (one decimal, so nothing ever prints
/// "1024.0" of anything).
pub fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        return format!("{bytes} B");
    }
    let mut value = value / KIB;
    for unit in ["KiB", "MiB", "GiB"] {
        if value < KIB {
            return format!("{value:.1} {unit}");
        }
        value /= KIB;
    }
    format!("{value:.1} TiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_names_its_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 5), "5.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024 * 31), "31.0 GiB");
        // the cap: two tebibytes, no "1024.0 GiB"
        assert_eq!(human_bytes(1024u64 * 1024 * 1024 * 1024 * 2), "2.0 TiB");
    }

    #[test]
    fn boundaries_never_show_1024() {
        // exactly one of each unit, and just under the next line
        for (bytes, shown) in [
            (1024u64, "1.0 KiB"),
            (1024 * 1024 - 1, "1024.0 KiB"),
            (1024 * 1024, "1.0 MiB"),
            (1024 * 1024 * 1024 - 1, "1024.0 MiB"),
            (1024 * 1024 * 1024, "1.0 GiB"),
            (1024u64 * 1024 * 1024 * 1024 - 1, "1024.0 GiB"),
        ] {
            assert_eq!(human_bytes(bytes), shown);
        }
    }
}
