//! Regression tests for filesystem entries that aren't plain readable files:
//! FIFOs, symlinks, unreadable entries, and pre-epoch timestamps.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slog::{o, Discard, Logger};

async fn run(root: &Path, expr: &str) -> Vec<String> {
    run_with(root, expr, detect::RuntimeConfig::default(), false).await
}

async fn run_following(root: &Path, expr: &str, respect_gitignore: bool) -> Vec<String> {
    let config = detect::RuntimeConfig {
        follow_links: true,
        ..Default::default()
    };
    run_with(root, expr, config, respect_gitignore).await
}

async fn run_with(
    root: &Path,
    expr: &str,
    config: detect::RuntimeConfig,
    respect_gitignore: bool,
) -> Vec<String> {
    let mut found = Vec::new();
    detect::parse_and_run_fs(
        Logger::root(Discard, o!()),
        root,
        respect_gitignore,
        expr.to_owned(),
        config,
        |p| found.push(p.file_name().unwrap().to_string_lossy().to_string()),
    )
    .await
    .unwrap();
    found.sort();
    found
}

/// chmod 000 for the guard's lifetime, restoring on drop (even on panic) so
/// tempdir cleanup can remove the entry
struct Unreadable<'a>(&'a Path);

impl<'a> Unreadable<'a> {
    fn new(path: &'a Path) -> Self {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        Self(path)
    }
}

impl Drop for Unreadable<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success());
}

#[test]
fn test_fifo_does_not_block() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
    mkfifo(&tmp.path().join("pipe"));

    // Each of these used to hang forever opening the FIFO
    let queries = [
        ("size < 1mb", vec!["a.txt", "pipe"]),
        ("type == fifo", vec!["pipe"]),
        ("content contains hello", vec!["a.txt"]),
        ("NOT content contains hello", vec!["pipe"]),
    ];
    for (expr, expected) in queries {
        // a blocked open() can't be cancelled from inside tokio, so run the
        // query on its own thread and abandon it if it hangs
        let (tx, rx) = std::sync::mpsc::channel();
        let root = tmp.path().to_owned();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _ = tx.send(rt.block_on(run(&root, expr)));
        });
        let found = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("query {expr:?} hung on a FIFO"));
        assert_eq!(found, expected, "query: {expr}");
    }
}

#[tokio::test]
async fn test_symlink_type_matches_link_not_target() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target.txt");
    std::fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("missing"), tmp.path().join("dangling")).unwrap();

    assert_eq!(
        run(tmp.path(), "type == symlink").await,
        vec!["dangling", "link"]
    );
    assert_eq!(run(tmp.path(), "type == file").await, vec!["target.txt"]);
    assert_eq!(
        run(tmp.path(), "content contains target").await,
        vec!["target.txt"]
    );

    // -L evaluates against link targets; dangling links are skipped
    assert_eq!(
        run_following(tmp.path(), "type == file", false).await,
        vec!["link", "target.txt"]
    );
    assert_eq!(
        run_following(tmp.path(), "content contains target", false).await,
        vec!["link", "target.txt"]
    );
    assert!(run_following(tmp.path(), "type == symlink", false)
        .await
        .is_empty());
}

#[tokio::test]
async fn test_follow_links_descends_into_linked_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("real")).unwrap();
    std::fs::write(tmp.path().join("real/inner.json"), r#"{"a": 1}"#).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("alias")).unwrap();

    assert_eq!(run(tmp.path(), "json:.a == 1").await, vec!["inner.json"]);
    assert_eq!(
        run_following(tmp.path(), "json:.a == 1", false).await,
        vec!["inner.json", "inner.json"]
    );
    assert_eq!(
        run_following(tmp.path(), "type == dir", false).await,
        vec!["alias", "real"]
    );
}

#[tokio::test]
async fn test_unreadable_entries_match_metadata_queries() {
    let tmp = tempfile::tempdir().unwrap();
    if is_root() {
        eprintln!("skipping: root can read 000 entries");
        return;
    }
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "classified").unwrap();
    let locked = tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let _secret = Unreadable::new(&secret);
    let _locked = Unreadable::new(&locked);

    assert_eq!(
        run(tmp.path(), "type == file AND size > 0").await,
        vec!["secret.txt"]
    );
    assert_eq!(run(tmp.path(), "type == dir").await, vec!["locked"]);
}

#[tokio::test]
async fn test_pre_epoch_mtime_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("old.txt");
    std::fs::write(&old, "").unwrap();
    std::fs::write(tmp.path().join("new.txt"), "").unwrap();
    let file = std::fs::File::options().write(true).open(&old).unwrap();
    file.set_modified(UNIX_EPOCH - Duration::from_secs(10 * 365 * 86400))
        .unwrap();

    assert_eq!(
        run(tmp.path(), "modified < 1965-01-01").await,
        vec!["old.txt"]
    );
    assert_eq!(
        run(tmp.path(), "modified > 1965-01-01").await,
        vec!["new.txt"]
    );
}

