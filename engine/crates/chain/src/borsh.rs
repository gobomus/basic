//! Minimal little-endian Borsh reader. Decoders read only the fields they need,
//! in order, and tolerate trailing (appended) fields — Pump appends fields to
//! events and accounts over time.

use solana_sdk::pubkey::Pubkey;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("borsh: unexpected end of data at offset {0}")]
pub struct Eof(pub usize);

pub struct Reader<'a> {
    data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Eof> {
        if self.remaining() < n {
            return Err(Eof(self.pos));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn skip(&mut self, n: usize) -> Result<(), Eof> {
        self.bytes(n).map(|_| ())
    }
    pub fn u8(&mut self) -> Result<u8, Eof> {
        Ok(self.bytes(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, Eof> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16, Eof> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, Eof> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, Eof> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, Eof> {
        Ok(i64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    pub fn u128(&mut self) -> Result<u128, Eof> {
        Ok(u128::from_le_bytes(self.bytes(16)?.try_into().unwrap()))
    }
    pub fn i128(&mut self) -> Result<i128, Eof> {
        Ok(i128::from_le_bytes(self.bytes(16)?.try_into().unwrap()))
    }
    pub fn pubkey(&mut self) -> Result<Pubkey, Eof> {
        Ok(Pubkey::new_from_array(self.bytes(32)?.try_into().unwrap()))
    }
    pub fn string(&mut self) -> Result<String, Eof> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }
    /// Optional trailing field: `None` if the data ends before it.
    pub fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, Eof>) -> Option<T> {
        let save = self.pos;
        match f(self) {
            Ok(v) => Some(v),
            Err(_) => {
                self.pos = save;
                None
            }
        }
    }
}

/// Anchor discriminator: sha256("<namespace>:<name>")[..8].
pub fn anchor_disc(namespace: &str, name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("{namespace}:{name}").as_bytes());
    h[..8].try_into().unwrap()
}

/// Prefix Anchor puts on self-CPI event instructions (`emit_cpi!`).
pub const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
