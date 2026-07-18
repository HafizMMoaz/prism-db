//! Security audit logging.
//!
//! Emits structured [`tracing`] events on the `audit` target for security-
//! relevant actions: authentication outcomes, authorization denials, and
//! user/grant management. **Passwords are never recorded** - only usernames,
//! OIDs, peer addresses, and the action taken. Initialize a subscriber (see the
//! `prismd` binary) to route these somewhere; with no subscriber they are free.

use tracing::{info, warn};

/// The tracing target for audit events (filter with e.g. `RUST_LOG=audit=info`).
pub const TARGET: &str = "audit";

/// A successful authentication.
pub fn auth_success(username: &str, oid: u64) {
    info!(target: TARGET, event = "auth_success", user = username, oid, "authenticated");
}

/// A failed authentication (bad credentials or unknown user - not distinguished
/// in the log, so it cannot reveal which usernames exist via the message).
pub fn auth_failure(username: &str) {
    warn!(target: TARGET, event = "auth_failure", user = username, "authentication failed");
}

/// A request denied for lack of privilege.
pub fn denied(oid: u64, need: &str) {
    warn!(target: TARGET, event = "denied", oid, need, "permission denied");
}

/// An administrative action (user/grant management) succeeded.
pub fn admin(oid: u64, action: &str, target_user: &str) {
    info!(target: TARGET, event = "admin", oid, action, target = target_user, "admin action");
}

/// A connection was accepted.
pub fn connection_opened(peer: &str) {
    info!(target: TARGET, event = "conn_open", peer, "connection opened");
}

/// A connection closed.
pub fn connection_closed(peer: &str) {
    info!(target: TARGET, event = "conn_close", peer, "connection closed");
}
