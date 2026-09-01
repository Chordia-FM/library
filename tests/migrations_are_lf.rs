//! Every migration is LF, and this test is why that stays true.
//!
//! `sqlx` records a SHA-384 of each migration file when it applies it and re-hashes the file on
//! every boot to prove nothing changed. The hash is over raw bytes, so `\r\n` and `\n` are two
//! different migrations as far as it is concerned — and a file whose line endings changed is
//! reported as "previously applied but has been modified", which refuses to start the service.
//!
//! On 2026-09-01 that took the whole dev stack down, with no migration edited and no file younger
//! than a fortnight. `core.autocrlf=true` had rewritten some files to CRLF at checkout, at
//! different times, so the working tree disagreed with the index on 15 of 109 files. The failure is
//! latent as well: `sqlx::migrate!` embeds the files at compile time, so the drift is invisible
//! until something forces a rebuild, then surfaces as a service that will not boot.
//!
//! `.gitattributes` (`* text=auto eol=lf`) is what prevents it. This test is what notices if that
//! ever stops working — a migration committed from a machine without the attribute, or the
//! attribute removed. It costs a directory read, it fails loudly, and the alternative is finding
//! out from a production container that will not start.
//!
//! It runs everywhere rather than only on CI on purpose. On CI it can never fail, because a Linux
//! checkout is LF whatever the repository says; the machine it has to protect is the one where the
//! problem happens.

use std::fs;
use std::path::Path;

#[test]
fn no_migration_contains_a_carriage_return() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut offenders = Vec::new();
    let mut checked = 0usize;

    let entries = fs::read_dir(&dir).expect("migrations directory");
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        checked += 1;
        let bytes = fs::read(&path).expect("read migration");
        if bytes.contains(&b'\r') {
            offenders.push(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string(),
            );
        }
    }

    // A guard over an empty directory is a guard that cannot fail, and this one is here precisely
    // because a check that always passes is how the problem went unnoticed in the first place.
    assert!(
        checked > 0,
        "found no migrations to check in {}",
        dir.display()
    );

    assert!(
        offenders.is_empty(),
        "{} migration(s) contain CRLF line endings: {}.\n\
         sqlx checksums these files byte for byte, so this will be reported as a modified \
         migration and the service will refuse to boot against any database that applied them \
         as LF.\n\
         Fix: ensure .gitattributes carries `* text=auto eol=lf`, then refresh the working tree \
         with `git rm --cached -r . && git reset --hard`.",
        offenders.len(),
        offenders.join(", ")
    );
}
