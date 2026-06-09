//! User accounts and password authentication.
//!
//! Passwords are stored as scrypt PHC hashes ([`docs/components/network-server.md`],
//! "Authentication") — never in plaintext. Verification is constant-time within
//! scrypt and needs no RNG; only account creation salts and hashes.
//!
//! **Scope (this increment):** an in-memory user store seeded with a default
//! `admin` account. Persisting users as a catalog system table (so accounts
//! survive restart) is a follow-up, the same deferral as the rest of the
//! catalog. mTLS is also deferred.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use scrypt::Scrypt;
use scrypt::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

use crate::error::{Result, ServerError};

/// The OID handed to the trusted embedded API (no network authentication).
pub const SYSTEM_OID: u64 = 1;
/// The first OID allocated to a created user account.
const FIRST_USER_OID: u64 = 100;

struct Account {
    oid: u64,
    /// scrypt PHC hash string.
    phc: String,
}

/// An in-memory store of user accounts keyed by username.
pub struct UserStore {
    users: Mutex<HashMap<String, Account>>,
    next_oid: AtomicU64,
}

impl UserStore {
    /// A store seeded with a default `admin` account (dev default password
    /// `admin` — real deployments configure their own accounts).
    pub fn with_default_admin() -> Result<Self> {
        let store = Self {
            users: Mutex::new(HashMap::new()),
            next_oid: AtomicU64::new(FIRST_USER_OID),
        };
        store.add_user("admin", "admin")?;
        Ok(store)
    }

    /// Create (or replace) a user with the given password. Returns the OID.
    pub fn add_user(&self, username: &str, password: &str) -> Result<u64> {
        let salt = SaltString::generate(&mut OsRng);
        let phc = Scrypt
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| ServerError::State(format!("password hashing failed: {e}")))?
            .to_string();
        let oid = self.next_oid.fetch_add(1, Ordering::Relaxed);
        self.users
            .lock()
            .expect("user store poisoned")
            .insert(username.to_string(), Account { oid, phc });
        Ok(oid)
    }

    /// Verify `password` for `username`. Returns the user's OID on success, or
    /// `None` on a bad password or unknown user (the caller must not reveal
    /// which).
    pub fn verify(&self, username: &str, password: &str) -> Option<u64> {
        let users = self.users.lock().expect("user store poisoned");
        let account = users.get(username)?;
        let parsed = PasswordHash::new(&account.phc).ok()?;
        Scrypt
            .verify_password(password.as_bytes(), &parsed)
            .ok()
            .map(|_| account.oid)
    }
}
