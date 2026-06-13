//! A multi-database server instance: a central data directory holding many named
//! databases plus the server-global user store.
//!
//! This is the MySQL/Postgres-style layer over [`crate::Database`] (which is a
//! single database = one data directory). An `Instance` owns a *data root*; each
//! database is a subdirectory under it (`<data_root>/<name>/`). A reserved
//! `_system` database holds the server-global accounts, so a client authenticates
//! once against the instance and then selects a database to operate on.
//!
//! Databases are opened lazily and cached. Names are restricted to a safe
//! character set so a database name can never escape the data root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::auth::Privileges;
use crate::database::{Config, Database};
use crate::error::{Result, ServerError};

/// The reserved database name holding the server's user accounts.
pub const SYSTEM_DB: &str = "_system";

/// A running PrismDB instance: a data root, the global user store (in the
/// `_system` database), and the set of open data databases.
pub struct Instance {
    data_root: PathBuf,
    config: Config,
    /// Holds the server-global users; never exposed as a data database.
    system: Arc<Database>,
    /// Lazily-opened data databases, cached by name.
    databases: Mutex<HashMap<String, Arc<Database>>>,
}

impl Instance {
    /// Open (creating if absent) an instance rooted at `data_root`, durable.
    pub fn open(data_root: &Path) -> Result<Self> {
        Self::open_with(data_root, Config::durable())
    }

    /// Open an instance with an explicit [`Config`].
    pub fn open_with(data_root: &Path, config: Config) -> Result<Self> {
        std::fs::create_dir_all(data_root)?;
        // The system database owns the server-global users (admin is seeded and
        // persisted there on first open).
        let system_dir = data_root.join(SYSTEM_DB);
        std::fs::create_dir_all(&system_dir)?;
        let system = Arc::new(Database::open_with(&system_dir, config)?);
        Ok(Self {
            data_root: data_root.to_path_buf(),
            config,
            system,
            databases: Mutex::new(HashMap::new()),
        })
    }

    /// The data root directory.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    // ---- database management -------------------------------------------------

    fn db_path(&self, name: &str) -> PathBuf {
        self.data_root.join(name)
    }

