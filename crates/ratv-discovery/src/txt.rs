/// Parsed TXT record: `key=value` pairs. Keys compare case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Txt {
    entries: Vec<(String, String)>,
}

impl Txt {
    /// Parses raw TXT data (length-prefixed strings). Malformed tails are ignored.
    #[must_use]
    pub fn parse(data: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut pos = 0;
        while let Some(&len) = data.get(pos) {
            let Some(item) = data.get(pos + 1..pos + 1 + usize::from(len)) else {
                break;
            };
            pos += 1 + usize::from(len);
            if item.is_empty() {
                continue;
            }
            let (k, v) = item
                .iter()
                .position(|&b| b == b'=')
                .map_or((item, &[][..]), |i| (&item[..i], &item[i + 1..]));
            entries.push((
                String::from_utf8_lossy(k).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            ));
        }
        Self { entries }
    }

    /// Returns the value for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// Iterates over all entries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if there are no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parses AirPlay `features`: `0xLOW` or `0xLOW,0xHIGH` into one 64-bit value.
#[must_use]
pub(crate) fn parse_features(s: &str) -> Option<u64> {
    let mut parts = s.split(',');
    let parse = |p: &str| {
        u32::from_str_radix(
            p.trim().trim_start_matches("0x").trim_start_matches("0X"),
            16,
        )
        .ok()
    };
    let low = parse(parts.next()?)?;
    let high = match parts.next() {
        Some(p) => parse(p)?,
        None => 0,
    };
    Some(u64::from(high) << 32 | u64::from(low))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_entries() {
        let t = Txt::parse(b"\x07model=X\x04flag\x00\x05a=b=c");
        assert_eq!(t.get("MODEL"), Some("X"));
        assert_eq!(t.get("flag"), Some(""));
        assert_eq!(t.get("a"), Some("b=c"));
        assert_eq!(t.len(), 3);
        assert!(!t.is_empty());
        assert_eq!(t.iter().count(), 3);
        assert!(Txt::parse(b"\x09short").is_empty());
        assert_eq!(Txt::default().get("x"), None);
    }

    #[test]
    fn features() {
        assert_eq!(
            parse_features("0x4A7FDFD5,0xBC157FDE"),
            Some(0xBC15_7FDE_4A7F_DFD5)
        );
        assert_eq!(parse_features("0x5A7FFFF7"), Some(0x5A7F_FFF7));
        assert_eq!(parse_features("zz"), None);
        assert_eq!(parse_features("0x1,zz"), None);
    }
}
