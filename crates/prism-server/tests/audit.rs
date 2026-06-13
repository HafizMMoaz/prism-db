//! Audit logging emits structured events for security-relevant actions and
//! never records passwords.

use std::io;
use std::sync::{Arc, Mutex};

use prism_protocol::{AuthMechanism, Message, PROTOCOL_VERSION};
use prism_server::{Database, Session};
use prism_testkit::TempDir;

/// A `tracing` writer that captures output into a shared buffer.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn hello() -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "t".into(),
        client_version: "0".into(),
        features: 0,
    }
}

fn auth(user: &str, pw: &str) -> Message {
    Message::Auth {
        mechanism: AuthMechanism::Password,
        username: user.into(),
        password: pw.into(),
    }
}

fn sql(s: &str) -> Message {
    Message::SqlExecute {
        sql: s.into(),
        params: vec![],
        options: 1,
    }
}

/// Run `body` with a capturing subscriber installed; return everything logged.
fn capture(body: impl FnOnce()) -> String {
    let buf = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn audit_records_auth_denial_and_admin_without_passwords() {
    let tmp = TempDir::new("audit").unwrap();
    let db = Arc::new(Database::open(tmp.path()).unwrap());

    let logged = capture(|| {
        // Admin creates a read-only user with a secret password.
        let mut admin = Session::new_authenticating(db.clone());
        admin.handle(hello());
        admin.handle(auth("admin", "admin"));
        admin.handle(sql("CREATE TABLE t (id BIGINT PRIMARY KEY)"));
        admin.handle(sql(
            "CREATE USER reader WITH PASSWORD 'sup3rs3cret' ROLE readonly",
        ));

        // A failed login.
        let mut bad = Session::new_authenticating(db.clone());
        bad.handle(hello());
        bad.handle(auth("reader", "wrongpassword"));

        // The reader is denied a write.
        let mut reader = Session::new_authenticating(db.clone());
        reader.handle(hello());
        reader.handle(auth("reader", "sup3rs3cret"));
        reader.handle(sql("INSERT INTO t VALUES (1)"));
    });

    // Authentication outcomes are audited.
    assert!(
        logged.contains("authenticated"),
        "auth success logged:\n{logged}"
    );
    assert!(
        logged.contains("authentication failed"),
        "auth failure logged:\n{logged}"
    );
    // The admin action is audited (action + target user).
    assert!(logged.contains("admin action"), "admin action logged");
    assert!(logged.contains("CREATE USER"));
    // The authorization denial is audited.
    assert!(
        logged.contains("permission denied"),
        "denial logged:\n{logged}"
    );

    // Crucially: no password ever appears in the audit log.
    assert!(
        !logged.contains("sup3rs3cret"),
        "password leaked into the log:\n{logged}"
    );
    assert!(
        !logged.contains("wrongpassword"),
        "attempted password leaked into the log:\n{logged}"
    );
}
