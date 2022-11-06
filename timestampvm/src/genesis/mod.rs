use std::{
    fmt,
    fs::{self, File},
    io::{self, Error, ErrorKind, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct Genesis {
    pub data: String,
}

impl Default for Genesis {
    fn default() -> Self {
        Self::default()
    }
}

impl Genesis {
    pub fn default() -> Self {
        Self {
            data: String::from("Hello from Rust VM!"),
        }
    }

    pub fn to_slice(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&self).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to serialize Genesis to JSON bytes {}", e),
            )
        })
    }

    pub fn from_slice<S>(d: S) -> io::Result<Self>
    where
        S: AsRef<[u8]>,
    {
        serde_json::from_slice(d.as_ref())
            .map_err(|e| Error::new(ErrorKind::Other, format!("failed to decode {}", e)))
    }

    pub fn sync(&self, file_path: &str) -> io::Result<()> {
        log::info!("syncing genesis to '{}'", file_path);

        let path = Path::new(file_path);
        let parent_dir = path.parent().unwrap();
        fs::create_dir_all(parent_dir)?;

        let d = serde_json::to_vec(&self).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to serialize genesis info to JSON {}", e),
            )
        })?;

        let mut f = File::create(&file_path)?;
        f.write_all(&d)?;

        Ok(())
    }
}

impl fmt::Display for Genesis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_string(&self).unwrap();
        write!(f, "{}", s)
    }
}
