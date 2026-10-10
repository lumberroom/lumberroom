//! Downloads the embedding weights at image build time.
//!
//! A first-request download on the VM is a silent failure mode when egress is locked down, so the
//! weights ship inside the image instead. Same reasoning as the previous build; different runtime.

/// The image carries the weights while either block names a local model. A previous block on
/// `local` keeps its weights in the image through the whole rollback window.
fn should_download(provider: &str, previous_provider: Option<&str>) -> bool {
    provider == "local" || previous_provider == Some("local")
}

fn main() {
    let provider = std::env::var("EMBED_PROVIDER").unwrap_or_else(|_| "local".into());
    let previous = std::env::var("EMBED_PREVIOUS_PROVIDER").ok().filter(|p| !p.is_empty());
    if !should_download(&provider, previous.as_deref()) {
        println!(
            "prefetch skipped: EMBED_PROVIDER={provider} EMBED_PREVIOUS_PROVIDER={}",
            previous.as_deref().unwrap_or("")
        );
        return;
    }

    let model = std::env::var("EMBED_MODEL").unwrap_or_else(|_| "Xenova/bge-base-en-v1.5".into());
    let cache = std::env::var("MODEL_CACHE_DIR").unwrap_or_else(|_| "/models".into());

    let started = std::time::Instant::now();
    let opts = fastembed::InitOptions::new(fastembed::EmbeddingModel::BGEBaseENV15Q)
        .with_cache_dir(cache.clone().into())
        .with_show_download_progress(false);

    let mut embedder = match fastembed::TextEmbedding::try_new(opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("prefetch failed: {e}");
            std::process::exit(1);
        }
    };

    match embedder.embed(vec!["prefetch"], None) {
        Ok(v) => println!(
            "prefetched {model} into {cache}: {} dims in {}ms",
            v[0].len(),
            started.elapsed().as_millis()
        ),
        Err(e) => {
            eprintln!("prefetch produced no vector: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::should_download;

    #[test]
    fn prefetch_runs_when_either_block_is_local() {
        assert!(should_download("local", None));
        assert!(should_download("local", Some("openai")));
        assert!(should_download("openai", Some("local")));
        assert!(!should_download("openai", None));
        assert!(!should_download("openai", Some("openai")));
        assert!(!should_download("hash", Some("hash")));
    }
}