#[tokio::test]
async fn test_created_is_birth_time_not_ctime() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("f.txt");
    std::fs::write(&file, "").unwrap();

    let Ok(birth) = std::fs::metadata(&file).unwrap().created() else {
        eprintln!("skipping: filesystem doesn't record birth time");
        return;
    };
    // chmod bumps ctime but not birth time; sleep on both sides of the
    // cutoff since the query only has second resolution
    std::thread::sleep(Duration::from_millis(1100));
    let cutoff = SystemTime::now();
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(birth < cutoff);

    let cutoff: chrono::DateTime<chrono::Local> = cutoff.into();
    let expr = format!("created > {}", cutoff.format("%Y-%m-%dT%H:%M:%S"));
    assert!(run(tmp.path(), &expr).await.is_empty(), "query: {expr}");
}

#[test]
fn test_logs_go_to_stderr_not_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "").unwrap();
    let locked = tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let _locked = Unreadable::new(&locked);

    let out = Command::new(env!("CARGO_BIN_EXE_detect"))
        .args(["-l", "info", "name == a.txt"])
        .arg(tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().collect::<Vec<_>>(), vec!["./a.txt"]);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("INFO"), "expected logs on stderr: {stderr}");
}

#[tokio::test]
async fn test_follow_links_respects_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "ignored*\n").unwrap();
    std::fs::write(tmp.path().join("target.txt"), "").unwrap();
    for name in ["ignored_link", "kept_link"] {
        std::os::unix::fs::symlink(tmp.path().join("target.txt"), tmp.path().join(name)).unwrap();
    }
    std::os::unix::fs::symlink(
        tmp.path().join("missing"),
        tmp.path().join("ignored_dangling"),
    )
    .unwrap();

    assert_eq!(
        run_following(tmp.path(), "name ~= link", true).await,
        vec!["kept_link"]
    );
    assert!(run_following(tmp.path(), "name ~= dangling", true)
        .await
        .is_empty());
}

#[tokio::test]
async fn test_out_of_range_timestamps_do_not_panic() {
    // ext4 clamps timestamps, tmpfs doesn't
    let Ok(tmp) = tempfile::tempdir_in("/dev/shm") else {
        eprintln!("skipping: no /dev/shm");
        return;
    };
    let far = [
        UNIX_EPOCH + Duration::from_secs(100_000_000_000_000),
        UNIX_EPOCH - Duration::from_secs(100_000_000_000_000),
    ];
    for (i, t) in far.into_iter().enumerate() {
        let path = tmp.path().join(format!("f{i}"));
        std::fs::write(&path, "").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        if file.set_modified(t).is_err() {
            eprintln!("skipping: can't set extreme mtime");
            return;
        }
    }

    assert_eq!(run(tmp.path(), "modified > 2000-01-01").await, vec!["f0"]);
    assert_eq!(run(tmp.path(), "modified < 2000-01-01").await, vec!["f1"]);
    assert!(run(tmp.path(), "modified == 2000-01-01").await.is_empty());
    assert_eq!(
        run(tmp.path(), "modified != 2000-01-01").await,
        vec!["f0", "f1"]
    );
}

#[test]
fn test_output_paths_are_relative_to_root() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("sub/a.txt"), "").unwrap();
    let link = tempfile::tempdir().unwrap();
    let linked_root = link.path().join("root");
    std::os::unix::fs::symlink(tmp.path(), &linked_root).unwrap();

    let detect = |root: &Path, cwd: &Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_detect"))
            .args(["name == a.txt"])
            .arg(root)
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(detect(Path::new("."), tmp.path()), "./sub/a.txt\n");
    assert_eq!(detect(tmp.path(), link.path()), "./sub/a.txt\n");
    assert_eq!(detect(&linked_root, link.path()), "./sub/a.txt\n");
    // relative roots print paths usable from the current dir
    assert_eq!(detect(Path::new("root"), link.path()), "./root/sub/a.txt\n");
    assert_eq!(
        detect(Path::new("./root/"), link.path()),
        "./root/sub/a.txt\n"
    );
    assert_eq!(
        detect(Path::new(".."), &tmp.path().join("sub")),
        "../sub/a.txt\n"
    );
}

#[tokio::test]
async fn test_huge_relative_time_is_a_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    for expr in ["modified > -99999999999d", "modified > -9999999999999w"] {
        let result = detect::parse_and_run_fs(
            Logger::root(Discard, o!()),
            tmp.path(),
            false,
            expr.to_owned(),
            detect::RuntimeConfig::default(),
            |_| {},
        )
        .await;
        assert!(result.is_err(), "query: {expr}");
    }
}
