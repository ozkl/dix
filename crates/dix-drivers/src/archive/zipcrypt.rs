//! Zip encryption, for reading members with a password:
//!
//! * **ZipCrypto**, the traditional PKWARE stream cipher (APPNOTE 6.1): three
//!   32-bit keys stirred by the password, a 12-byte header whose last byte
//!   checks the password (the CRC's top byte, or the time's with a data
//!   descriptor). Weak, but what `zip -e` and most tools still write;
//! * **WinZip AES** (AE-1 / AE-2; method 99 with an `0x9901` extra field):
//!   PBKDF2-HMAC-SHA1 (1000 rounds) from the password and a salt gives an
//!   AES-128/192/256 key, an HMAC key and a 2-byte password check; the data
//!   is AES-CTR (a little-endian counter from 1) and ends with 10 bytes of
//!   HMAC-SHA1 over it. What 7-Zip and WinZip write when asked for AES.

use dix_core::{Error, Result};

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

struct Keys([u32; 3]);

impl Keys {
    fn new(password: &[u8]) -> Keys {
        let mut k = Keys([0x1234_5678, 0x2345_6789, 0x3456_7890]);
        for &b in password {
            k.update(b);
        }
        k
    }

    fn update(&mut self, b: u8) {
        let k = &mut self.0;
        k[0] = CRC[((k[0] ^ b as u32) & 0xFF) as usize] ^ (k[0] >> 8);
        k[1] = k[1].wrapping_add(k[0] & 0xFF).wrapping_mul(134_775_813).wrapping_add(1);
        k[2] = CRC[((k[2] ^ (k[1] >> 24)) & 0xFF) as usize] ^ (k[2] >> 8);
    }

    fn decrypt(&mut self, c: u8) -> u8 {
        let t = (self.0[2] | 2) as u16;
        let p = c ^ (t.wrapping_mul(t ^ 1) >> 8) as u8;
        self.update(p);
        p
    }
}

/// ZipCrypto: the member's data without its encryption, or `None` when the
/// header's check byte says the password is wrong (1 in 256 wrong ones
/// pass: the caller checks the CRC too).
pub fn zipcrypto(password: &[u8], data: &[u8], check: u8) -> Result<Option<Vec<u8>>> {
    if data.len() < 12 {
        return Err(Error::corrupted("an encrypted zip member is too short"));
    }
    let mut k = Keys::new(password);
    let mut header = [0u8; 12];
    for (h, &c) in header.iter_mut().zip(&data[..12]) {
        *h = k.decrypt(c);
    }
    if header[11] != check {
        return Ok(None);
    }
    Ok(Some(data[12..].iter().map(|&c| k.decrypt(c)).collect()))
}

/// WinZip AES with key strength 1–3 (128, 192, 256 bits): the member's
/// data decrypted, or `None` when the password check says it is wrong.
#[cfg(feature = "archive-aes")]
pub fn winzip_aes(password: &[u8], data: &[u8], strength: u8) -> Result<Option<Vec<u8>>> {
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    use hmac::{Hmac, Mac};
    let damaged = |what: &str| Error::corrupted(format!("an AES-encrypted zip member: {what}"));
    let key_len = match strength {
        1 => 16,
        2 => 24,
        3 => 32,
        _ => return Err(damaged("an unknown key size")),
    };
    let salt_len = key_len / 2;
    if data.len() < salt_len + 2 + 10 {
        return Err(damaged("too short"));
    }
    let (salt, rest) = data.split_at(salt_len);
    let (verify, rest) = rest.split_at(2);
    let (body, mac) = rest.split_at(rest.len() - 10);
    let mut keys = vec![0u8; 2 * key_len + 2];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, 1000, &mut keys);
    if keys[2 * key_len..] != *verify {
        return Ok(None);
    }
    let mut hmac = <Hmac<sha1::Sha1> as Mac>::new_from_slice(&keys[key_len..2 * key_len]).expect("any key length");
    hmac.update(body);
    if hmac.finalize().into_bytes()[..10] != *mac {
        return Err(damaged("its authentication code does not match (damaged)"));
    }
    enum Cipher {
        A128(aes::Aes128),
        A192(aes::Aes192),
        A256(aes::Aes256),
    }
    let key = &keys[..key_len];
    let cipher = match key_len {
        16 => Cipher::A128(aes::Aes128::new_from_slice(key).unwrap()),
        24 => Cipher::A192(aes::Aes192::new_from_slice(key).unwrap()),
        _ => Cipher::A256(aes::Aes256::new_from_slice(key).unwrap()),
    };
    let mut out = body.to_vec();
    for (i, chunk) in out.chunks_mut(16).enumerate() {
        let mut block = GenericArray::from(((i as u128) + 1).to_le_bytes());
        match &cipher {
            Cipher::A128(c) => c.encrypt_block(&mut block),
            Cipher::A192(c) => c.encrypt_block(&mut block),
            Cipher::A256(c) => c.encrypt_block(&mut block),
        }
        for (b, k) in chunk.iter_mut().zip(block.iter()) {
            *b ^= k;
        }
    }
    Ok(Some(out))
}

#[cfg(not(feature = "archive-aes"))]
pub fn winzip_aes(_: &[u8], _: &[u8], _: u8) -> Result<Option<Vec<u8>>> {
    Err(super::codec::left_out("AES encryption"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypting is decrypting with the plain byte fed back: the same keys.
    fn encrypt(password: &[u8], header: [u8; 12], plain: &[u8]) -> Vec<u8> {
        let mut k = Keys::new(password);
        header
            .iter()
            .chain(plain)
            .map(|&p| {
                let t = (k.0[2] | 2) as u16;
                let c = p ^ (t.wrapping_mul(t ^ 1) >> 8) as u8;
                k.update(p);
                c
            })
            .collect()
    }

    #[test]
    fn zipcrypto_round_trip_and_check() {
        let mut header = [7u8; 12];
        header[11] = 0xAB;
        let data = encrypt(b"secret", header, b"hello world");
        assert_eq!(zipcrypto(b"secret", &data, 0xAB).unwrap().unwrap(), b"hello world");
        assert_eq!(zipcrypto(b"wrong", &data, 0xAB).unwrap().filter(|d| d == b"hello world"), None);
    }
}
