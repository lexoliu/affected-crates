//! Selects the workspace packages a diff affects.
//!
//! Run from a repository checkout standing at the head revision:
//!
//! ```text
//! affected-crates --base origin/main --rules .github/affected-rules.toml
//! ```
//!
//! The diff runs from the merge-base of `--base` and `HEAD`. Both revisions'
//! package graphs go to the `determinator` crate together with the caller's
//! rules file; the tool prints one JSON [`Report`] on stdout.
//!
//! `affected` is what the determinator marks (the packages owning a changed
//! file plus every package whose build could change because a dependency
//! did). `workspace` is true when a changed path matched a
//! `mark-changed = "all"` rule or matched no package at all, where listing
//! packages would pretend precision the diff does not have. A prose-only
//! diff, whose paths rules map to nothing, selects nothing. Without `--base`
//! there is no diff and the whole workspace is in scope.
//!
//! When the workspace has a root package (manifest at the repository root),
//! its package directory is the repository root, so every unmatched path
//! would resolve to it. A path that ancestor-matches only the root package
//! counts as package-owned when it is one of its declared target files or
//! sits directly inside a directory holding one; anything else is reported
//! as unmatched, which selects the whole workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use determinator::{
    Determinator,
    rules::{DeterminatorRules, PathMatch},
};
use guppy::{
    CargoMetadata, PackageId,
    graph::{DependencyDirection, PackageGraph},
};
use serde::Serialize;
use tracing::{debug, info};

/// Reports which workspace packages a diff affects.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Repository root: the checkout standing at the head revision.
    #[arg(long, default_value = ".")]
    root: Utf8PathBuf,
    /// Revision the diff is measured from, through its merge-base with
    /// HEAD. Absent means no diff: the whole workspace is in scope.
    #[arg(long)]
    base: Option<String>,
    /// The caller's determinator rules file (TOML).
    #[arg(long)]
    rules: Utf8PathBuf,
    /// Resolve the head revision's package graph with `cargo metadata
    /// --locked`.
    #[arg(long)]
    locked: bool,
}

/// The tool's result, printed as JSON.
#[derive(Serialize)]
struct Report {
    /// The merge-base the diff ran from; absent without `--base`.
    merge_base: Option<String>,
    /// Every package may be affected.
    workspace: bool,
    /// Affected package names, sorted.
    affected: Vec<String>,
    /// `workspace`, the space-separated affected names, or empty.
    packages: String,
    /// `packages` as cargo `-p` flags; empty when `workspace` is true.
    package_args: String,
    /// Every workspace member's name, sorted.
    members: Vec<String>,
    /// Each changed path with the package owning it by directory, `null`
    /// when a rule consumed it or no package owns it.
    owners: BTreeMap<String, Option<String>>,
}

fn git(root: &Utf8Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("git {} failed to start", args[0]))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn cargo_metadata(manifest_path: &Utf8Path, locked: bool) -> Result<PackageGraph> {
    let mut command = Command::new("cargo");
    command
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest_path);
    if locked {
        command.arg("--locked");
    }
    let output = command.output().context("cargo metadata failed to start")?;
    if !output.status.success() {
        bail!(
            "cargo metadata on {manifest_path} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let json = String::from_utf8(output.stdout).context("cargo metadata output is not UTF-8")?;
    CargoMetadata::parse_json(&json)
        .and_then(CargoMetadata::build_graph)
        .with_context(|| format!("package graph of {manifest_path} did not resolve"))
}

/// The base revision checked out as a linked worktree: `cargo metadata`
/// needs the whole tree (path dependencies), not the manifest alone.
struct BaseWorktree {
    repo: Utf8PathBuf,
    _dir: tempfile::TempDir,
    path: Utf8PathBuf,
}

impl BaseWorktree {
    fn add(repo: &Utf8Path, revision: &str) -> Result<Self> {
        let dir = tempfile::tempdir().context("creating the base worktree directory")?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("base"))
            .map_err(|path| anyhow::anyhow!("temp dir {path:?} is not UTF-8"))?;
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                path.as_str(),
                revision,
            ],
        )?;
        Ok(Self {
            repo: repo.to_owned(),
            _dir: dir,
            path,
        })
    }
}

