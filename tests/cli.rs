use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

fn rmm(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rmm")).arg("-rf").args(args).output().unwrap()
}

fn make_tree(root: &Path, depth: usize, width: usize, files: usize) {
    fs::create_dir_all(root).unwrap();
    for f in 0..files {
        fs::write(root.join(format!("f{f}.txt")), b"x").unwrap();
    }
    if depth > 0 {
        for d in 0..width {
            make_tree(&root.join(format!("d{d}")), depth - 1, width, files);
        }
    }
}

#[test]
fn removes_nested_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    make_tree(&root, 4, 4, 20);
    fs::create_dir_all(root.join("empty/deeper/deepest")).unwrap();
    fs::write(root.join("ünïcødé 🗑.txt"), b"x").unwrap();
    // A big directory gets split into several unlink batches.
    let big = root.join("big");
    fs::create_dir(&big).unwrap();
    for i in 0..3000 {
        fs::write(big.join(i.to_string()), b"").unwrap();
    }

    let out = rmm(&[&root]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!root.exists());
}

#[test]
fn does_not_follow_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    make_tree(&outside, 1, 2, 3);
    let root = tmp.path().join("tree");
    fs::create_dir(&root).unwrap();
    symlink(&outside, root.join("link")).unwrap();
    symlink(tmp.path().join("nope"), root.join("dangling")).unwrap();

    assert!(rmm(&[&root]).status.success());
    assert!(!root.exists());
    assert!(outside.join("d1/f2.txt").exists());

    // A symlink given directly is unlinked, not traversed.
    let link = tmp.path().join("top-link");
    symlink(&outside, &link).unwrap();
    assert!(rmm(&[&link]).status.success());
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(outside.exists());
}

#[test]
fn removes_single_file_and_ignores_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();
    let out = rmm(&[&file, &tmp.path().join("missing")]);
    assert!(out.status.success());
    assert!(!file.exists());
}

#[test]
fn reports_errors_and_removes_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    make_tree(&root, 1, 3, 5);
    let locked = root.join("d0");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let out = rmm(&[&root]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));
    assert!(locked.exists());
    assert!(!root.join("d1").exists());
    assert!(!root.join("f0.txt").exists());
}

#[test]
fn refuses_dot_and_root() {
    for p in [".", "..", "foo/.", "/"] {
        let out = rmm(&[Path::new(p)]);
        assert_eq!(out.status.code(), Some(2), "{p}");
    }
}

#[test]
fn prints_completions() {
    for shell in ["zsh", "bash", "fish"] {
        let out = Command::new(env!("CARGO_BIN_EXE_rmm")).args(["--completions", shell]).output().unwrap();
        assert!(out.status.success(), "{shell}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("rmm"), "{shell}");
    }
}
