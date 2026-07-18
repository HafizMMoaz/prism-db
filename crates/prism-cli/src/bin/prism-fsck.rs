//! `prism-fsck` - offline integrity checker.
//!
//! `prism-fsck <dir>` validates the database header, every allocated page's
//! checksum, and the WAL's record CRCs, printing a report and exiting non-zero
//! if corruption is found. Reads on-disk formats only - never live state. See
//! `docs/architecture/module-layout.md`.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: prism-fsck <database-dir>");
        return ExitCode::FAILURE;
    };
    let report = prism_fsck::check(Path::new(&dir));
    println!("{report}");
    if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
