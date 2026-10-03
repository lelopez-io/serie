use std::{cell::Cell, fs, path::Path, process::Command};

use crate::{
    git::{self, CommitType, Repository},
    graph,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn load(path: &Path, range: &str, max_count: Option<usize>) -> git::Repository {
    Repository::load(
        path,
        git::SortCommit::Chronological,
        max_count,
        Some(range),
        true,
    )
    .unwrap()
}

fn subjects(repository: &Repository) -> Vec<String> {
    repository
        .all_commits()
        .iter()
        .map(|c| match c.commit_type {
            CommitType::WorkingTree => "WIP".into(),
            CommitType::Stash => format!("stash: {}", c.subject),
            CommitType::Commit => c.subject.clone(),
        })
        .collect()
}

// master: c1 c2, other: o1 off c1, parent: p1 off c2, child: k1 k2.
fn stack(git: &TestGit) {
    git.init();
    git.commit("c1");
    git.run(&["branch", "other"]);
    git.commit("c2");
    git.run(&["tag", "v1"]);
    git.run(&["checkout", "-b", "parent"]);
    git.commit("p1");
    git.run(&["checkout", "-b", "child"]);
    git.commit("k1");
    git.commit("k2");
    git.run(&["checkout", "other"]);
    git.commit("o1");
    git.run(&["checkout", "child"]);
}

#[test]
fn range_loads_only_the_branch() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);

    let repository = load(dir.path(), "parent..HEAD", None);

    assert_eq!(subjects(&repository), ["WIP", "k2", "k1"]);
    let refs: Vec<&str> = repository.all_refs().iter().map(|r| r.name()).collect();
    assert_eq!(refs, ["child"]);
    Ok(())
}

#[test]
fn range_with_no_commits_keeps_the_worktree_row() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);
    git.run(&["checkout", "-b", "fresh", "parent"]);

    let repository = load(dir.path(), "parent..HEAD", None);

    assert_eq!(subjects(&repository), ["WIP"]);
    let graph = graph::calc_graph(&repository);
    assert_eq!(graph.commits.len(), 1);
    Ok(())
}

#[test]
fn range_keeps_only_stashes_based_inside_it() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);
    git.write("f.txt", "k3\n");
    git.run(&["add", "f.txt"]);
    git.commit("k3");
    git.write("f.txt", "inside\n");
    git.run(&["stash", "push", "-m", "inside"]);
    git.run(&["checkout", "other"]);
    git.write("g.txt", "outside\n");
    git.run(&["add", "g.txt"]);
    git.run(&["stash", "push", "-m", "outside"]);
    git.run(&["checkout", "child"]);

    let repository = load(dir.path(), "parent..HEAD", None);

    assert_eq!(
        subjects(&repository),
        ["WIP", "stash: On child: inside", "k3", "k2", "k1"]
    );
    let refs: Vec<&str> = repository.all_refs().iter().map(|r| r.name()).collect();
    assert!(refs.contains(&"stash@{1}"));
    assert!(!refs.contains(&"stash@{0}"));
    Ok(())
}

#[test]
fn range_from_a_merge_draws() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);
    // parent's tip becomes a merge, so the range starts at one.
    git.run(&["checkout", "parent"]);
    git.run(&["merge", "--no-ff", "-m", "m1", "other"]);
    git.run(&["checkout", "-b", "on-merge"]);
    git.commit("n1");
    // A merge inside the range whose second side is partly outside it.
    git.run(&["merge", "--no-ff", "-m", "m2", "child"]);
    git.commit("n2");

    let repository = load(dir.path(), "parent..HEAD", None);
    assert_eq!(subjects(&repository), ["WIP", "n2", "m2", "n1", "k2", "k1"]);
    let graph = graph::calc_graph(&repository);
    assert_eq!(graph.commits.len(), 6);

    // The oldest row is itself a merge with neither parent loaded.
    let repository = load(dir.path(), "HEAD~2..HEAD ^child", None);
    assert_eq!(subjects(&repository), ["WIP", "n2", "m2"]);
    let graph = graph::calc_graph(&repository);
    assert_eq!(graph.commits.len(), 3);
    Ok(())
}

#[test]
fn range_line_stats_cover_its_rows() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);
    git.write("f.txt", "a\nb\nc\n");
    git.run(&["add", "f.txt"]);
    git.commit("k3");
    // Newer commits elsewhere fill an all-branches log first.
    git.run(&["checkout", "other"]);
    for n in 0..3 {
        git.commit(&format!("o{}", n + 2));
    }
    git.run(&["checkout", "child"]);

    let repository = load(dir.path(), "parent..HEAD", Some(2));
    let k3 = repository
        .all_commits()
        .into_iter()
        .find(|c| c.subject == "k3")
        .unwrap()
        .commit_hash
        .clone();
    assert_eq!(repository.line_stats(&k3), Some((3, 0)));
    Ok(())
}

#[test]
fn bad_range_is_an_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let git = TestGit::new(dir.path());
    stack(&git);

    let err = Repository::load(
        dir.path(),
        git::SortCommit::Chronological,
        None,
        Some("nope..HEAD"),
        true,
    )
    .unwrap_err();
    assert!(err.to_string().starts_with("bad range nope..HEAD"), "{err}");
    Ok(())
}

struct TestGit<'a> {
    path: &'a Path,
    // Each commit a minute apart, so chronological order is the
    // order they were made.
    minute: Cell<u32>,
}

impl TestGit<'_> {
    fn new(path: &Path) -> TestGit<'_> {
        TestGit {
            path,
            minute: Cell::new(0),
        }
    }

    fn init(&self) {
        self.run(&["init", "-b", "master"]);
    }

    fn commit(&self, message: &str) {
        self.run(&["commit", "--allow-empty", "-m", message]);
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.path.join(name), content).unwrap();
    }

    fn run(&self, args: &[&str]) {
        let minute = self.minute.get();
        self.minute.set(minute + 1);
        let date = format!("2024-01-01T{:02}:{:02}:00+00:00", minute / 60, minute % 60);
        let status = Command::new("git")
            .args(args)
            .current_dir(self.path)
            .env("GIT_AUTHOR_NAME", "Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", "Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "true")
            .env("HOME", "/dev/null")
            .output()
            .unwrap_or_else(|_| panic!("failed to execute git {}", args.join(" ")));
        assert!(
            status.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&status.stderr)
        );
    }
}
