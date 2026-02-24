use crate::error::{MmapError, Result, checked_add};
use std::mem::size_of;

#[derive(Debug, Clone, PartialEq)]
pub struct RawEntry<'a> {
    bytes: &'a [u8],
    encoded_len: usize,
}

impl<'a> RawEntry<'a> {
    pub fn from_slice(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < size_of::<u32>() {
            return Err(MmapError::OutOfBounds {
                offset: size_of::<u32>(),
                len: bytes.len(),
            });
        }

        let encoded_len = u32::from_ne_bytes(bytes[0..4].try_into().expect("slice len checked"));
        let encoded_len = encoded_len as usize;
        Self::check_encoded_len(encoded_len)?;

        let total_len = Self::calc_total_len(encoded_len)?;
        if total_len > bytes.len() {
            return Err(MmapError::OutOfBounds {
                offset: total_len,
                len: bytes.len(),
            });
        }

        Ok(Self {
            bytes: &bytes[4..total_len],
            encoded_len,
        })
    }

    pub fn save(dst: &mut [u8], key: &[u8], value: f64) -> Result<usize> {
        let total_len = Self::calc_total_len(key.len())?;
        if total_len > dst.len() {
            return Err(MmapError::EntryTooLarge {
                needed: total_len,
                available: dst.len(),
            });
        }

        let key_len = key.len() as u32;
        dst[0..4].copy_from_slice(&key_len.to_ne_bytes());

        let mut pos = 4;
        dst[pos..pos + key.len()].copy_from_slice(key);
        pos += key.len();

        let pad_len = Self::padding_len(key.len());
        dst[pos..pos + pad_len].fill(b' ');
        pos += pad_len;

        dst[pos..pos + 8].copy_from_slice(&value.to_ne_bytes());

        Self::calc_value_offset(key.len())
    }

    pub fn value(&self) -> f64 {
        let offset = self.encoded_len + Self::padding_len(self.encoded_len);
        let value_bytes: [u8; 8] = self.bytes[offset..offset + 8]
            .try_into()
            .expect("entry validated");
        f64::from_ne_bytes(value_bytes)
    }

    pub fn json(&self) -> &[u8] {
        &self.bytes[..self.encoded_len]
    }

    pub fn total_len(&self) -> usize {
        Self::calc_total_len(self.encoded_len).expect("encoded len validated")
    }

    pub fn calc_total_len(encoded_len: usize) -> Result<usize> {
        checked_add(Self::calc_value_offset(encoded_len)?, size_of::<f64>())
    }

    pub fn calc_value_offset(encoded_len: usize) -> Result<usize> {
        Self::check_encoded_len(encoded_len)?;
        checked_add(
            size_of::<u32>() + encoded_len,
            Self::padding_len(encoded_len),
        )
    }

    pub fn padding_len(encoded_len: usize) -> usize {
        8 - ((size_of::<u32>() + encoded_len) % 8)
    }

    fn check_encoded_len(encoded_len: usize) -> Result<()> {
        if encoded_len as u64 > i32::MAX as u64 {
            return Err(MmapError::KeyTooLong);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::RawEntry;

    #[test]
    fn roundtrip_entry() {
        let key = br#"[\"family\",\"metric\",[\"a\"],[1]]"#;
        let value = 42.5f64;

        let total = RawEntry::calc_total_len(key.len()).unwrap();
        let mut buf = vec![0; total];
        let value_offset = RawEntry::save(&mut buf, key, value).unwrap();
        assert!(value_offset > 0);

        let parsed = RawEntry::from_slice(&buf).unwrap();
        assert_eq!(parsed.json(), key);
        assert_eq!(parsed.value(), value);
    }
}
