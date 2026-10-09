use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temporary_directory(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("dev-integration-{label}-{nonce}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn run(current_dir: &Path, program: &str, args: &[&str]) -> Output {
    run_with_env(current_dir, program, args, &[])
}

fn run_with_env(
    current_dir: &Path,
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(program);
    command.args(args).current_dir(current_dir);
    for (name, value) in environment {
        command.env(name, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("could not run {program}: {error}"));
    if !output.status.success() {
        panic!(
            "{program} {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

fn wait_for_contents(path: &Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if fs::read_to_string(path)
            .map(|contents| contents.trim() == expected)
            .unwrap_or(false)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {} to contain {expected:?}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn creates_reports_and_destroys_real_git_worktrees() {
    let sandbox = temporary_directory("git");
    let source = sandbox.join("source");
    let project = sandbox.join("project");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&project).unwrap();

    run(&source, "git", &["init", "--initial-branch=main"]);
    run(&source, "git", &["config", "user.name", "Dev Test"]);
    run(
        &source,
        "git",
        &["config", "user.email", "dev-test@example.invalid"],
    );
    fs::write(source.join("README.md"), "fixture\n").unwrap();
    run(&source, "git", &["add", "README.md"]);
    run(&source, "git", &["commit", "-m", "initial"]);

    fs::write(
        project.join("project.toml"),
        format!(
            "[project]\nname = \"fixture\"\n\n[repositories]\napi = {:?}\n",
            source.to_string_lossy()
        ),
    )
    .unwrap();

    let binary = env!("CARGO_BIN_EXE_dev");
    run(
        &project,
        binary,
        &["task", "new", "oauth", "--objective", "Implement OAuth"],
    );
    let task = project.join("tasks/oauth");
    assert!(task.join("api/.git").exists());
    assert!(task.join("AGENTS.md").is_file());
    assert!(task.join("OBJECTIVE.md").is_file());
    assert!(task.join(".dev-task.toml").is_file());
    assert_eq!(
        fs::read_to_string(task.join(".dev-status")).unwrap(),
        "created\n"
    );

    let tmux_tmp = sandbox.join("tmux");
    fs::create_dir_all(&tmux_tmp).unwrap();
    let tmux_tmp = tmux_tmp.to_string_lossy().into_owned();
    let base_environment = [("TMUX_TMPDIR", tmux_tmp.as_str())];

    let status = run_with_env(
        &project,
        binary,
        &["task", "status", "oauth"],
        &base_environment,
    );
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("Status:     created"));
    assert!(status.contains("api: clean"));

    fs::write(task.join("api/uncommitted.txt"), "do not lose me\n").unwrap();
    let refused = Command::new(binary)
        .args(["task", "destroy", "oauth"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("uncommitted changes in api"));
    fs::remove_file(task.join("api/uncommitted.txt")).unwrap();

    if Command::new("tmux").arg("-V").output().is_ok() {
        let fake_bin = sandbox.join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_codex = fake_bin.join("codex");
        fs::write(&fake_codex, "#!/bin/sh\nexec sleep 300\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o755)).unwrap();

        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let environment = [("PATH", path.as_str()), ("TMUX_TMPDIR", tmux_tmp.as_str())];

        run_with_env(&project, binary, &["task", "start", "oauth"], &environment);
        wait_for_contents(&task.join(".dev-worker-status"), "running");
        let running = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        let running = String::from_utf8_lossy(&running.stdout);
        assert!(running.contains("Status:     running"));
        assert!(running.contains("Worker:     running"));
        assert!(running.contains("Tmux:       detached"));

        let listed = run_with_env(&project, binary, &["task", "list"], &environment);
        let listed = String::from_utf8_lossy(&listed.stdout);
        assert!(listed.contains("WORKER"));
        assert!(listed.contains("TMUX"));
        assert!(listed.contains("detached"));

        fs::write(task.join(".dev-status"), "blocked\n").unwrap();
        let blocked = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        let blocked = String::from_utf8_lossy(&blocked.stdout);
        assert!(blocked.contains("Status:     blocked"));
        assert!(blocked.contains("Worker:     running"));
        assert!(blocked.contains("Tmux:       detached"));

        let blocked_list = run_with_env(&project, binary, &["task", "list"], &environment);
        let blocked_list = String::from_utf8_lossy(&blocked_list.stdout);
        assert!(blocked_list.contains("oauth"));
        assert!(blocked_list.contains("blocked"));
        assert!(blocked_list.contains("running"));

        fs::write(task.join(".dev-status"), "working\n").unwrap();
        let resumed = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        assert!(String::from_utf8_lossy(&resumed.stdout).contains("Status:     running"));

        fs::write(task.join(".dev-status"), "complete\n").unwrap();
        let completed = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        let completed = String::from_utf8_lossy(&completed.stdout);
        assert!(completed.contains("Status:     complete"));
        assert!(completed.contains("Tmux:       detached"));

        let panes = run_with_env(
            &project,
            "tmux",
            &[
                "list-panes",
                "-t",
                "dev-fixture-oauth",
                "-F",
                "#{pane_index}",
            ],
            &environment,
        );
        assert_eq!(String::from_utf8_lossy(&panes.stdout).lines().count(), 2);

        run_with_env(&project, binary, &["task", "stop", "oauth"], &environment);
        let stopped = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        let stopped = String::from_utf8_lossy(&stopped.stdout);
        assert!(stopped.contains("Status:     complete"));
        assert!(stopped.contains("Worker:     stopped"));
        assert!(stopped.contains("Tmux:       -"));

        fs::write(&fake_codex, "#!/bin/sh\nexit 7\n").unwrap();
        run_with_env(
            &project,
            binary,
            &["task", "start", "oauth", "--new-session"],
            &environment,
        );
        wait_for_contents(&task.join(".dev-worker-status"), "exited:7");
        let failed = run_with_env(&project, binary, &["task", "status", "oauth"], &environment);
        let failed = String::from_utf8_lossy(&failed.stdout);
        assert!(failed.contains("Status:     running"));
        assert!(failed.contains("Worker:     failed (7)"));
        assert!(failed.contains("Tmux:       detached"));

        let failed_panes = run_with_env(
            &project,
            "tmux",
            &[
                "list-panes",
                "-t",
                "dev-fixture-oauth",
                "-F",
                "#{pane_index}",
            ],
            &environment,
        );
        assert_eq!(
            String::from_utf8_lossy(&failed_panes.stdout)
                .lines()
                .count(),
            2
        );

        run_with_env(&project, binary, &["task", "stop", "oauth"], &environment);
    }

    run(
        &project,
        binary,
        &[
            "task",
            "new",
            "broken",
            "--objective",
            "Exercise list isolation",
        ],
    );
    fs::write(project.join("tasks/broken/.dev-status"), "not-a-status\n").unwrap();
    let damaged_list = Command::new(binary)
        .args(["task", "list"])
        .current_dir(&project)
        .env("TMUX_TMPDIR", tmux_tmp.as_str())
        .output()
        .unwrap();
    assert!(!damaged_list.status.success());
    let damaged_stdout = String::from_utf8_lossy(&damaged_list.stdout);
    assert!(damaged_stdout.contains("broken"));
    assert!(damaged_stdout.contains("error"));
    assert!(damaged_stdout.contains("oauth"));
    assert!(String::from_utf8_lossy(&damaged_list.stderr).contains("invalid task status"));
    run_with_env(
        &project,
        binary,
        &["task", "destroy", "broken", "--force"],
        &base_environment,
    );

    let listed = run_with_env(&project, binary, &["task", "list"], &base_environment);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("oauth"));

    run_with_env(
        &project,
        binary,
        &["task", "destroy", "oauth", "--force"],
        &base_environment,
    );
    assert!(!task.exists());

    let worktrees = run(&project, "git", &["-C", "main/api", "worktree", "list"]);
    assert!(!String::from_utf8_lossy(&worktrees.stdout).contains("tasks/oauth"));

    run(
        &project,
        binary,
        &["task", "new", "oauth", "--objective", "Continue OAuth"],
    );
    assert!(task.join("api/.git").exists());
    run_with_env(
        &project,
        binary,
        &["task", "destroy", "oauth", "--force"],
        &base_environment,
    );

    fs::remove_dir_all(sandbox).unwrap();
}
