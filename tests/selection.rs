//! Selection semantics, run through the built binary against small real
//! git repositories created per test.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const RULES: &str = r#"
[[path-rule]]
globs = ["rust-toolchain.toml", "deny.toml"]
mark-changed = "all"

[[path-rule]]
globs = ["docs/**", "*.md"]
mark-changed = []
"#;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git starts");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Records the index as a commit on the current branch with plumbing, so a
/// fixture repository never goes through the porcelain commit path (and
/// needs no signing key).
fn record(root: &Path, message: &str) {
    git(root, &["add", "."]);
    let tree = git(root, &["write-tree"]);
    let head = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .unwrap();
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
    let parent = String::from_utf8(head.stdout).unwrap().trim().to_string();
    if head.status.success() {
        args.extend(["-p", parent.as_str()]);
    }
    let commit = git(root, &args);
    git(root, &["update-ref", "HEAD", &commit]);
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn package(root: &Path, dir: &str, name: &str, dependency: Option<&str>) {
    let dependencies = dependency
        .map(|dep| format!("{dep} = {{ path = \"../{dep}\" }}\n"))
        .unwrap_or_default();
    write(
        root,
        &format!("{dir}/Cargo.toml"),
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}"
        ),
    );
    write(root, &format!("{dir}/src/lib.rs"), "pub fn f() {}\n");
}

/// A virtual workspace: `leaf` <- `mid` <- `top`, and an unrelated `solo`.
fn virtual_workspace() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"leaf\", \"mid\", \"top\", \"solo\"]\n",
    );
    package(root, "leaf", "leaf", None);
    package(root, "mid", "mid", Some("leaf"));
    package(root, "top", "top", Some("mid"));
    package(root, "solo", "solo", None);
    write(root, "rules.toml", RULES);
    write(root, "docs/guide.md", "guide\n");
    write(root, "README.md", "readme\n");
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    record(root, "base");
    git(root, &["checkout", "--quiet", "-b", "change"]);
    dir
}

fn commit(root: &Path) {
    record(root, "change");
}

fn run(root: &Path, base: Option<&str>) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_affected-crates"));
    command
        .arg("--root")
        .arg(root)
        .arg("--rules")
        .arg(root.join("rules.toml"));
    if let Some(base) = base {
        command.args(["--base", base]);
    }
    let output = command.output().expect("binary starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is JSON")
}

fn affected(report: &Value) -> Vec<&str> {
    report["affected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect()
}

#[test]
fn changed_crate_selects_itself_and_reverse_dependencies() {
    let dir = virtual_workspace();
    write(dir.path(), "mid/src/lib.rs", "pub fn f() { let _ = 1; }\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], false);
    assert_eq!(affected(&report), ["mid", "top"]);
    assert_eq!(report["packages"], "mid top");
    assert_eq!(report["package_args"], "-p mid -p top");
}

#[test]
fn leaf_change_reaches_the_whole_chain_but_not_unrelated_crates() {
    let dir = virtual_workspace();
    write(dir.path(), "leaf/src/lib.rs", "pub fn f() { let _ = 1; }\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(affected(&report), ["leaf", "mid", "top"]);
}

#[test]
fn prose_only_selects_nothing() {
    let dir = virtual_workspace();
    write(dir.path(), "docs/guide.md", "changed\n");
    write(dir.path(), "README.md", "changed\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], false);
    assert!(affected(&report).is_empty());
    assert_eq!(report["packages"], "");
    assert_eq!(report["package_args"], "");
}

#[test]
fn unmatched_outside_path_selects_the_workspace() {
    let dir = virtual_workspace();
    write(dir.path(), "scripts/build.sh", "echo\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], true);
    assert_eq!(report["packages"], "workspace");
    assert_eq!(report["package_args"], "");
}

#[test]
fn mark_changed_all_rule_selects_the_workspace() {
    let dir = virtual_workspace();
    write(dir.path(), "deny.toml", "x = 1\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], true);
}

#[test]
fn no_base_selects_the_workspace() {
    let dir = virtual_workspace();
    let report = run(dir.path(), None);
    assert_eq!(report["workspace"], true);
    assert_eq!(report["packages"], "workspace");
    assert_eq!(report["members"].as_array().unwrap().len(), 4);
}

#[test]
fn empty_diff_selects_nothing() {
    let dir = virtual_workspace();
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], false);
    assert!(affected(&report).is_empty());
}

#[test]
fn markdown_inside_a_package_belongs_to_that_package() {
    let dir = virtual_workspace();
    write(dir.path(), "mid/NOTES.md", "included by the crate\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], false);
    assert_eq!(affected(&report), ["mid"]);
    assert_eq!(report["owners"]["mid/NOTES.md"], "mid");
}

/// A workspace whose root manifest is also a package, with member `leaf`.
fn rooted_workspace() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"rooted\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nleaf = { path = \"leaf\" }\n\n[workspace]\nmembers = [\"leaf\"]\n",
    );
    write(root, "src/lib.rs", "pub fn f() {}\n");
    write(root, "tests/it.rs", "#[test]\nfn t() {}\n");
    package(root, "leaf", "leaf", None);
    write(root, "rules.toml", RULES);
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    record(root, "base");
    git(root, &["checkout", "--quiet", "-b", "change"]);
    dir
}

#[test]
fn root_package_owns_only_its_declared_sources() {
    let dir = rooted_workspace();
    write(
        dir.path(),
        "tests/it.rs",
        "#[test]\nfn t() { let _ = 1; }\n",
    );
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], false);
    assert_eq!(affected(&report), ["rooted"]);
}

#[test]
fn stray_top_level_file_in_a_rooted_workspace_selects_the_workspace() {
    let dir = rooted_workspace();
    write(dir.path(), "Clippy.toml", "x = 1\n");
    commit(dir.path());
    let report = run(dir.path(), Some("main"));
    assert_eq!(report["workspace"], true);
}
