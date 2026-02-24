use crate::error::{MmapError, Result, checked_add};
use crate::raw_entry::RawEntry;
use memmap2::{MmapMut, MmapOptions};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub const HEADER_SIZE: usize = 8;

#[derive(Debug)]
pub struct MmapMetricStore {
    path: PathBuf,
    file: File,
    map: MmapMut,
    positions: HashMap<String, usize>,
}

impl MmapMetricStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let mut this = Self::with_file(path, file)?;
        this.rebuild_index()?;
        Ok(this)
    }

    pub fn get(&mut self, key_json: &str) -> Result<f64> {
        let used = self.used()?;
        if let Some(&offset) = self.positions.get(key_json) {
            if offset + 8 > used {
                return Err(MmapError::OutOfBounds {
                    offset: offset + 8,
                    len: used,
                });
            }
            return self.load_value(offset);
        }
        Ok(0.0)
    }

    pub fn increment(&mut self, key_json: &str, by: f64) -> Result<f64> {
        self.upsert(key_json, |v| v + by)
    }

    pub fn set(&mut self, key_json: &str, value: f64) -> Result<f64> {
        self.upsert(key_json, |_| value)
    }

    pub fn used(&self) -> Result<usize> {
        if self.map.len() < HEADER_SIZE {
            return Ok(HEADER_SIZE);
        }
        let used =
            u32::from_ne_bytes(self.map[0..4].try_into().expect("slice len checked")) as usize;
        if used == 0 {
            return Ok(HEADER_SIZE);
        }
        if used > self.map.len() {
            return Err(MmapError::InvalidUsed {
                used,
                len: self.map.len(),
            });
        }
        Ok(used)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.map.flush()?;
        Ok(())
    }

    fn with_file(path: PathBuf, file: File) -> Result<Self> {
        let mut file_len = file.metadata()?.len() as usize;
        if file_len < HEADER_SIZE {
            let size = page_aligned_size(HEADER_SIZE)?;
            file.set_len(size as u64)?;
            file_len = size;
        }

        let map = unsafe { MmapOptions::new().len(file_len).map_mut(&file)? };
        let mut this = Self {
            path,
            file,
            map,
            positions: HashMap::new(),
        };

        if this.read_used_header_raw() == 0 {
            this.write_used_header(HEADER_SIZE)?;
        }
        Ok(this)
    }

    fn rebuild_index(&mut self) -> Result<()> {
        self.positions.clear();
        let used = self.used()?;
        let mut pos = HEADER_SIZE;
        while pos + 4 <= used {
            let entry = RawEntry::from_slice(&self.map[pos..used])?;
            let value_offset = checked_add(pos, RawEntry::calc_value_offset(entry.json().len())?)?;
            let key = std::str::from_utf8(entry.json())?.to_owned();
            self.positions.insert(key, value_offset);
            pos = checked_add(pos, entry.total_len())?;
        }
        Ok(())
    }

    fn upsert<F>(&mut self, key_json: &str, f: F) -> Result<f64>
    where
        F: FnOnce(f64) -> f64,
    {
        if let Some(&offset) = self.positions.get(key_json) {
            let current = self.load_value(offset)?;
            let new_value = f(current);
            self.save_value(offset, new_value)?;
            return Ok(new_value);
        }

        let used = self.used()?;
        let entry_len = RawEntry::calc_total_len(key_json.len())?;
        let new_used = checked_add(used, entry_len)?;
        self.ensure_capacity(new_used)?;

        let start = used;
        let end = new_used;
        let value = f(0.0);
        let value_offset = RawEntry::save(&mut self.map[start..end], key_json.as_bytes(), value)?;
        let absolute_value_offset = checked_add(start, value_offset)?;

        self.write_used_header(new_used)?;
        self.positions
            .insert(key_json.to_owned(), absolute_value_offset);
        Ok(value)
    }

    fn ensure_capacity(&mut self, needed: usize) -> Result<()> {
        if self.map.len() >= needed {
            return Ok(());
        }

        self.flush()?;
        let mut new_len = self.map.len().max(HEADER_SIZE);
        while new_len < needed {
            new_len = checked_add(new_len, new_len)?;
        }
        new_len = page_aligned_size(new_len)?;

        self.file.set_len(new_len as u64)?;
        self.map.flush()?;
        self.map = unsafe { MmapOptions::new().len(new_len).map_mut(&self.file)? };
        Ok(())
    }

    fn load_value(&self, offset: usize) -> Result<f64> {
        if offset + 8 > self.map.len() {
            return Err(MmapError::OutOfBounds {
                offset: offset + 8,
                len: self.map.len(),
            });
        }
        let bytes: [u8; 8] = self.map[offset..offset + 8]
            .try_into()
            .expect("slice len checked");
        Ok(f64::from_ne_bytes(bytes))
    }

    fn save_value(&mut self, offset: usize, value: f64) -> Result<()> {
        if offset + 8 > self.map.len() {
            return Err(MmapError::OutOfBounds {
                offset: offset + 8,
                len: self.map.len(),
            });
        }
        self.map[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
        Ok(())
    }

    fn read_used_header_raw(&self) -> u32 {
        if self.map.len() < 4 {
            return 0;
        }
        u32::from_ne_bytes(self.map[0..4].try_into().expect("slice len checked"))
    }

    fn write_used_header(&mut self, used: usize) -> Result<()> {
        let used = u32::try_from(used).map_err(|_| MmapError::Overflow)?;
        self.map[0..4].copy_from_slice(&used.to_ne_bytes());
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn page_aligned_size(min: usize) -> Result<usize> {
    let page_size = page_size::get();
    let rem = min % page_size;
    if rem == 0 {
        Ok(min)
    } else {
        checked_add(min, page_size - rem)
    }
}

mod page_size {
    pub fn get() -> usize {
        #[cfg(unix)]
        {
            let val = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if val > 0 {
                return val as usize;
            }
        }
        4096
    }
}

#[cfg(test)]
mod test {
    use super::MmapMetricStore;
    use tempfile::tempdir;

    #[test]
    fn can_create_and_update_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("counter.db");
        let key = r#"["demo_counter","demo_counter",["k"],["1"]]"#;

        let mut store = MmapMetricStore::open(&path).unwrap();
        assert_eq!(store.get(key).unwrap(), 0.0);
        assert_eq!(store.increment(key, 2.0).unwrap(), 2.0);
        assert_eq!(store.increment(key, 3.0).unwrap(), 5.0);
        assert_eq!(store.set(key, 9.0).unwrap(), 9.0);
        assert_eq!(store.get(key).unwrap(), 9.0);
        store.flush().unwrap();
    }
}