impl Drop for BaseWorktree {
    fn drop(&mut self) {
        let removed = git(
            &self.repo,
            &["worktree", "remove", "--force", self.path.as_str()],
        );
        if removed.is_err() {
            // The directory is deleted with `dir`; drop the stale entry.
            let _ = git(&self.repo, &["worktree", "prune"]);
        }
    }
}

/// What the root package itself can own: its declared target files plus the
/// directories that directly hold them, matching the depth cargo
/// autodiscovery scans.
struct RootOwnership {
    package_id: PackageId,
    files: BTreeSet<Utf8PathBuf>,
    dirs: BTreeSet<Utf8PathBuf>,
}

impl RootOwnership {
    /// `None` for a virtual workspace, which has no root package.
    fn new(graph: &PackageGraph) -> Option<Self> {
        let workspace_root = graph.workspace().root();
        let members = graph.query_workspace().resolve();
        let root_package = members
            .packages(DependencyDirection::Forward)
            .find(|package| package.manifest_path().parent() == Some(workspace_root))?;
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        for target in root_package.build_targets() {
            let relative = target
                .path()
                .strip_prefix(workspace_root)
                .expect("root package target outside the workspace")
                .to_path_buf();
            if let Some(parent) = relative.parent()
                && !parent.as_str().is_empty()
            {
                dirs.insert(parent.to_path_buf());
            }
            files.insert(relative);
        }
        Some(Self {
            package_id: root_package.id().clone(),
            files,
            dirs,
        })
    }

    fn owns(&self, path: &Utf8Path) -> bool {
        self.files.contains(path)
            || path
                .parent()
                .is_some_and(|parent| self.dirs.contains(parent))
    }
}

/// Whether `id` is the root package and `path` is outside its own sources.
fn root_fallthrough(root: &Option<RootOwnership>, id: &PackageId, path: &Utf8Path) -> bool {
    root.as_ref()
        .is_some_and(|root| root.package_id == *id && !root.owns(path))
}

/// The package a path would ancestor-match to, as the determinator's own
/// `member_by_path` walk resolves it after its rules. A mark-nothing rule
/// may still consume a file that lives inside a package, and such a file
/// belongs to its package because `include_str!` and `#![doc]` compile it
/// into the crate.
fn ancestor_owner(graph: &PackageGraph, path: &Utf8Path) -> Option<PackageId> {
    path.ancestors().find_map(|ancestor| {
        graph
            .workspace()
            .member_by_path(ancestor)
            .ok()
            .map(|package| package.id().clone())
    })
}

fn member_names(graph: &PackageGraph) -> Vec<String> {
    let mut members: Vec<String> = graph
        .workspace()
        .iter()
        .map(|package| package.name().to_string())
        .collect();
    members.sort_unstable();
    members
}

