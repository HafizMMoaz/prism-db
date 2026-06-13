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
//! The store is the in-memory cache; the [`crate::Database`] persists each
//! account (and privilege change) to a reserved system heap, so accounts and
//! grants survive restart. mTLS is deferred.

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

    /// Reconstruct a privilege set from its raw bitmask (e.g. on load).
    pub fn from_bits(bits: u8) -> Privileges {
        Privileges(bits & (Self::READ.0 | Self::WRITE.0 | Self::ADMIN.0))
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

    /// The canonical role name for this set (for display, e.g. `SHOW GRANTS`).
    pub fn role_name(self) -> &'static str {
        if self.can_admin() {
            "admin"
        } else if self.can_write() {
            "readwrite"
        } else if self.can_read() {
            "readonly"
        } else {
            "none"
        }
    }
}

struct Account {
    oid: u64,
    /// scrypt PHC hash string.
    phc: String,
    /// The global privilege set: the default for any database without a
    /// per-database override below.
    privileges: Privileges,
    /// Per-database overrides keyed by database name. An entry shadows
    /// `privileges` for that database (it may grant more or — as `NONE` — deny).
    db_grants: HashMap<String, Privileges>,
}

/// An in-memory store of user accounts keyed by username.
pub struct UserStore {
    users: Mutex<HashMap<String, Account>>,
    next_oid: AtomicU64,
}

impl UserStore {
    /// An empty store (no accounts). The caller seeds or loads accounts.
    pub fn empty() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            next_oid: AtomicU64::new(FIRST_USER_OID),
        }
    }

    /// Create (or replace) a user with the given password and privileges.
    /// Returns the allocated OID and the scrypt PHC hash (so the caller can
    /// persist the account).
    pub fn add_user(
        &self,
        username: &str,
        password: &str,
        privileges: Privileges,
    ) -> Result<(u64, String)> {
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
                phc: phc.clone(),
                privileges,
                db_grants: HashMap::new(),
            },
        );
        Ok((oid, phc))
    }

    /// Insert an account loaded from persistence (preserving its OID, hash, and
    /// per-database grants), advancing the OID allocator past it.
    pub fn insert_loaded(
        &self,
        username: &str,
        oid: u64,
        phc: String,
        privileges: Privileges,
        db_grants: HashMap<String, Privileges>,
    ) {
        self.users.lock().expect("user store poisoned").insert(
            username.to_string(),
            Account {
                oid,
                phc,
                privileges,
                db_grants,
            },
        );
        // Ensure newly-allocated OIDs never collide with a loaded one.
        let mut cur = self.next_oid.load(Ordering::Relaxed);
        while oid >= cur {
            match self.next_oid.compare_exchange_weak(
                cur,
                oid + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// The full persistable snapshot of `username`: `(oid, phc, global
    /// privileges, per-database grants)`. Writing the whole snapshot keeps the
    /// append-only user heap correct (the last record per user wins).
    pub fn account_snapshot(
        &self,
        username: &str,
    ) -> Option<(u64, String, Privileges, HashMap<String, Privileges>)> {
        self.users
            .lock()
            .expect("user store poisoned")
            .get(username)
            .map(|a| (a.oid, a.phc.clone(), a.privileges, a.db_grants.clone()))
    }

    /// The per-database grants of `username` (for `SHOW GRANTS`).
    pub fn db_grants_of(&self, username: &str) -> Option<HashMap<String, Privileges>> {
        self.users
            .lock()
            .expect("user store poisoned")
            .get(username)
            .map(|a| a.db_grants.clone())
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

    /// The global privileges of the account with `oid`, if any.
    pub fn privileges_of(&self, oid: u64) -> Option<Privileges> {
        self.users
            .lock()
            .expect("user store poisoned")
            .values()
            .find(|a| a.oid == oid)
            .map(|a| a.privileges)
    }

    /// The effective privileges of the account with `oid` for database `db`: a
    /// per-database override if one exists, else the account's global set.
    /// `db = None` (or a database without an override) yields the global set.
    pub fn effective_privileges(&self, oid: u64, db: Option<&str>) -> Option<Privileges> {
        self.users
            .lock()
            .expect("user store poisoned")
            .values()
            .find(|a| a.oid == oid)
            .map(|a| {
                db.and_then(|name| a.db_grants.get(name).copied())
                    .unwrap_or(a.privileges)
            })
    }

    /// Set `username`'s global privileges (used by `GRANT`/`REVOKE` without a
    /// database scope).
    pub fn set_privileges(&self, username: &str, privileges: Privileges) -> Result<()> {
        let mut users = self.users.lock().expect("user store poisoned");
        let account = users
            .get_mut(username)
            .ok_or_else(|| ServerError::State(format!("no such user: {username}")))?;
        account.privileges = privileges;
        Ok(())
    }

    /// Set `username`'s privileges for a single database (the effect of
    /// `GRANT … ON <db>` / `REVOKE ALL ON <db>`). `NONE` denies that database
    /// even when the global set would allow it.
    pub fn set_db_privileges(
        &self,
        username: &str,
        db: &str,
        privileges: Privileges,
    ) -> Result<()> {
        let mut users = self.users.lock().expect("user store poisoned");
        let account = users
            .get_mut(username)
            .ok_or_else(|| ServerError::State(format!("no such user: {username}")))?;
        account.db_grants.insert(db.to_string(), privileges);
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
