use crate::error::{MmapError, Result, checked_add};
use std::mem::size_of;

const LENGTH_FIELDS_SIZE: usize = size_of::<u32>() * 3;

#[derive(Debug, Clone, PartialEq)]
pub struct RawEntry<'a> {
    bytes: &'a [u8],
    family_len: usize,
    sample_len: usize,
    labels_len: usize,
}

impl<'a> RawEntry<'a> {
    pub fn from_slice(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < LENGTH_FIELDS_SIZE {
            return Err(MmapError::OutOfBounds {
                offset: LENGTH_FIELDS_SIZE,
                len: bytes.len(),
            });
        }

        let family_len =
            u32::from_ne_bytes(bytes[0..4].try_into().expect("slice len checked")) as usize;
        let sample_len =
            u32::from_ne_bytes(bytes[4..8].try_into().expect("slice len checked")) as usize;
        let labels_len =
            u32::from_ne_bytes(bytes[8..12].try_into().expect("slice len checked")) as usize;
        Self::check_field_len(family_len)?;
        Self::check_field_len(sample_len)?;
        Self::check_field_len(labels_len)?;

        let total_len = Self::calc_total_len(family_len, sample_len, labels_len)?;
        if total_len > bytes.len() {
            return Err(MmapError::OutOfBounds {
                offset: total_len,
                len: bytes.len(),
            });
        }

        Ok(Self {
            bytes: &bytes[LENGTH_FIELDS_SIZE..total_len],
            family_len,
            sample_len,
            labels_len,
        })
    }

    pub fn save(
        dst: &mut [u8],
        family: &[u8],
        sample: &[u8],
        labels: &[u8],
        value: f64,
    ) -> Result<usize> {
        let total_len = Self::calc_total_len(family.len(), sample.len(), labels.len())?;
        if total_len > dst.len() {
            return Err(MmapError::EntryTooLarge {
                needed: total_len,
                available: dst.len(),
            });
        }

        dst[0..4].copy_from_slice(&(family.len() as u32).to_ne_bytes());
        dst[4..8].copy_from_slice(&(sample.len() as u32).to_ne_bytes());
        dst[8..12].copy_from_slice(&(labels.len() as u32).to_ne_bytes());

        let mut pos = LENGTH_FIELDS_SIZE;
        dst[pos..pos + family.len()].copy_from_slice(family);
        pos += family.len();

        dst[pos..pos + sample.len()].copy_from_slice(sample);
        pos += sample.len();

        dst[pos..pos + labels.len()].copy_from_slice(labels);
        pos += labels.len();

        let pad_len = Self::padding_len(family.len(), sample.len(), labels.len())?;
        dst[pos..pos + pad_len].fill(b' ');
        pos += pad_len;

        dst[pos..pos + 8].copy_from_slice(&value.to_ne_bytes());

        Self::calc_value_offset(family.len(), sample.len(), labels.len())
    }

    pub fn value(&self) -> f64 {
        let offset = self
            .body_len()
            .checked_add(
                Self::padding_len(self.family_len, self.sample_len, self.labels_len)
                    .expect("entry validated"),
            )
            .expect("entry validated");
        let value_bytes: [u8; 8] = self.bytes[offset..offset + 8]
            .try_into()
            .expect("entry validated");
        f64::from_ne_bytes(value_bytes)
    }

    pub fn family_bytes(&self) -> &[u8] {
        &self.bytes[..self.family_len]
    }

    pub fn sample_bytes(&self) -> &[u8] {
        let start = self.family_len;
        let end = start + self.sample_len;
        &self.bytes[start..end]
    }

    pub fn labels_bytes(&self) -> &[u8] {
        let start = self.family_len + self.sample_len;
        let end = start + self.labels_len;
        &self.bytes[start..end]
    }

    pub fn total_len(&self) -> usize {
        Self::calc_total_len(self.family_len, self.sample_len, self.labels_len)
            .expect("field lengths validated")
    }

    pub fn calc_total_len(
        family_len: usize,
        sample_len: usize,
        labels_len: usize,
    ) -> Result<usize> {
        checked_add(
            Self::calc_value_offset(family_len, sample_len, labels_len)?,
            size_of::<f64>(),
        )
    }

    pub fn calc_value_offset(
        family_len: usize,
        sample_len: usize,
        labels_len: usize,
    ) -> Result<usize> {
        Self::check_field_len(family_len)?;
        Self::check_field_len(sample_len)?;
        Self::check_field_len(labels_len)?;
        let len = checked_add(
            LENGTH_FIELDS_SIZE,
            Self::body_len_from_parts(family_len, sample_len, labels_len)?,
        )?;
        checked_add(len, Self::padding_len(family_len, sample_len, labels_len)?)
    }

    pub fn padding_len(family_len: usize, sample_len: usize, labels_len: usize) -> Result<usize> {
        let unaligned = checked_add(
            LENGTH_FIELDS_SIZE,
            Self::body_len_from_parts(family_len, sample_len, labels_len)?,
        )?;
        Ok((8 - (unaligned % 8)) % 8)
    }

    fn body_len(&self) -> usize {
        self.family_len
            .checked_add(self.sample_len)
            .and_then(|len| len.checked_add(self.labels_len))
            .expect("entry validated")
    }

    fn body_len_from_parts(
        family_len: usize,
        sample_len: usize,
        labels_len: usize,
    ) -> Result<usize> {
        let len = checked_add(family_len, sample_len)?;
        checked_add(len, labels_len)
    }

    fn check_field_len(field_len: usize) -> Result<()> {
        if field_len as u64 > u32::MAX as u64 {
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
        let family = b"http_requests_total";
        let sample = b"http_requests_total";
        let labels = br#"{a="1"}"#;
        let value = 42.5f64;

        let total = RawEntry::calc_total_len(family.len(), sample.len(), labels.len()).unwrap();
        let mut buf = vec![0; total];
        let value_offset = RawEntry::save(&mut buf, family, sample, labels, value).unwrap();
        assert!(value_offset > 0);

        let parsed = RawEntry::from_slice(&buf).unwrap();
        assert_eq!(parsed.family_bytes(), family);
        assert_eq!(parsed.sample_bytes(), sample);
        assert_eq!(parsed.labels_bytes(), labels);
        assert_eq!(parsed.value(), value);
    }
}
