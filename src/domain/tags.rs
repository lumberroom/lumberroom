//! Tags as the store keeps them.
//!
//! One spelling serves the write and the filter. A filter that lowercased against a store that did
//! not, or the reverse, matches nothing, and the caller reads that as "no fact carries this tag"
//! about a tag the store holds.

/// Trimmed, ASCII-lowercased, blanks dropped, each tag once in the order first seen.
///
/// The write path runs every tag through this, so a filter run through it matches the stored form.
/// A restore copies an archive's tags as they stand and skips this function, so a row restored from
/// an archive written before the write path cleaned its tags can hold a spelling no filter reaches.
pub fn normalise<I, S>(tags: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.as_ref().trim().to_ascii_lowercase();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_trimmed_lowercased_and_kept_once_in_first_seen_order() {
        let tags = normalise([" Preference ", "preference", "", "Tooling"]);
        assert_eq!(tags, vec!["preference", "tooling"]);
    }

    /// A filter of blanks is a filter of nothing. The read then answers with everything the grant
    /// admits, which is the unfiltered list and never a row past the grant.
    #[test]
    fn a_filter_of_blanks_normalises_to_no_filter() {
        assert!(normalise(["  ", ""]).is_empty());
    }

    /// ASCII folding, because that is what every stored row went through. Folding Unicode case
    /// here alone would make `ÉTÉ` in a filter miss the `Été` a write kept.
    #[test]
    fn case_folding_is_ascii_only_to_match_the_write() {
        assert_eq!(normalise(["ÉTÉ Plan"]), vec!["ÉtÉ plan"]);
    }
}
