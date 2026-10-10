//! How the embedding sweep reads the plaintext of a private row (decision 0027).
//!
//! A port rather than a function in the sweep, so the fork can put its own key order (the labelled
//! key, then the service key) in a file of its own instead of editing the engine's sweep.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::SealedReader;
use crate::crypto::kek::KeyProvider;
use crate::domain::errors::Result;

#[async_trait]
pub trait RowOpener: Send + Sync {
    /// Plaintext for each private row that opens. An id absent from the map did not open; the
    /// sweep counts it as one failure for that row.
    async fn open(&self, unit: &str, ids: &[uuid::Uuid]) -> Result<HashMap<uuid::Uuid, String>>;
}

/// The engine's opener: every private row is sealed under the one service KEK.
pub struct ServiceKeyOpener {
    pub reader: Arc<dyn SealedReader>,
    pub keys: Arc<dyn KeyProvider>,
}

#[async_trait]
impl RowOpener for ServiceKeyOpener {
    async fn open(&self, unit: &str, ids: &[uuid::Uuid]) -> Result<HashMap<uuid::Uuid, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The key lives for this call only, so no key stays resident between batches.
        let kek = self.keys.kek().await?;
        let mut opened = HashMap::new();
        for (id, sealed, kek_id) in self.reader.sealed_batch(unit, ids).await? {
            match crate::crypto::envelope::open(&kek, id, &sealed) {
                Ok(plaintext) => {
                    opened.insert(id, plaintext);
                }
                // `open` logged the stage. The kek id is what tells a rotation from one bad row.
                Err(_) => tracing::error!(
                    unit,
                    id = %id,
                    kek_id = kek_id.as_deref().unwrap_or("unknown"),
                    "embedding sweep could not open a private row"
                ),
            }
        }
        Ok(opened)
    }
}

/// For a store with no KEK provider. Pair it with `kek_verified: false`, so the sweep skips
/// private rows and reports `blocked: kek` rather than counting each one as a failure.
pub struct NoOpener;

#[async_trait]
impl RowOpener for NoOpener {
    async fn open(&self, _unit: &str, _ids: &[uuid::Uuid]) -> Result<HashMap<uuid::Uuid, String>> {
        Ok(HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;
    use crate::crypto::envelope::{seal, SealedContent};
    use crate::crypto::kek::Kek;

    struct FixedKey([u8; 32]);

    #[async_trait]
    impl KeyProvider for FixedKey {
        fn kek_id(&self) -> String {
            "test".into()
        }
        fn provider(&self) -> &'static str {
            "test"
        }
        async fn kek(&self) -> Result<Kek> {
            Ok(Zeroizing::new(self.0))
        }
    }

    struct Rows(Vec<(uuid::Uuid, SealedContent)>);

    #[async_trait]
    impl SealedReader for Rows {
        async fn sealed_batch(
            &self,
            _tenant: &str,
            ids: &[uuid::Uuid],
        ) -> Result<Vec<(uuid::Uuid, SealedContent, Option<String>)>> {
            Ok(self
                .0
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .map(|(id, s)| (*id, s.clone(), Some("test".into())))
                .collect())
        }
    }

    #[tokio::test]
    async fn the_service_key_opens_its_own_rows_and_leaves_out_the_rest() {
        let key = Zeroizing::new([7u8; 32]);
        let other_key = Zeroizing::new([9u8; 32]);
        let (good, foreign, missing) =
            (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let reader = Rows(vec![
            (good, seal(&key, good, "the plaintext").unwrap()),
            (foreign, seal(&other_key, foreign, "sealed under another key").unwrap()),
        ]);
        let opener =
            ServiceKeyOpener { reader: Arc::new(reader), keys: Arc::new(FixedKey([7u8; 32])) };

        let opened = opener.open("me", &[good, foreign, missing]).await.unwrap();

        assert_eq!(opened.len(), 1, "{opened:?}");
        assert_eq!(opened.get(&good).map(String::as_str), Some("the plaintext"));
    }

    #[tokio::test]
    async fn the_keyless_opener_opens_nothing() {
        let id = uuid::Uuid::new_v4();
        assert!(NoOpener.open("me", &[id]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_ids_reads_no_key() {
        struct NoKey;
        #[async_trait]
        impl KeyProvider for NoKey {
            fn kek_id(&self) -> String {
                "none".into()
            }
            fn provider(&self) -> &'static str {
                "none"
            }
            async fn kek(&self) -> Result<Kek> {
                panic!("an empty request must not read the key")
            }
        }
        let opener = ServiceKeyOpener { reader: Arc::new(Rows(vec![])), keys: Arc::new(NoKey) };
        assert!(opener.open("me", &[]).await.unwrap().is_empty());
    }
}
