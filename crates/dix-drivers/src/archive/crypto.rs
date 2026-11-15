//! AES pieces shared by the 7z and RAR readers (zip has its own, see
//! `zipcrypt`): AES-256-CBC decryption and the two key derivations.
//!
//! * **7z** (`7zAES`): SHA-256 over (salt, password as UTF-16LE, an 8-byte
//!   counter) repeated 2^cycles times (2^19 by default); AES-256-CBC with
//!   the IV from the coder's properties. There is no password check: a
//!   wrong password shows as damaged data, caught by the CRCs.
//! * **RAR 5**: PBKDF2-HMAC-SHA256 with 2^count rounds (2^15 by default)
//!   gives the key; 16 and 32 rounds more give a key for the checksums and
//!   an 8-byte password check value. AES-256-CBC.
//!
//! * **RAR 4** (RAR 3.x): SHA-1 over (password as UTF-16LE, an 8-byte
//!   salt, a 3-byte counter) 2^18 times gives an AES-128 key; a byte of 16
//!   intermediate digests gives the IV. AES-128-CBC. RAR 3's SHA-1 wrote
//!   its message schedule back over input it hashed in place (passwords of
//!   29 characters and more are affected); unrar keeps that for
//!   compatibility, and so does `Sha1Rar29` here.
//!
//! Derived keys are kept (by salt, rounds and a hash of the password):
//! deriving takes a noticeable fraction of a second.

use dix_core::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// AES-256-CBC decryption in place; `data` is a whole number of blocks.
/// `iv` is updated, so a long stream can be decrypted in pieces.
pub fn aes256_cbc_decrypt(key: &[u8; 32], iv: &mut [u8; 16], data: &mut [u8]) -> Result<()> {
    use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
    if !data.len().is_multiple_of(16) {
        return Err(Error::corrupted("encrypted data that is not whole AES blocks"));
    }
    let cipher = aes::Aes256::new(GenericArray::from_slice(key));
    for block in data.chunks_exact_mut(16) {
        let ct: [u8; 16] = block.try_into().unwrap();
        let b = GenericArray::from_mut_slice(block);
        cipher.decrypt_block(b);
        for (p, v) in block.iter_mut().zip(iv.iter()) {
            *p ^= v;
        }
        *iv = ct;
    }
    Ok(())
}

/// AES-CBC decryption with a 128- or 256-bit key (see `aes256_cbc_decrypt`).
#[cfg(feature = "archive-rar")]
pub fn aes_cbc_decrypt(key: &[u8], iv: &mut [u8; 16], data: &mut [u8]) -> Result<()> {
    use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
    if key.len() == 32 {
        return aes256_cbc_decrypt(key.try_into().unwrap(), iv, data);
    }
    if !data.len().is_multiple_of(16) {
        return Err(Error::corrupted("encrypted data that is not whole AES blocks"));
    }
    let cipher = aes::Aes128::new_from_slice(key).map_err(|_| Error::invalid_input("an AES key size"))?;
    for block in data.chunks_exact_mut(16) {
        let ct: [u8; 16] = block.try_into().unwrap();
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (p, v) in block.iter_mut().zip(iv.iter()) {
            *p ^= v;
        }
        *iv = ct;
    }
    Ok(())
}

/// SHA-1 as RAR 3 computed it (see the module notes).
#[cfg(feature = "archive-rar")]
#[derive(Clone)]
struct Sha1Rar29 {
    state: [u32; 5],
    count: u64,
    buf: [u8; 64],
}

