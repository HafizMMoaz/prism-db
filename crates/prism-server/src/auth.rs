//! User accounts, password authentication, and per-user privileges.
//!
//! Passwords are stored as scrypt PHC hashes ([`docs/components/network-server.md`],
//! "Authentication") — never in plaintext. Verification is constant-time within
//! scrypt and needs no RNG; only account creation salts and hashes.
//!
//! Each account carries a [`Privileges`] set (READ / WRITE / ADMIN). The session
//! enforces these per request; the embedded API runs as [`SYSTEM_OID`], a
//! superuser that bypasses checks.
//!
//! **Scope (this increment):** an in-memory user store seeded with a default
//! `admin`. Persisting users (and their privileges) as a catalog system table so
//! accounts survive restart is a follow-up — the same deferral the store already
//! had. mTLS is also deferred.

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

/// A user's privilege set: a bitmask of READ, WRITE, and ADMIN. READ permits
/// queries; WRITE permits data mutation (and DDL) and implies read; ADMIN
/// permits user/grant management and implies write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Privileges(u8);

impl Privileges {
    /// No privileges (a disabled account).
    pub const NONE: Privileges = Privileges(0);
    /// Read data.
    pub const READ: Privileges = Privileges(1);
    /// Mutate data and run DDL.
    pub const WRITE: Privileges = Privileges(2);
    /// Manage users and grants.
    pub const ADMIN: Privileges = Privileges(4);

    /// The `readonly` role: READ.
    pub fn read_only() -> Privileges {
        Self::READ
    }
    /// The `readwrite` role: READ + WRITE.
    pub fn read_write() -> Privileges {
        Privileges(Self::READ.0 | Self::WRITE.0)
    }
    /// The `admin` role: READ + WRITE + ADMIN.
    pub fn admin() -> Privileges {
        Privileges(Self::READ.0 | Self::WRITE.0 | Self::ADMIN.0)
    }

    /// Whether this set may read (any non-empty set may read).
    pub fn can_read(self) -> bool {
        self.0 != 0
    }
    /// Whether this set may mutate data or run DDL.
    pub fn can_write(self) -> bool {
        self.0 & (Self::WRITE.0 | Self::ADMIN.0) != 0
    }
    /// Whether this set may manage users and grants.
    pub fn can_admin(self) -> bool {
        self.0 & Self::ADMIN.0 != 0
    }

    /// The raw bitmask (for persistence / display).
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Parse a role name (case-insensitive) into a privilege set.
    pub fn from_role(name: &str) -> Option<Privileges> {
        match name.to_ascii_lowercase().as_str() {
            "readonly" | "read" => Some(Self::read_only()),
            "readwrite" | "write" => Some(Self::read_write()),
            "admin" | "superuser" => Some(Self::admin()),
            "none" => Some(Self::NONE),
            _ => None,
        }
    }
}

struct Account {
    oid: u64,
    /// scrypt PHC hash string.
    phc: String,
    privileges: Privileges,
}

/// An in-memory store of user accounts keyed by username.
pub struct UserStore {
    users: Mutex<HashMap<String, Account>>,
    next_oid: AtomicU64,
}

impl UserStore {
    /// A store seeded with a default `admin` account (dev default password
    /// `admin` — real deployments configure their own accounts), which holds the
    /// full privilege set.
    pub fn with_default_admin() -> Result<Self> {
        let store = Self {
            users: Mutex::new(HashMap::new()),
            next_oid: AtomicU64::new(FIRST_USER_OID),
        };
        store.add_user("admin", "admin", Privileges::admin())?;
        Ok(store)
    }

    /// Create (or replace) a user with the given password and privileges.
    /// Returns the OID.
    pub fn add_user(&self, username: &str, password: &str, privileges: Privileges) -> Result<u64> {
        let salt = SaltString::generate(&mut OsRng);
        let phc = Scrypt
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| ServerError::State(format!("password hashing failed: {e}")))?
            .to_string();
        let oid = self.next_oid.fetch_add(1, Ordering::Relaxed);
        self.users.lock().expect("user store poisoned").insert(
            username.to_string(),
            Account {
                oid,
                phc,
                privileges,
            },
        );
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

    /// The privileges of the account with `oid`, if any.
    pub fn privileges_of(&self, oid: u64) -> Option<Privileges> {
        self.users
            .lock()
            .expect("user store poisoned")
            .values()
            .find(|a| a.oid == oid)
            .map(|a| a.privileges)
    }

    /// Set `username`'s privileges (used by `GRANT`/`REVOKE`).
    pub fn set_privileges(&self, username: &str, privileges: Privileges) -> Result<()> {
        let mut users = self.users.lock().expect("user store poisoned");
        let account = users
            .get_mut(username)
            .ok_or_else(|| ServerError::State(format!("no such user: {username}")))?;
        account.privileges = privileges;
        Ok(())
    }

    /// Whether `username` exists.
    pub fn exists(&self, username: &str) -> bool {
        self.users
            .lock()
            .expect("user store poisoned")
            .contains_key(username)
    }

    /// Remove `username`. Refuses to drop the built-in `admin` (lockout guard).
    pub fn drop_user(&self, username: &str) -> Result<()> {
        if username == "admin" {
            return Err(ServerError::State("cannot drop the admin account".into()));
        }
        let mut users = self.users.lock().expect("user store poisoned");
        if users.remove(username).is_none() {
            return Err(ServerError::State(format!("no such user: {username}")));
        }
        Ok(())
    }
}
