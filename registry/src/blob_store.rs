use std::{
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use thiserror::Error;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobStoreError>;
    async fn get(&self, key: &str) -> Result<Vec<u8>, BlobStoreError>;
    async fn exists(&self, key: &str) -> Result<bool, BlobStoreError>;
    async fn delete(&self, key: &str) -> Result<(), BlobStoreError>;
}

#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, BlobStoreError> {
        Ok(self.root.join(validate_key(key)?))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), BlobStoreError> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
        tokio::fs::write(&tmp_path, bytes).await?;
        if let Err(err) = tokio::fs::rename(&tmp_path, &path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(err.into());
        }

        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, BlobStoreError> {
        let path = self.path_for(key)?;
        tokio::fs::read(path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                BlobStoreError::NotFound
            } else {
                BlobStoreError::Io(err)
            }
        })
    }

    async fn exists(&self, key: &str) -> Result<bool, BlobStoreError> {
        let path = self.path_for(key)?;
        Ok(tokio::fs::try_exists(path).await?)
    }

    async fn delete(&self, key: &str) -> Result<(), BlobStoreError> {
        let path = self.path_for(key)?;
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn validate_key(key: &str) -> Result<PathBuf, BlobStoreError> {
    if key.is_empty() {
        return Err(BlobStoreError::InvalidKey);
    }

    let path = Path::new(key);
    if path.is_absolute() {
        return Err(BlobStoreError::InvalidKey);
    }

    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(BlobStoreError::InvalidKey),
        }
    }

    if clean.as_os_str().is_empty() {
        return Err(BlobStoreError::InvalidKey);
    }

    Ok(clean)
}

#[derive(Debug, Error)]
pub enum BlobStoreError {
    #[error("invalid blob storage key")]
    InvalidKey,
    #[error("blob not found")]
    NotFound,
    #[error("blob store I/O error")]
    Io(#[from] std::io::Error),
    #[error("blob store backend error: {0}")]
    Backend(String),
}

#[cfg(test)]
mod tests {
    use super::validate_key;

    #[test]
    fn validate_key_rejects_absolute_and_parent_paths() {
        assert!(validate_key("").is_err());
        assert!(validate_key("/archive.tgz").is_err());
        assert!(validate_key("../archive.tgz").is_err());
        assert!(validate_key("archives/../archive.tgz").is_err());
    }
}