#[cfg(feature = "archive-rar")]
impl Sha1Rar29 {
    fn new() -> Self {
        Sha1Rar29 { state: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], count: 0, buf: [0; 64] }
    }

    /// One block; returns the last 16 words of the message schedule.
    fn transform(state: &mut [u32; 5], block: &[u8]) -> [u32; 16] {
        let mut w = [0u32; 80];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(c.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
        w[64..80].try_into().unwrap()
    }

    /// Hashes `data`; with `quirk`, whole blocks taken straight from it are
    /// overwritten with their message schedule, as RAR 3 did.
    fn update(&mut self, data: &mut [u8], quirk: bool) {
        let j = (self.count & 63) as usize;
        let len = data.len();
        self.count += len as u64;
        let mut i = 0;
        let mut j = j;
        if j + len > 63 {
            i = 64 - j;
            self.buf[j..].copy_from_slice(&data[..i]);
            let buf = self.buf;
            Self::transform(&mut self.state, &buf);
            while i + 63 < len {
                let w = Self::transform(&mut self.state, &data[i..i + 64]);
                if quirk {
                    for (k, v) in w.iter().enumerate() {
                        data[i + 4 * k..i + 4 * k + 4].copy_from_slice(&v.to_le_bytes());
                    }
                }
                i += 64;
            }
            j = 0;
        }
        self.buf[j..j + len - i].copy_from_slice(&data[i..]);
    }

    fn digest(&self) -> [u8; 20] {
        let mut c = self.clone();
        let bits = c.count * 8;
        c.update(&mut [0x80], false);
        while c.count % 64 != 56 {
            c.update(&mut [0], false);
        }
        c.update(&mut bits.to_be_bytes(), false);
        let mut out = [0u8; 20];
        for (o, s) in out.chunks_exact_mut(4).zip(c.state) {
            o.copy_from_slice(&s.to_be_bytes());
        }
        out
    }
}

/// The RAR 4 (RAR 3.x) AES-128 key and IV for `password` and `salt`.
#[cfg(feature = "archive-rar")]
pub fn keys_rar3(password: &str, salt: &[u8; 8]) -> ([u8; 16], [u8; 16]) {
    static CACHE: std::sync::OnceLock<Cache<([u8; 16], [u8; 16])>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let id = (salt.to_vec(), 3, password_hash(password));
    if let Some(k) = cache.lock().unwrap().get(&id) {
        return *k;
    }
    let mut raw: Vec<u8> = password.encode_utf16().take(127).flat_map(|c| c.to_le_bytes()).collect();
    raw.extend_from_slice(salt);
    const ROUNDS: u32 = 0x40000;
    let mut sha = Sha1Rar29::new();
    let mut iv = [0u8; 16];
    for i in 0..ROUNDS {
        sha.update(&mut raw, true);
        let mut n = [i as u8, (i >> 8) as u8, (i >> 16) as u8];
        sha.update(&mut n, true);
        if i % (ROUNDS / 16) == 0 {
            iv[(i / (ROUNDS / 16)) as usize] = sha.digest()[19];
        }
    }
    let d = sha.digest();
    let mut key = [0u8; 16];
    for i in 0..4 {
        for j in 0..4 {
            key[i * 4 + j] = d[i * 4 + 3 - j];
        }
    }
    cache.lock().unwrap().insert(id, (key, iv));
    (key, iv)
}

/// AES-256-CBC encryption in place (`data`: whole blocks).
#[cfg(feature = "archive-7z")]
pub fn aes256_cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) {
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    let cipher = aes::Aes256::new(GenericArray::from_slice(key));
    let mut prev = *iv;
    for block in data.chunks_exact_mut(16) {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
        prev = block.try_into().unwrap();
    }
}

/// Unpredictable bytes (for IVs): from the standard library's randomly
/// seeded hasher (the OS's randomness where there is one), a counter and,
/// natively, the time.
#[cfg(feature = "archive-7z")]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            h.write_u128(t.as_nanos());
        }
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

type Cache<V> = Mutex<HashMap<(Vec<u8>, u32, [u8; 32]), V>>;

fn password_hash(password: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(password.as_bytes()).into()
}

