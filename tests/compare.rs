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

/// a/{b/,f,big}, c/{g (hard link to a/f),h/i/}, s -> a, locked/ (mode 000),
/// and many/: enough entries to be scanned in chunks, holding hard links
/// (to each other and to a/big), a subdirectory, and an unreadable one.
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
    fs::create_dir_all(root.join("many/sub")).unwrap();
    for i in 0..1200 {
        fs::write(root.join(format!("many/f{i}")), "x").unwrap();
    }
    for i in 0..20 {
        fs::hard_link(root.join(format!("many/f{i}")), root.join(format!("many/l{i}"))).unwrap();
    }
    fs::hard_link(root.join("a/big"), root.join("many/sub/big")).unwrap();
    fs::create_dir(root.join("many/locked")).unwrap();
    fs::set_permissions(root.join("many/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    root
}

/// wide/ with 100,500 empty subdirectories: more than the scan reads in
/// one batch, and more than the threshold at which GNU du sorts a
/// directory by inode. Each one is a line of output, so a dropped entry
/// shows. Costs a few seconds and ~400 MB of temporary space.
fn build_wide_tree() -> PathBuf {
    let root = std::env::temp_dir().join(format!("du-compare-wide-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("wide")).unwrap();
    std::thread::scope(|s| {
        for t in 0..8 {
            let wide = root.join("wide");
            s.spawn(move || {
                for i in (t..100_500).step_by(8) {
                    fs::create_dir(wide.join(format!("d{i}"))).unwrap();
                }
            });
        }
    });
    root
}

fn run(bin: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(bin).args(args).current_dir(dir).env("LC_ALL", "C").output().unwrap()
}

/// Runs both implementations in `dir` and compares everything they produce.
fn assert_same(name: &str, dir: &Path, args: &[&str]) {
    let want = run("du", dir, args);
    let got = run(env!("CARGO_BIN_EXE_du"), dir, args);
    assert_eq!(String::from_utf8_lossy(&got.stdout), String::from_utf8_lossy(&want.stdout), "stdout: {name}");
    assert_eq!(String::from_utf8_lossy(&got.stderr), String::from_utf8_lossy(&want.stderr), "stderr: {name}");
    assert_eq!(got.status.code(), want.status.code(), "exit code: {name}");
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
        ("chunked", &["many"]),
        ("chunked summarized", &["-s", "many"]),
        ("chunked below depth", &["-d1", "."]),
        ("link into chunked dir, dir first", &["many", "a"]),
        ("link into chunked dir, file first", &["a", "many"]),
    ];
    for (name, args) in cases {
        assert_same(name, &root, args);
    }
    for locked in ["locked", "many/locked"] {
        fs::set_permissions(root.join(locked), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::remove_dir_all(&root).unwrap();
}

/// /dev carries mount points (pts, shm, ...) on any Linux system, so it
/// exercises -x without privileges to mount anything.
#[test]
fn matches_gnu_du_one_file_system() {
    if !gnu_du_available() {
        eprintln!("skipping: GNU du not found");
        return;
    }
    let root = Path::new("/");
    let with = run("du", root, &["-x", "/dev"]);
    let without = run("du", root, &["/dev"]);
    assert_ne!(with.stdout, without.stdout, "no mount point under /dev, so -x is not exercised");
    let cases: &[(&str, &[&str])] = &[
        ("skips mount points", &["-x", "/dev"]),
        ("long option", &["--one-file-system", "-s", "/dev"]),
        ("mount point as operand is kept", &["-x", "/dev", "/dev/shm"]),
        ("clustered", &["-xhd1", "/dev"]),
    ];
    for (name, args) in cases {
        assert_same(name, root, args);
    }
}

#[test]
fn matches_gnu_du_across_batches() {
    if !gnu_du_available() {
        eprintln!("skipping: GNU du not found");
        return;
    }
    let root = build_wide_tree();
    let want = run("du", &root, &["wide"]);
    let got = run(env!("CARGO_BIN_EXE_du"), &root, &["wide"]);
    fs::remove_dir_all(&root).unwrap();
    assert_eq!(got.stdout.iter().filter(|&&b| b == b'\n').count(), 100_501, "lines of output");
    assert_eq!(got.stdout, want.stdout, "stdout differs from GNU du");
    assert_eq!(got.status.code(), want.status.code(), "exit code");
}
