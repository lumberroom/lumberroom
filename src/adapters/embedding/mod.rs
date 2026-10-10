//! Embedders. Three implementations behind one trait, chosen by configuration. The same shape
//! the TypeScript service used, and the reason a provider swap has never needed a code change.

mod hash;
mod local;
mod remote;

pub use hash::HashEmbedder;
pub use local::LocalEmbedder;
pub use remote::RemoteEmbedder;

use std::sync::Arc;

use crate::config::{Config, EmbedProvider};
use crate::domain::errors::Result;
use crate::ports::Embedder;

pub fn create(cfg: &Config) -> Result<Arc<dyn Embedder>> {
    match cfg.embed.provider {
        EmbedProvider::Local => {
            Ok(Arc::new(LocalEmbedder::new(&cfg.embed.model, cfg.embed.dim, &cfg.embed.cache_dir)?))
        }
        EmbedProvider::Hash => Ok(Arc::new(HashEmbedder::new(cfg.embed.dim))),
        EmbedProvider::Openai => {
            Ok(Arc::new(RemoteEmbedder::new(&cfg.embed.model, cfg.embed.dim, &cfg.embed.remote)?))
        }
    }
}
