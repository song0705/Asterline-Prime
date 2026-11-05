//! The public binaries must report their build version without startup side effects.

use std::process::{Command, Stdio};

#[test]
fn version_flags_work_for_both_binaries_without_opening_a_workspace() {
    let root = std::env::temp_dir().join(format!("asterline-version-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    for binary in [env!("CARGO_BIN_EXE_ast"), env!("CARGO_BIN_EXE_asterline")] {
        for flag in ["-v", "--version"] {
            let output = Command::new(binary)
                .arg(flag)
                .arg("--workspace")
                .arg(root.join("missing-workspace"))
                .arg("--team")
                .arg(root.join("missing-team.json"))
                .arg("--db")
                .arg(root.join("missing-db.sqlite3"))
                .current_dir(&root)
                .env("ASTERLINE_HOME", root.join("state"))
                .env("PATH", "")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success(), "{binary} {flag}: {:?}", output);
            assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                format!("asterline {}", env!("CARGO_PKG_VERSION"))
            );
            assert!(output.stderr.is_empty());
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                0,
                "version lookup must not create application files"
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
