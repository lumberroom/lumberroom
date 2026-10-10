//! Embedders. Three implementations behind one trait, chosen by configuration. The same shape
//! the TypeScript service used, and the reason a provider swap has never needed a code change.

mod hash;
mod local;
mod remote;

pub use hash::HashEmbedder;
pub use local::LocalEmbedder;
pub use remote::RemoteEmbedder;

use std::sync::Arc;

use crate::config::{Config, EmbedProvider, EmbedderSpec};
use crate::domain::errors::Result;
use crate::ports::Embedder;

pub fn create(cfg: &Config) -> Result<Arc<dyn Embedder>> {
    create_spec(&cfg.embed.current_spec())
}

/// Builds the embedder one configured block describes. The `EMBED_*` block and the
/// `EMBED_PREVIOUS_*` block both come through here. A model that fails to load is an error, never a
/// reason to build a different one (decision 0028).
pub fn create_spec(spec: &EmbedderSpec) -> Result<Arc<dyn Embedder>> {
    match spec.provider {
        EmbedProvider::Local => {
            Ok(Arc::new(LocalEmbedder::new(&spec.model, spec.dim, &spec.cache_dir)?))
        }
        EmbedProvider::Hash => Ok(Arc::new(HashEmbedder::new(spec.dim))),
        EmbedProvider::Openai => {
            Ok(Arc::new(RemoteEmbedder::new(&spec.model, spec.dim, &spec.remote)?))
        }
    }
}

/// The id the embedder `spec` describes reports, computed without building it. Ids are stored data
/// (`embedding_model`), so this and each adapter's `id()` must agree byte for byte. The local and
/// remote adapters call the same helpers; `HashEmbedder::id` spells its format itself, and the test
/// below holds the two together.
pub fn id_for(spec: &EmbedderSpec) -> String {
    match spec.provider {
        EmbedProvider::Local => local::id(&spec.model),
        EmbedProvider::Openai => remote::id(&spec.model),
        EmbedProvider::Hash => format!("hash-v1-{}", spec.dim),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteEmbedConfig;

    fn spec(provider: EmbedProvider, model: &str, dim: usize) -> EmbedderSpec {
        EmbedderSpec {
            provider,
            model: model.into(),
            dim,
            cache_dir: "/nonexistent".into(),
            remote: RemoteEmbedConfig {
                base_url: "http://127.0.0.1:1/v1".into(),
                timeout_secs: 1,
                ..Default::default()
            },
        }
    }

    #[test]
    fn id_for_matches_each_embedder_id() {
        // Hash and remote build with no network and no weights, so their real id() is compared.
        for dim in [4, 768] {
            let s = spec(EmbedProvider::Hash, "ignored", dim);
            assert_eq!(id_for(&s), create_spec(&s).unwrap().id());
            assert_eq!(id_for(&s), format!("hash-v1-{dim}"));
        }
        for model in ["google/embeddinggemma-2", "text-embedding-3-small", "m"] {
            let s = spec(EmbedProvider::Openai, model, 768);
            assert_eq!(id_for(&s), create_spec(&s).unwrap().id());
            assert_eq!(id_for(&s), format!("openai:{model}"));
        }
    }

    #[test]
    fn local_ids_keep_their_stored_format() {
        // LocalEmbedder::new loads weights, so this test cannot build one. Production rows carry
        // these exact strings in embedding_model; a change here strands every one of them.
        for (model, want) in [
            ("Xenova/bge-base-en-v1.5", "Xenova/bge-base-en-v1.5@q8"),
            ("bge-small-en-v1.5", "bge-small-en-v1.5@q8"),
        ] {
            assert_eq!(id_for(&spec(EmbedProvider::Local, model, 768)), want);
            assert_eq!(local::id(model), want);
        }
    }
}