    /// Validate a database name: non-empty, only `[A-Za-z0-9_-]`, not the
    /// reserved system name — so it can never traverse out of the data root.
    fn validate(name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(ServerError::State("database name is empty".into()));
        }
        if name.eq_ignore_ascii_case(SYSTEM_DB) {
            return Err(ServerError::State(format!("`{SYSTEM_DB}` is reserved")));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ServerError::State(format!(
                "invalid database name `{name}` (use letters, digits, `_`, `-`)"
            )));
        }
        Ok(())
    }

    fn exists_on_disk(&self, name: &str) -> bool {
        self.db_path(name).join("heap.db").exists()
    }

    /// All data database names, sorted (the `_system` database is hidden).
    pub fn list_databases(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.data_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != SYSTEM_DB && self.db_path(&name).join("heap.db").exists() {
                    names.push(name);
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create a new (empty) database. Errors if it already exists.
    pub fn create_database(&self, name: &str) -> Result<()> {
        Self::validate(name)?;
        if self.exists_on_disk(name) {
            return Err(ServerError::State(format!(
                "database `{name}` already exists"
            )));
        }
        let dir = self.db_path(name);
        std::fs::create_dir_all(&dir)?;
        let db = Arc::new(Database::open_data(&dir, self.config)?);
        self.databases
            .lock()
            .expect("databases poisoned")
            .insert(name.to_string(), db);
        Ok(())
    }

    /// Open (lazily, then cached) the named data database.
    pub fn database(&self, name: &str) -> Result<Arc<Database>> {
        Self::validate(name)?;
        let mut map = self.databases.lock().expect("databases poisoned");
        if let Some(db) = map.get(name) {
            return Ok(db.clone());
        }
        if !self.exists_on_disk(name) {
            return Err(ServerError::State(format!("no such database: `{name}`")));
        }
        let db = Arc::new(Database::open_data(&self.db_path(name), self.config)?);
        map.insert(name.to_string(), db.clone());
        Ok(db)
    }

    /// Drop a database and delete its files. The caller must ensure no session
    /// still holds it (on Windows, open files block directory removal).
    pub fn drop_database(&self, name: &str) -> Result<()> {
        Self::validate(name)?;
        if !self.exists_on_disk(name) {
            return Err(ServerError::State(format!("no such database: `{name}`")));
        }
        // Drop our cached handle so the files can be removed.
        self.databases
            .lock()
            .expect("databases poisoned")
            .remove(name);
        std::fs::remove_dir_all(self.db_path(name))?;
        Ok(())
    }

    /// Flush every open database (and the system database) to disk.
    pub fn checkpoint_all(&self) -> Result<()> {
        self.system.checkpoint()?;
        for db in self.databases.lock().expect("databases poisoned").values() {
            db.checkpoint()?;
        }
        Ok(())
    }

    // ---- server-global auth (delegated to the system database) ---------------

    /// Verify credentials against the server-global accounts.
    pub fn verify_user(&self, username: &str, password: &str) -> Option<u64> {
        self.system.verify_user(username, password)
    }

    /// Create a server-global user with explicit privileges.
    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        privileges: Privileges,
    ) -> Result<u64> {
        self.system.create_user(username, password, privileges)
    }

    /// Set a server-global user's privileges.
    pub fn set_user_privileges(&self, username: &str, privileges: Privileges) -> Result<()> {
        self.system.set_user_privileges(username, privileges)
    }

    /// Drop a server-global user.
    pub fn drop_user(&self, username: &str) -> Result<()> {
        self.system.drop_user(username)
    }

    /// The privileges of the account with `oid`.
    pub fn privileges(&self, oid: u64) -> Option<Privileges> {
        self.system.privileges(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prism_testkit::TempDir;

    fn instance() -> (Instance, TempDir) {
        let tmp = TempDir::new("instance").unwrap();
        let inst = Instance::open_with(tmp.path(), Config::default()).unwrap();
        (inst, tmp)
    }

    #[test]
    fn create_list_and_drop_databases() {
        let (inst, _tmp) = instance();
        assert!(inst.list_databases().unwrap().is_empty());

        inst.create_database("sales").unwrap();
        inst.create_database("hr").unwrap();
        assert_eq!(inst.list_databases().unwrap(), vec!["hr", "sales"]);

        // Duplicate create is rejected; the system name is reserved.
        assert!(inst.create_database("sales").is_err());
        assert!(inst.create_database("_system").is_err());
        assert!(inst.create_database("../escape").is_err());

        inst.drop_database("hr").unwrap();
        assert_eq!(inst.list_databases().unwrap(), vec!["sales"]);
        assert!(inst.database("hr").is_err(), "dropped db is gone");
    }

    #[test]
    fn databases_are_isolated_and_persist() {
        let tmp = TempDir::new("instance-persist").unwrap();
        {
            let inst = Instance::open_with(tmp.path(), Config::default()).unwrap();
            inst.create_database("a").unwrap();
            inst.create_database("b").unwrap();
            // Same table name in two databases — independent data.
            let a = inst.database("a").unwrap();
            let b = inst.database("b").unwrap();
            a.sql()
                .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY)")
                .unwrap();
            a.sql()
                .execute_autocommit("INSERT INTO t VALUES (1)")
                .unwrap();
            a.persist_sql_tables().unwrap();
            b.sql()
                .execute_autocommit("CREATE TABLE t (id BIGINT PRIMARY KEY)")
                .unwrap();
            b.persist_sql_tables().unwrap();
        }
        // Reopen: both databases survived, with their separate contents.
        let inst = Instance::open_with(tmp.path(), Config::default()).unwrap();
        assert_eq!(inst.list_databases().unwrap(), vec!["a", "b"]);
        let a = inst.database("a").unwrap();
        let b = inst.database("b").unwrap();
        use prism_sql::Outcome;
        let count = |db: &Database| match db.sql().execute_autocommit("SELECT id FROM t").unwrap() {
            Outcome::Select { rows, .. } => rows.len(),
            other => panic!("{other:?}"),
        };
        assert_eq!(count(&a), 1, "a has its row");
        assert_eq!(count(&b), 0, "b is independent and empty");
    }

    #[test]
    fn users_are_server_global_and_persist() {
        let tmp = TempDir::new("instance-users").unwrap();
        {
            let inst = Instance::open_with(tmp.path(), Config::default()).unwrap();
            // The seeded admin authenticates.
            assert!(inst.verify_user("admin", "admin").is_some());
            inst.create_user("alice", "pw", Privileges::read_write())
                .unwrap();
            inst.create_database("d").unwrap();
        }
        // Reopen: the user persisted at the instance level (independent of any
        // data database).
        let inst = Instance::open_with(tmp.path(), Config::default()).unwrap();
        assert!(inst.verify_user("alice", "pw").is_some());
        assert!(inst.verify_user("alice", "wrong").is_none());
    }
}
