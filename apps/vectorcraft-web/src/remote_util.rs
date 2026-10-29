//! Pure helpers of the browser's remote storage: request time limits.
//!
//! No browser types, so it also builds natively for its tests.

use std::time::Duration;

/// Time limit of a request that moves no file data (PROPFIND, HEAD, MOVE, DELETE).
pub const META_TIMEOUT: Duration = Duration::from_secs(20);
/// Time limit of a whole file transfer (GET, PUT), body included.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);
/// Time limit of the config file read at startup (a stuck server must not hold the app back).
pub const CONFIG_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a request with `method` may take.
pub fn timeout_for(method: &str) -> Duration {
    match method {
        "GET" | "PUT" => TRANSFER_TIMEOUT,
        _ => META_TIMEOUT,
    }
}

/// The error of a request that ran out of time.
pub fn timeout_message(method: &str, limit: Duration) -> String {
    format!("the server did not finish the {method} request within {} s", limit.as_secs())
}
