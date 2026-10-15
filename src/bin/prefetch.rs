//! Downloads the embedding weights at image build time.
//!
//! A first-request download on the VM is a silent failure mode when egress is locked down, so the
//! weights ship inside the image instead. Same reasoning as the previous build; different runtime.

use lumberroom_server::adapters::embedding::variant_for;

/// The image carries the weights while either block names a local model. A previous block on
/// `local` keeps its weights in the image through the whole rollback window.
fn should_download(provider: &str, previous_provider: Option<&str>) -> bool {
    provider == "local" || previous_provider == Some("local")
}

/// Every model name a local block configures, current block first, each once. A previous block
/// without a model is skipped here; the server refuses to boot on it anyway.
fn local_models(
    provider: &str,
    model: &str,
    previous_provider: Option<&str>,
    previous_model: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    if provider == "local" {
        out.push(model.to_string());
    }
    if let (Some("local"), Some(previous)) = (previous_provider, previous_model) {
        if !out.iter().any(|m| m == previous) {
            out.push(previous.to_string());
        }
    }
    out
}

/// `config::env`: an empty value takes the default. The server boots on what config reads, so the
/// image must bake the same name.
fn or_default(raw: Option<String>, fallback: &str) -> String {
    raw.filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.to_string())
}

/// `config::env_opt`: trimmed, and a blank value reads as unset.
fn trimmed(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn main() {
    let var = |key: &str| std::env::var(key).ok();
    let provider = or_default(var("EMBED_PROVIDER"), "local");
    let previous = trimmed(var("EMBED_PREVIOUS_PROVIDER"));
    if !should_download(&provider, previous.as_deref()) {
        println!(
            "prefetch skipped: EMBED_PROVIDER={provider} EMBED_PREVIOUS_PROVIDER={}",
            previous.as_deref().unwrap_or("")
        );
        return;
    }

    let model = or_default(var("EMBED_MODEL"), "Xenova/bge-base-en-v1.5");
    let previous_model = trimmed(var("EMBED_PREVIOUS_MODEL"));
    let cache = or_default(var("MODEL_CACHE_DIR"), "/models");

    for name in local_models(&provider, &model, previous.as_deref(), previous_model.as_deref()) {
        fetch(&name, &cache);
    }
}

fn fetch(name: &str, cache: &str) {
    let variant = match variant_for(name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("prefetch failed: {}", e.client_message());
            std::process::exit(1);
        }
    };
    let started = std::time::Instant::now();
    let opts = fastembed::TextInitOptions::new(variant)
        .with_cache_dir(cache.into())
        .with_show_download_progress(false);

    let mut embedder = match fastembed::TextEmbedding::try_new(opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("prefetch failed for {name}: {e}");
            std::process::exit(1);
        }
    };

    match embedder.embed(vec!["prefetch"], None) {
        Ok(v) => println!(
            "prefetched {name} into {cache}: {} dims in {}ms",
            v[0].len(),
            started.elapsed().as_millis()
        ),
        Err(e) => {
            eprintln!("prefetch produced no vector for {name}: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{local_models, or_default, should_download, trimmed};

    #[test]
    fn prefetch_reads_the_variables_the_way_config_does() {
        // config::env: an empty value takes the default, and the value is not trimmed.
        assert_eq!(or_default(None, "d"), "d");
        assert_eq!(or_default(Some(String::new()), "d"), "d");
        assert_eq!(or_default(Some("m".into()), "d"), "m");
        // config::env_opt: trimmed, and blank reads as unset.
        assert_eq!(
            trimmed(Some("  bge-small-en-v1.5 ".into())).as_deref(),
            Some("bge-small-en-v1.5")
        );
        assert_eq!(trimmed(Some("   ".into())), None);
        assert_eq!(trimmed(None), None);
    }

    #[test]
    fn prefetch_runs_when_either_block_is_local() {
        assert!(should_download("local", None));
        assert!(should_download("local", Some("openai")));
        assert!(should_download("openai", Some("local")));
        assert!(!should_download("openai", None));
        assert!(!should_download("openai", Some("openai")));
        assert!(!should_download("hash", Some("hash")));
    }

    #[test]
    fn prefetch_fetches_the_model_each_local_block_names() {
        // The image used to bake bge-base whatever EMBED_MODEL said, and a server configured for
        // another local model then downloaded it at boot: the failure prefetch exists to prevent.
        assert_eq!(
            local_models("local", "multilingual-e5-base", None, None),
            vec!["multilingual-e5-base"]
        );
        assert_eq!(
            local_models("local", "multilingual-e5-base", Some("local"), Some("bge-small-en-v1.5")),
            vec!["multilingual-e5-base", "bge-small-en-v1.5"]
        );
        assert_eq!(
            local_models(
                "openai",
                "google/embeddinggemma-2",
                Some("local"),
                Some("bge-base-en-v1.5")
            ),
            vec!["bge-base-en-v1.5"]
        );
        assert_eq!(
            local_models("local", "bge-small-en-v1.5", Some("local"), Some("bge-small-en-v1.5")),
            vec!["bge-small-en-v1.5"]
        );
        assert!(local_models("openai", "m", Some("openai"), Some("n")).is_empty());
    }
}
