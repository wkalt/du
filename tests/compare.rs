//! Differential tests: our output (stdout, stderr, exit code) must match
//! the system's GNU du byte for byte. Skipped if GNU du isn't installed.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gnu_du_available() -> bool {
    Command::new("du")
        .arg("--version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("GNU coreutils"))
}

/// a/{b/,f,big}, c/{g (hard link to a/f),h/i/}, s -> a, locked/ (mode 000).
fn build_tree() -> PathBuf {
    let root = std::env::temp_dir().join(format!("du-compare-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir_all(root.join("c/h/i")).unwrap();
    fs::write(root.join("a/f"), "hi").unwrap();
    fs::write(root.join("a/big"), vec![1u8; 300_000]).unwrap();
    fs::hard_link(root.join("a/f"), root.join("c/g")).unwrap();
    symlink("a", root.join("s")).unwrap();
    fs::create_dir(root.join("locked")).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    root
}

fn run(bin: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(bin).args(args).current_dir(dir).env("LC_ALL", "C").output().unwrap()
}

#[test]
fn matches_gnu_du() {
    if !gnu_du_available() {
        eprintln!("skipping: GNU du not found");
        return;
    }
    let root = build_tree();
    type Case<'a> = (&'a str, &'a [&'a str]);
    let cases: &[Case] = &[
        ("default operand", &[]),
        ("plain", &["a", "c"]),
        ("human", &["-h", "."]),
        ("depth zero", &["-d0", "."]),
        ("depth one human", &["-hd", "1", "."]),
        ("long depth", &["--max-depth=2", "c"]),
        ("hard link order a first", &["a", "c"]),
        ("hard link order c first", &["c", "a"]),
        ("nested operands", &[".", "a"]),
        ("nested operands reversed", &["a", "."]),
        ("file operand inside dir operand", &["a/f", "a"]),
        ("repeated operand", &["a", "a/"]),
        ("trailing slashes", &["a/", "c//"]),
        ("many trailing slashes", &["c///"]),
        ("symlink operand", &["s"]),
        ("missing operand", &["nope", "a"]),
        ("bad option", &["-q"]),
        ("bad depth", &["-d", "x"]),
        ("summarize", &["-s", "a", "c"]),
        ("summarize kilobytes", &["-sk", "."]),
        ("summarize human", &["--summarize", "-h", "."]),
        ("summarize with depth zero", &["-s", "-d0", "a"]),
        ("summarize conflicts with depth", &["-d1", "-s", "a"]),
        ("k overrides h", &["-h", "-k", "a"]),
        ("h overrides k", &["-k", "-h", "a"]),
    ];
    for (name, args) in cases {
        let want = run("du", &root, args);
        let got = run(env!("CARGO_BIN_EXE_du"), &root, args);
        assert_eq!(String::from_utf8_lossy(&got.stdout), String::from_utf8_lossy(&want.stdout), "stdout: {name}");
        assert_eq!(String::from_utf8_lossy(&got.stderr), String::from_utf8_lossy(&want.stderr), "stderr: {name}");
        assert_eq!(got.status.code(), want.status.code(), "exit code: {name}");
    }
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir_all(&root).unwrap();
}
