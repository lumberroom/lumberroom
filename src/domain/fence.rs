//! The data fence `review_queue` draws, recognised on the way back in (decision 0031).

pub const CONTENT_CARRIES_FENCE: &str = "content_carries_fence";

/// True when any line is `----- data <12 hex> -----` or `----- end of data <12 hex> -----`.
pub fn carries_fence(content: &str) -> bool {
    let _ = content;
    todo!("task 3b")
}
