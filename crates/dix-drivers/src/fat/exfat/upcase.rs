//! The up-case table: how exFAT compares names without case (and hashes
//! them). One UTF-16 unit per entry; stored compressed: `0xFFFF, n` stands
//! for `n` entries that map to themselves.
//!
//! Lookups use the table of the volume being read, whatever made it; new
//! volumes get one generated from Rust's (Unicode) simple upper-case
//! mappings of the Basic Multilingual Plane.

pub struct Upcase(Vec<u16>);

impl Upcase {
    /// Decompresses a stored table (entries it does not cover map to
    /// themselves).
    pub fn parse(bytes: &[u8]) -> Upcase {
        let words: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let mut map: Vec<u16> = Vec::with_capacity(0x10000);
        let mut i = 0;
        while i < words.len() && map.len() < 0x10000 {
            if words[i] == 0xFFFF && i + 1 < words.len() {
                for _ in 0..words[i + 1] {
                    let n = map.len() as u16;
                    map.push(n);
                    if map.len() == 0x10000 {
                        break;
                    }
                }
                i += 2;
            } else {
                map.push(words[i]);
                i += 1;
            }
        }
        while map.len() < 0x10000 {
            let n = map.len() as u16;
            map.push(n);
        }
        Upcase(map)
    }

    /// The table new volumes get.
    pub fn generate() -> Upcase {
        let map = (0..=0xFFFFu32)
            .map(|u| {
                let Some(c) = char::from_u32(u) else { return u as u16 };
                let mut up = c.to_uppercase();
                match (up.next(), up.next()) {
                    (Some(x), None) if (x as u32) <= 0xFFFF => x as u16,
                    _ => u as u16,
                }
            })
            .collect();
        Upcase(map)
    }

    pub fn up(&self, u: u16) -> u16 {
        self.0[u as usize]
    }

    /// A name in upper case, as UTF-16 units.
    pub fn upper(&self, name: &str) -> Vec<u16> {
        name.encode_utf16().map(|u| self.up(u)).collect()
    }

    /// The compressed form.
    pub fn compress(&self) -> Vec<u8> {
        let mut out: Vec<u16> = Vec::new();
        let mut i = 0usize;
        while i < 0x10000 {
            let mut run = 0;
            while i + run < 0x10000 && self.0[i + run] as usize == i + run && run < 0xFFFF {
                run += 1;
            }
            if run >= 3 {
                out.push(0xFFFF);
                out.push(run as u16);
                i += run;
            } else {
                out.push(self.0[i]);
                i += 1;
            }
        }
        out.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

/// The table checksum (in the root's up-case table entry).
pub fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |c, &b| c.rotate_right(1).wrapping_add(b as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = Upcase::generate();
        assert_eq!(t.up('a' as u16), 'A' as u16);
        assert_eq!(t.up('ü' as u16), 'Ü' as u16);
        assert_eq!(t.up('я' as u16), 'Я' as u16);
        assert_eq!(t.up('1' as u16), '1' as u16);
        let packed = t.compress();
        assert!(packed.len() < 8192, "{}", packed.len());
        let back = Upcase::parse(&packed);
        assert!(back.0 == t.0);
    }
}
