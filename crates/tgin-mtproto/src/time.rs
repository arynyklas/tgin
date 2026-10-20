//! The wall clock, as Unix time.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds and nanoseconds since the Unix epoch: zero before it, the
/// seconds capped at `i64::MAX`.
pub(crate) fn now_unix() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
        now.subsec_nanos(),
    )
}
