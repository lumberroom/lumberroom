//! The data fence `review_queue` draws, recognised on the way back in (decision 0031).
//!
//! An agent working the queue reads fenced values in the structured copy and may paste one into a
//! merge with its markers. A row stored that way prints a marker inside every later queue and
//! digest, where it reads as the edge of a block the server drew. The write path refuses it here.

pub const CONTENT_CARRIES_FENCE: &str = "content_carries_fence";

/// True when any line is `----- data <12 hex> -----` or `----- end of data <12 hex> -----`.
///
/// Lowercase hex only, because `review_queue::fence_nonce` draws lowercase. A sentence that
/// mentions a marker mid-line passes: only a whole line opens or closes a fence.
pub fn carries_fence(content: &str) -> bool {
    content.lines().any(|line| {
        let rest =
            line.strip_prefix("----- end of data ").or_else(|| line.strip_prefix("----- data "));
        rest.and_then(|r| r.strip_suffix(" -----")).is_some_and(|n| {
            n.len() == 12 && n.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_line_is_recognised_and_a_sentence_about_fences_is_not() {
        assert!(carries_fence("x\n----- data 3f9a1c07b2e4 -----\ny"));
        assert!(carries_fence("----- end of data 3f9a1c07b2e4 -----"));
        assert!(!carries_fence("the review queue draws ----- data blocks around rows"));
        assert!(!carries_fence("----- data 3F9A1C07B2E4 -----"));
        assert!(!carries_fence("----- data 3f9a -----"));
    }

    /// A value pasted from a Windows client keeps its `\r\n`, and `lines` strips both.
    #[test]
    fn a_fence_line_ending_in_a_carriage_return_is_still_a_fence_line() {
        assert!(carries_fence("x\r\n----- data 3f9a1c07b2e4 -----\r\ny"));
    }

    #[test]
    fn a_marker_with_text_around_it_on_the_same_line_is_prose() {
        assert!(!carries_fence("see ----- data 3f9a1c07b2e4 -----"));
        assert!(!carries_fence("----- data 3f9a1c07b2e4 ----- and more"));
        assert!(!carries_fence("----- data 3f9a1c07b2e4g -----"));
    }
}
