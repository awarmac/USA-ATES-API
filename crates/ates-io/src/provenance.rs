//! Provenance stamped into every output.

/// Statement carried by every output file.
pub const DISCLAIMER: &str = "Modeled terrain classification, not an avalanche forecast. \
     Does not assess snowpack or current conditions.";

/// Where an output came from: tool, configuration and input data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub tool_version: String,
    /// Path (or other label) of the config file used.
    pub config_source: String,
    /// Hex digest of the config file bytes.
    pub config_hash: String,
    /// Region preset applied, if any.
    pub region: Option<String>,
    /// Identity of the DEM, e.g. its path or dataset name.
    pub dem_source: String,
    /// What the output contains, e.g. "slope and aspect (crude proxy)".
    pub product: String,
}

impl Provenance {
    /// Key/value tags in the order they are written to file metadata.
    pub fn tags(&self) -> Vec<(&'static str, String)> {
        vec![
            ("ATES_TOOL_VERSION", self.tool_version.clone()),
            ("ATES_PRODUCT", self.product.clone()),
            ("ATES_CONFIG", self.config_source.clone()),
            ("ATES_CONFIG_FNV1A64", self.config_hash.clone()),
            (
                "ATES_REGION",
                self.region.clone().unwrap_or_else(|| "none".into()),
            ),
            ("ATES_DEM_SOURCE", self.dem_source.clone()),
            ("ATES_DISCLAIMER", DISCLAIMER.to_owned()),
        ]
    }
}

/// 64-bit FNV-1a digest as 16 hex characters. Used to fingerprint config
/// files; it is not a cryptographic hash.
pub fn fnv1a64_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_vectors() {
        assert_eq!(fnv1a64_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a64_hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64_hex(b"foobar"), "85944171f73967e8");
    }

    #[test]
    fn tags_include_disclaimer() {
        let p = Provenance {
            tool_version: "0.1.0".into(),
            config_source: "config/default.toml".into(),
            config_hash: "0".into(),
            region: None,
            dem_source: "dem.tif".into(),
            product: "slope".into(),
        };
        let tags = p.tags();
        assert!(
            tags.iter()
                .any(|(k, v)| *k == "ATES_DISCLAIMER" && v.contains("not an avalanche forecast"))
        );
        assert!(tags.iter().any(|(k, v)| *k == "ATES_REGION" && v == "none"));
    }
}