fn render(
    merge_base: Option<String>,
    workspace: bool,
    mut affected: Vec<String>,
    members: Vec<String>,
    owners: BTreeMap<String, Option<String>>,
) -> Report {
    affected.sort_unstable();
    affected.dedup();
    let (packages, package_args) = if workspace {
        ("workspace".to_string(), String::new())
    } else {
        (
            affected.join(" "),
            affected
                .iter()
                .map(|name| format!("-p {name}"))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    Report {
        merge_base,
        workspace,
        affected,
        packages,
        package_args,
        members,
        owners,
    }
}

fn report(args: &Args) -> Result<Report> {
    let head_graph = cargo_metadata(&args.root.join("Cargo.toml"), args.locked)?;
    let Some(base) = &args.base else {
        info!("no base revision: the whole workspace is in scope");
        return Ok(render(
            None,
            true,
            Vec::new(),
            member_names(&head_graph),
            BTreeMap::new(),
        ));
    };

    // The merge-base is the honest comparison point: a pull request's
    // reported base may sit behind commits the branch already merged.
    let merge_base = String::from_utf8(git(&args.root, &["merge-base", base, "HEAD"])?)
        .context("merge-base output is not UTF-8")?
        .trim()
        .to_string();
    let changed = git(
        &args.root,
        &["diff", "--name-only", "-z", &merge_base, "HEAD"],
    )?
    .split(|&byte| byte == 0)
    .filter(|token| !token.is_empty())
    .map(|token| String::from_utf8(token.to_vec()).context("path is not UTF-8"))
    .collect::<Result<Vec<String>>>()?;
    info!(%merge_base, changed = changed.len(), "diff computed");

    let base_worktree = BaseWorktree::add(&args.root, &merge_base)?;
    let base_graph = cargo_metadata(&base_worktree.path.join("Cargo.toml"), false)?;

    let rules_text = std::fs::read_to_string(&args.rules)
        .with_context(|| format!("reading rules file {}", args.rules))?;
    let rules = DeterminatorRules::parse(&rules_text)
        .with_context(|| format!("rules file {} did not parse", args.rules))?;
    let mut determinator = Determinator::new(&base_graph, &head_graph);
    determinator
        .set_rules(&rules)
        .context("the rules file does not resolve against this workspace")?;
    determinator.add_changed_paths(changed.iter().map(String::as_str));

    let root = RootOwnership::new(&head_graph);

    // A path that matched nothing, matched only the root package by
    // directory without being part of its declared sources, or hit a rule
    // that marks everything means the diff can reach anywhere: report
    // `workspace` rather than a list that happens to equal it.
    let mut workspace = false;
    let mut owners: BTreeMap<String, Option<String>> = BTreeMap::new();
    // Packages the rules swallowed but that own the file anyway; reverse
    // dependencies are not pulled in, since an include_str'd document
    // compiles into its owning crate alone.
    let mut manual_affected: BTreeSet<String> = BTreeSet::new();
    for path in &changed {
        let utf8 = Utf8Path::new(path);
        let mut matched: Vec<&PackageId> = Vec::new();
        let verdict = determinator.match_path(path.as_str(), |id| matched.push(id));
        debug!(path, ?verdict, "path matched");
        match verdict {
            PathMatch::RuleMatchedAll | PathMatch::NoMatches => {
                workspace = true;
                owners.insert(path.clone(), None);
            }
            PathMatch::RuleMatched(_) => match ancestor_owner(&head_graph, utf8) {
                Some(id) if !root_fallthrough(&root, &id, utf8) => {
                    let name = package_name(&head_graph, &id);
                    manual_affected.insert(name.clone());
                    owners.insert(path.clone(), Some(name));
                }
                _ => {
                    owners.insert(path.clone(), None);
                }
            },
            PathMatch::AncestorMatched => {
                match matched
                    .iter()
                    .copied()
                    .find(|id| !root_fallthrough(&root, id, utf8))
                {
                    Some(id) => {
                        owners.insert(path.clone(), Some(package_name(&head_graph, id)));
                    }
                    None => {
                        workspace = true;
                        owners.insert(path.clone(), None);
                    }
                }
            }
        }
    }

    let set = determinator.compute();
    let mut affected: Vec<String> = set
        .affected_set
        .packages(DependencyDirection::Forward)
        .map(|metadata| metadata.name().to_string())
        .collect();
    affected.extend(manual_affected);

    Ok(render(
        Some(merge_base),
        workspace,
        affected,
        member_names(&head_graph),
        owners,
    ))
}

fn package_name(graph: &PackageGraph, id: &PackageId) -> String {
    graph
        .metadata(id)
        .expect("ancestor match is a known package")
        .name()
        .to_string()
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let report = report(&args)?;
    let json = serde_json::to_string(&report).context("serializing the report")?;
    println!("{json}");
    Ok(())
}
