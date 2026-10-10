use crate::domain::errors::Result;

/// Free bytes on the filesystem the database lives on, as the process can see it.
pub trait FreeSpace: Send + Sync {
    fn free_bytes(&self) -> Result<u64>;
}