/// The 7z key for `password`.
#[cfg(feature = "archive-7z")]
pub fn key_7z(password: &str, salt: &[u8], cycles: u32) -> [u8; 32] {
    use sha2::Digest;
    static CACHE: std::sync::OnceLock<Cache<[u8; 32]>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let id = (salt.to_vec(), cycles, password_hash(password));
    if let Some(k) = cache.lock().unwrap().get(&id) {
        return *k;
    }
    let utf16: Vec<u8> = password.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let mut key = [0u8; 32];
    if cycles == 0x3F {
        // No hashing: salt and password, as they are.
        for (k, b) in key.iter_mut().zip(salt.iter().chain(&utf16)) {
            *k = *b;
        }
    } else {
        let mut buf = Vec::with_capacity(salt.len() + utf16.len() + 8);
        buf.extend_from_slice(salt);
        buf.extend_from_slice(&utf16);
        let at = buf.len();
        buf.extend_from_slice(&[0; 8]);
        let mut sha = sha2::Sha256::new();
        for i in 0..1u64 << cycles.min(30) {
            buf[at..].copy_from_slice(&i.to_le_bytes());
            sha.update(&buf);
        }
        key = sha.finalize().into();
    }
    cache.lock().unwrap().insert(id, key);
    key
}

/// RAR 5 keys: the AES key, the checksum key and the password check value.
#[cfg(feature = "archive-rar")]
#[derive(Clone, Copy)]
pub struct RarKeys {
    pub key: [u8; 32],
    pub hash_key: [u8; 32],
    pub check: [u8; 8],
}

/// The RAR 5 keys for `password`, salt and 2^`count` rounds.
#[cfg(feature = "archive-rar")]
pub fn keys_rar5(password: &str, salt: &[u8], count: u32) -> RarKeys {
    use hmac::{Hmac, Mac};
    static CACHE: std::sync::OnceLock<Cache<RarKeys>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let id = (salt.to_vec(), count, password_hash(password));
    if let Some(k) = cache.lock().unwrap().get(&id) {
        return *k;
    }
    type H = Hmac<sha2::Sha256>;
    let mac = <H as Mac>::new_from_slice(password.as_bytes()).expect("any key length");
    // PBKDF2 with one block, its running XOR read off after 2^count, then
    // 16 and 32 more rounds.
    let mut m = mac.clone();
    m.update(salt);
    m.update(&1u32.to_be_bytes());
    let mut u: [u8; 32] = m.finalize().into_bytes().into();
    let mut f = u;
    let mut out = [[0u8; 32]; 3];
    let mut rounds = (1u64 << count.min(24)) - 1;
    for o in &mut out {
        for _ in 0..rounds {
            let mut m = mac.clone();
            m.update(&u);
            u = m.finalize().into_bytes().into();
            for (a, b) in f.iter_mut().zip(&u) {
                *a ^= b;
            }
        }
        *o = f;
        rounds = 16;
    }
    let mut check = [0u8; 8];
    for (i, b) in out[2].iter().enumerate() {
        check[i % 8] ^= b;
    }
    let keys = RarKeys { key: out[0], hash_key: out[1], check };
    cache.lock().unwrap().insert(id, keys);
    keys
}

/// A RAR 5 CRC as stored when checksums are "tweaked" with the password
/// (so the CRC tells nothing about the content without it).
#[cfg(feature = "archive-rar")]
pub fn rar5_crc(hash_key: &[u8; 32], crc: u32) -> u32 {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(hash_key).expect("any key length");
    m.update(&crc.to_le_bytes());
    let d = m.finalize().into_bytes();
    let mut r = [0u8; 4];
    for (i, b) in d.iter().enumerate() {
        r[i & 3] ^= b;
    }
    u32::from_le_bytes(r)
}

#[cfg(all(test, feature = "archive-rar"))]
mod tests {
    use super::*;

    #[test]
    fn sha1_rar29_without_the_quirk_is_sha1() {
        use sha1::Digest;
        for n in [0usize, 3, 55, 56, 63, 64, 65, 200] {
            let mut data: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            let want: [u8; 20] = sha1::Sha1::digest(&data).into();
            let mut s = Sha1Rar29::new();
            s.update(&mut data, false);
            assert_eq!(s.digest(), want, "{n}");
        }
    }
}
