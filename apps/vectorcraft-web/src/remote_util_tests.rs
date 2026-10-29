//! Time limits of remote storage requests.

use std::time::Duration;

use crate::remote_util::{META_TIMEOUT, TRANSFER_TIMEOUT, timeout_for, timeout_message};

#[test]
fn limits_follow_the_method() {
    for m in ["PROPFIND", "HEAD", "MOVE", "DELETE"] {
        assert_eq!(timeout_for(m), META_TIMEOUT, "{m}");
    }
    for m in ["GET", "PUT"] {
        assert_eq!(timeout_for(m), TRANSFER_TIMEOUT, "{m}");
    }
    assert!(META_TIMEOUT < TRANSFER_TIMEOUT);
    let msg = timeout_message("PROPFIND", Duration::from_secs(20));
    assert!(msg.contains("PROPFIND") && msg.contains("20 s"), "{msg}");
}
