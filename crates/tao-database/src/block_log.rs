//! Append-only, length-prefixed block log.
//!
//! Record framing: `[u32 big-endian length][payload bytes]`. Appends are
//! flushed to disk; reads replay the whole file sequentially.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tao_core::error::{Result, TaoError};

/// A durable append-only log of serialized blocks.
pub struct BlockLog {
    path: PathBuf,
    writer: Mutex<File>,
}

impl BlockLog {
    /// Open (creating if absent) the block log at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
        })
    }

    /// Append one serialized block record, flushing to disk.
    pub fn append(&self, payload: &[u8]) -> Result<()> {
        let len: u32 = payload
            .len()
            .try_into()
            .map_err(|_| TaoError::Storage("block too large for log record".into()))?;
        let mut file = self.writer.lock().expect("block log mutex poisoned");
        file.write_all(&len.to_be_bytes())?;
        file.write_all(payload)?;
        file.flush()?;
        Ok(())
    }

    /// Atomically replace the entire log with `records` (compaction). Writes a
    /// temp file and renames it over the log, so a crash leaves either the old
    /// or the new log intact, never a partial one. The internal writer is
    /// repointed at the compacted file for subsequent appends.
    pub fn replace_all(&self, records: &[Vec<u8>]) -> Result<()> {
        let mut writer = self.writer.lock().expect("block log mutex poisoned");
        let tmp = self.path.with_extension("log.compact");
        let mut temp = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        for payload in records {
            let len: u32 = payload
                .len()
                .try_into()
                .map_err(|_| TaoError::Storage("block too large for log record".into()))?;
            temp.write_all(&len.to_be_bytes())?;
            temp.write_all(payload)?;
        }
        temp.flush()?;
        std::fs::rename(&tmp, &self.path)?;
        // Repoint the append handle at the compacted file.
        *writer = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        Ok(())
    }

    /// Read every record in order. Returns the raw payloads (block bytes).
    pub fn read_all(&self) -> Result<Vec<Vec<u8>>> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();
        loop {
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            reader
                .read_exact(&mut payload)
                .map_err(|e| TaoError::Storage(format!("truncated block log record: {e}")))?;
            records.push(payload);
        }
        Ok(records)
    }

    /// Number of records currently in the log.
    pub fn len(&self) -> Result<usize> {
        Ok(self.read_all()?.len())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_roundtrips() {
        let dir = std::env::temp_dir().join(format!("tao-blocklog-{}", std::process::id()));
        let path = dir.join("blocks.log");
        let _ = std::fs::remove_file(&path);

        let log = BlockLog::open(&path).unwrap();
        log.append(&[1, 2, 3]).unwrap();
        log.append(&[]).unwrap();
        log.append(&[9, 8, 7, 6]).unwrap();

        let all = log.read_all().unwrap();
        assert_eq!(all, vec![vec![1, 2, 3], vec![], vec![9, 8, 7, 6]]);

        // Reopen and confirm durability + continued appends.
        drop(log);
        let log = BlockLog::open(&path).unwrap();
        assert_eq!(log.len().unwrap(), 3);
        log.append(&[42]).unwrap();
        assert_eq!(log.read_all().unwrap().last().unwrap(), &vec![42]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
