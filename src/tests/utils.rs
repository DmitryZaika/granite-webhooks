use bytes::Bytes;
use std::fs;
use std::path::Path;

pub fn read_file_as_bytes<P: AsRef<Path>>(path: P) -> std::io::Result<Bytes> {
    let data = fs::read(path)?;
    Ok(Bytes::from(data))
}
