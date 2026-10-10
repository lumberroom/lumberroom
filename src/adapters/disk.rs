//! Free space on the filesystem the database lives on, for the embedding migration's disk floor.

use crate::domain::errors::{DomainError, Result};
use crate::ports::free_space::FreeSpace;

pub struct StatvfsFreeSpace {
    pub path: String,
}

impl FreeSpace for StatvfsFreeSpace {
    /// `f_bavail`, not `f_bfree`: the blocks an unprivileged writer can use, which is what Postgres
    /// sees. `f_bfree` also counts the root-reserved share and overstates the room by that much.
    fn free_bytes(&self) -> Result<u64> {
        let stat = rustix::fs::statvfs(&self.path).map_err(|e| {
            DomainError::internal(format!("could not read free space at {}", self.path))
                .with_source(e)
        })?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_reads_the_temp_dir() {
        let free = StatvfsFreeSpace { path: std::env::temp_dir().to_string_lossy().into_owned() };
        assert!(free.free_bytes().unwrap() > 0);
    }

    #[test]
    fn a_missing_path_is_an_error_naming_it() {
        let free = StatvfsFreeSpace { path: "/definitely/not/a/path".into() };
        let err = free.free_bytes().unwrap_err();
        assert!(err.to_string().contains("/definitely/not/a/path"), "{err}");
    }
}
