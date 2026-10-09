use crate::config::{ProjectConfig, find_project_root, validate_component};
use crate::{Cli, Command, CommandOutput, CommandRunner, DevError, Result, TaskCommand};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const METADATA_FILE: &str = ".dev-task.toml";
const STATUS_FILE: &str = ".dev-status";
const WORKER_STATUS_FILE: &str = ".dev-worker-status";
const OBJECTIVE_FILE: &str = "OBJECTIVE.md";
const AGENTS_FILE: &str = "AGENTS.md";
const LIFECYCLE_CONTRACT_START: &str = "<!-- dev:lifecycle:start -->";
const LIFECYCLE_CONTRACT_END: &str = "<!-- dev:lifecycle:end -->";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskMetadata {
    name: String,
    project: String,
    objective: String,
    state: TaskState,
    created_at_unix: u64,
    session: String,
    repositories: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TaskState {
    Created,
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskSignal {
    Created,
    Working,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerSignal {
    Running,
    Exited(i32),
}

impl std::fmt::Display for TaskSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(formatter, "created"),
            Self::Working => write!(formatter, "working"),
            Self::Complete => write!(formatter, "complete"),
            Self::Blocked => write!(formatter, "blocked"),
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(formatter, "created"),
            Self::Running => write!(formatter, "running"),
            Self::Stopped => write!(formatter, "stopped"),
        }
    }
}

pub struct App<R> {
    current_dir: PathBuf,
    runner: R,
}

impl<R: CommandRunner> App<R> {
    pub fn new(current_dir: PathBuf, runner: R) -> Self {
        Self {
            current_dir,
            runner,
        }
    }

    pub fn execute(&mut self, cli: Cli) -> Result<()> {
        let root = find_project_root(&self.current_dir)?;
        let config = ProjectConfig::load(&root)?;

        match cli.command {
            Command::Task { command } => match command {
                TaskCommand::New {
                    name,
                    objective,
                    repositories,
                } => self.new_task(&root, &config, &name, objective, repositories),
                TaskCommand::Start {
                    name,
                    plan,
                    multi_agent,
                    new_session,
                    attach,
                } => self.start_task(&root, &name, plan, multi_agent, new_session, attach),
                TaskCommand::List => self.list_tasks(&root),
                TaskCommand::Status { name } => self.status_task(&root, &name),
                TaskCommand::Attach { name } => self.attach_task(&root, &name),
                TaskCommand::Stop { name } => self.stop_task(&root, &name),
                TaskCommand::Destroy { name, force } => self.destroy_task(&root, &name, force),
            },
        }
    }

    fn new_task(
        &self,
        root: &Path,
        config: &ProjectConfig,
        name: &str,
        objective: Option<String>,
        requested_repositories: Vec<String>,
    ) -> Result<()> {
        validate_component("task", name)?;
        let task_root = task_root(root, name);
        if task_root.exists() {
            return Err(DevError(format!("task {name:?} already exists")));
        }

        let objective = match objective {
            Some(objective) if !objective.trim().is_empty() => objective.trim().to_owned(),
            Some(_) => return Err(DevError("the task objective must not be empty".into())),
            None => prompt("What are you trying to accomplish? ")?,
        };
        if objective.trim().is_empty() {
            return Err(DevError("the task objective must not be empty".into()));
        }

        let repositories = select_repositories(config, requested_repositories)?;
        fs::create_dir_all(&task_root).map_err(|error| {
            DevError(format!(
                "could not create task directory {}: {error}",
                task_root.display()
            ))
        })?;

        let mut worktrees = Vec::new();
        let result = (|| {
            fs::create_dir_all(root.join("main"))?;
            for repository in &repositories {
                let source = config.repositories.get(repository).expect("validated name");
                let main_repository = root.join("main").join(repository);
                self.ensure_main_repository(root, repository, source, &main_repository)?;

                let worktree = task_root.join(repository);
                let branch = format!("dev/{name}");
                let branch_exists = self.runner.output(
                    root,
                    "git",
                    &[
                        "-C".into(),
                        path_string(&main_repository)?,
                        "show-ref".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        format!("refs/heads/{branch}"),
                    ],
                )?;
                let mut worktree_args = vec![
                    "-C".into(),
                    path_string(&main_repository)?,
                    "worktree".into(),
                    "add".into(),
                ];
                if branch_exists.success {
                    worktree_args.extend([path_string(&worktree)?, branch]);
                } else {
                    worktree_args.extend(["-b".into(), branch.clone()]);
                    worktree_args.push(path_string(&worktree)?);
                }
                self.checked(
                    root,
                    "git",
                    worktree_args,
                    &format!("create worktree for {repository}"),
                )?;
                worktrees.push((main_repository, worktree));
            }

            let metadata = TaskMetadata {
                name: name.to_owned(),
                project: config.project.name.clone(),
                objective: objective.clone(),
                state: TaskState::Created,
                created_at_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| DevError(format!("system clock error: {error}")))?
                    .as_secs(),
                session: session_name(&config.project.name, name),
                repositories: repositories.clone(),
            };
            write_metadata(&task_root, &metadata)?;
            atomic_write(&task_root.join(STATUS_FILE), "created\n")?;
            atomic_write(
                &task_root.join(OBJECTIVE_FILE),
                &objective_document(&objective, &repositories),
            )?;
            atomic_write(
                &task_root.join(AGENTS_FILE),
                &agents_document(&repositories),
            )?;
            Ok(())
        })();

        if let Err(error) = result {
            let mut cleanup_errors = Vec::new();
            for (main_repository, worktree) in worktrees.into_iter().rev() {
                let output = self.runner.output(
                    root,
                    "git",
                    &[
                        "-C".into(),
                        main_repository.to_string_lossy().into_owned(),
                        "worktree".into(),
                        "remove".into(),
                        "--force".into(),
                        worktree.to_string_lossy().into_owned(),
                    ],
                );
                match output {
                    Ok(output) if output.success => {}
                    Ok(output) => cleanup_errors.push(
                        command_error(
                            &format!("roll back worktree {}", worktree.display()),
                            output,
                        )
                        .to_string(),
                    ),
                    Err(cleanup_error) => cleanup_errors.push(format!(
                        "could not roll back worktree {}: {cleanup_error}",
                        worktree.display()
                    )),
                }
            }
            if cleanup_errors.is_empty()
                && let Err(cleanup_error) = fs::remove_dir_all(&task_root)
                && cleanup_error.kind() != io::ErrorKind::NotFound
            {
                cleanup_errors.push(format!(
                    "could not remove task directory {}: {cleanup_error}",
                    task_root.display()
                ));
            }
            if cleanup_errors.is_empty() {
                return Err(error);
            }
            return Err(DevError(format!(
                "{error}; rollback also failed: {}. The task directory was preserved for recovery",
                cleanup_errors.join("; ")
            )));
        }

        println!("Created task {name} at {}", task_root.display());
        println!("Next: dev task start {name}");
        Ok(())
    }

    fn ensure_main_repository(
        &self,
        root: &Path,
        name: &str,
        source: &str,
        path: &Path,
    ) -> Result<()> {
        if path.exists() {
            let output = self.runner.output(
                root,
                "git",
                &[
                    "-C".into(),
                    path_string(path)?,
                    "rev-parse".into(),
                    "--git-dir".into(),
                ],
            )?;
            if !output.success {
                return Err(command_error(
                    &format!("validate main repository {name}"),
                    output,
                ));
            }
            return Ok(());
        }

        self.checked(
            root,
            "git",
            vec![
                "clone".into(),
                "--".into(),
                source.to_owned(),
                path_string(path)?,
            ],
            &format!("clone main repository {name}"),
        )?;
        Ok(())
    }

    fn start_task(
        &self,
        root: &Path,
        name: &str,
        plan: bool,
        multi_agent: bool,
        new_session: bool,
        attach: bool,
    ) -> Result<()> {
        let task_root = checked_task_root(root, name)?;
        let mut metadata = read_metadata(&task_root)?;
        if self.session_exists(root, &metadata.session)? {
            return Err(DevError(format!(
                "task {name:?} is already running; use `dev task attach {name}`"
            )));
        }

        let task_path = path_string(&task_root)?;
        ensure_task_lifecycle_contract(&task_root)?;
        let resume = !new_session && !matches!(metadata.state, TaskState::Created);
        let previous_signal = read_task_signal(&task_root)?;
        let status_changed = !resume || previous_signal.is_none();
        if status_changed {
            atomic_write(&task_root.join(STATUS_FILE), "working\n")?;
        }
        let shell_command = codex_shell_command(&task_path, plan, multi_agent, resume);
        if let Err(error) = self.checked(
            root,
            "tmux",
            vec![
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                metadata.session.clone(),
                "-c".into(),
                task_path.clone(),
                shell_command,
            ],
            "create task tmux session",
        ) {
            return Err(start_error_with_status_rollback(
                &task_root,
                previous_signal,
                status_changed,
                error,
            ));
        }

        let setup_result = (|| {
            self.checked(
                root,
                "tmux",
                vec![
                    "split-window".into(),
                    "-h".into(),
                    "-t".into(),
                    metadata.session.clone(),
                    "-c".into(),
                    task_path,
                ],
                "create task shell pane",
            )?;
            self.checked(
                root,
                "tmux",
                vec![
                    "select-pane".into(),
                    "-t".into(),
                    format!("{}:0.0", metadata.session),
                ],
                "select Codex pane",
            )?;
            Ok(())
        })();
        if let Err(error) = setup_result {
            let _ = self.runner.output(
                root,
                "tmux",
                &["kill-session".into(), "-t".into(), metadata.session.clone()],
            );
            return Err(start_error_with_status_rollback(
                &task_root,
                previous_signal,
                status_changed,
                error,
            ));
        }

        metadata.state = TaskState::Running;
        if let Err(error) = write_metadata(&task_root, &metadata) {
            let _ = self.runner.output(
                root,
                "tmux",
                &["kill-session".into(), "-t".into(), metadata.session.clone()],
            );
            return Err(start_error_with_status_rollback(
                &task_root,
                previous_signal,
                status_changed,
                error,
            ));
        }
        let codex_action = if resume {
            "resuming its latest Codex session"
        } else {
            "starting a new Codex session"
        };
        println!(
            "Started task {name} in tmux session {} ({codex_action})",
            metadata.session
        );

        if attach {
            self.attach_session(root, &metadata)?;
        } else {
            println!("Attach with: dev task attach {name}");
        }
        Ok(())
    }

    fn list_tasks(&self, root: &Path) -> Result<()> {
        let tasks = root.join("tasks");
        println!(
            "{:<24} {:<10} {:<12} {:<16} {:<24} REPOSITORIES",
            "TASK", "STATUS", "WORKER", "TMUX", "SESSION"
        );
        if !tasks.is_dir() {
            return Ok(());
        }

        let mut errors = Vec::new();
        let mut entries = fs::read_dir(&tasks)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_type()?.is_dir() || !entry.path().join(METADATA_FILE).is_file() {
                continue;
            }
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            let metadata = match read_metadata(&entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    println!(
                        "{:<24} {:<10} {:<12} {:<16} {:<24} -",
                        entry_name, "error", "-", "-", "-"
                    );
                    errors.push(format!("{entry_name}: {error}"));
                    continue;
                }
            };
            let runtime = match self.runtime_state(root, &metadata) {
                Ok(runtime) => runtime,
                Err(error) => {
                    println!(
                        "{:<24} {:<10} {:<12} {:<16} {:<24} {}",
                        metadata.name,
                        "error",
                        "-",
                        "-",
                        metadata.session,
                        metadata.repositories.join(",")
                    );
                    errors.push(format!("{}: {error}", metadata.name));
                    continue;
                }
            };
            println!(
                "{:<24} {:<10} {:<12} {:<16} {:<24} {}",
                metadata.name,
                runtime.lifecycle,
                runtime.worker,
                runtime.tmux,
                metadata.session,
                metadata.repositories.join(",")
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DevError(format!(
                "could not inspect {} task(s): {}",
                errors.len(),
                errors.join("; ")
            )))
        }
    }

    fn status_task(&self, root: &Path, name: &str) -> Result<()> {
        let task_root = checked_task_root(root, name)?;
        let metadata = read_metadata(&task_root)?;
        let runtime = self.runtime_state(root, &metadata)?;

        println!("Task:       {}", metadata.name);
        println!("Project:    {}", metadata.project);
        println!("Status:     {}", runtime.lifecycle);
        println!("Worker:     {}", runtime.worker);
        println!("Tmux:       {}", runtime.tmux);
        println!("Session:    {}", metadata.session);
        println!("Workspace:  {}", task_root.display());
        println!("Created:    {} (Unix time)", metadata.created_at_unix);
        println!("Objective:  {}", metadata.objective);
        println!("Repositories:");
        for repository in &metadata.repositories {
            let worktree = task_root.join(repository);
            let output = self.runner.output(
                root,
                "git",
                &[
                    "-C".into(),
                    path_string(&worktree)?,
                    "status".into(),
                    "--short".into(),
                ],
            )?;
            let repository_state = if !output.success {
                "unavailable"
            } else if output.stdout.trim().is_empty() {
                "clean"
            } else {
                "modified"
            };
            println!("  - {repository}: {repository_state}");
        }
        Ok(())
    }

    fn attach_task(&self, root: &Path, name: &str) -> Result<()> {
        let task_root = checked_task_root(root, name)?;
        let metadata = read_metadata(&task_root)?;
        if !self.session_exists(root, &metadata.session)? {
            return Err(DevError(format!(
                "task {name:?} is not running; use `dev task start {name}`"
            )));
        }
        self.attach_session(root, &metadata)
    }

    fn attach_session(&self, root: &Path, metadata: &TaskMetadata) -> Result<()> {
        let action = tmux_attach_action(std::env::var_os("TMUX").is_some());
        let success = self.runner.interactive(
            root,
            "tmux",
            &[action.into(), "-t".into(), metadata.session.clone()],
        )?;
        if success {
            Ok(())
        } else {
            Err(DevError(format!(
                "could not attach to tmux session {}",
                metadata.session
            )))
        }
    }

    fn stop_task(&self, root: &Path, name: &str) -> Result<()> {
        let task_root = checked_task_root(root, name)?;
        let mut metadata = read_metadata(&task_root)?;
        let session_exists = self.session_exists(root, &metadata.session)?;
        if !session_exists && matches!(metadata.state, TaskState::Created) {
            println!("Task {name} has not been started");
            return Ok(());
        }
        if session_exists {
            self.checked(
                root,
                "tmux",
                vec!["kill-session".into(), "-t".into(), metadata.session.clone()],
                "stop task tmux session",
            )?;
        }
        metadata.state = TaskState::Stopped;
        write_metadata(&task_root, &metadata)?;
        println!("Stopped task {name}");
        Ok(())
    }

    fn destroy_task(&self, root: &Path, name: &str, force: bool) -> Result<()> {
        let task_root = checked_task_root(root, name)?;
        let metadata = read_metadata(&task_root)?;

        if !force {
            let dirty = self.dirty_repositories(root, &task_root, &metadata)?;
            if !dirty.is_empty() {
                return Err(DevError(format!(
                    "task has uncommitted changes in {}; review them or rerun with --force",
                    dirty.join(", ")
                )));
            }
            let answer = prompt(&format!("Destroy task {name} and its worktrees? [y/N] "))?;
            if !matches!(answer.as_str(), "y" | "Y" | "yes" | "YES") {
                println!("Task not destroyed");
                return Ok(());
            }
        }

        if self.session_exists(root, &metadata.session)? {
            self.checked(
                root,
                "tmux",
                vec!["kill-session".into(), "-t".into(), metadata.session.clone()],
                "stop task tmux session",
            )?;
        }

        for repository in &metadata.repositories {
            let main_repository = root.join("main").join(repository);
            let worktree = task_root.join(repository);
            if !worktree.exists() {
                continue;
            }
            let mut args = vec![
                "-C".into(),
                path_string(&main_repository)?,
                "worktree".into(),
                "remove".into(),
            ];
            if force {
                args.push("--force".into());
            }
            args.push(path_string(&worktree)?);
            self.checked(
                root,
                "git",
                args,
                &format!("remove worktree for {repository}"),
            )?;
        }

        fs::remove_dir_all(&task_root).map_err(|error| {
            DevError(format!(
                "could not remove task directory {}: {error}",
                task_root.display()
            ))
        })?;
        println!("Destroyed task {name}; Git branches were preserved");
        Ok(())
    }

    fn dirty_repositories(
        &self,
        root: &Path,
        task_root: &Path,
        metadata: &TaskMetadata,
    ) -> Result<Vec<String>> {
        let mut dirty = Vec::new();
        for repository in &metadata.repositories {
            let worktree = task_root.join(repository);
            if !worktree.exists() {
                continue;
            }
            let output = self.runner.output(
                root,
                "git",
                &[
                    "-C".into(),
                    path_string(&worktree)?,
                    "status".into(),
                    "--porcelain".into(),
                ],
            )?;
            if !output.success {
                return Err(command_error(
                    &format!("inspect worktree for {repository}"),
                    output,
                ));
            }
            if !output.stdout.trim().is_empty() {
                dirty.push(repository.clone());
            }
        }
        Ok(dirty)
    }

    fn session_exists(&self, root: &Path, session: &str) -> Result<bool> {
        let output = self.runner.output(
            root,
            "tmux",
            &["has-session".into(), "-t".into(), session.into()],
        )?;
        Ok(output.success)
    }

    fn runtime_state(&self, root: &Path, metadata: &TaskMetadata) -> Result<RuntimeState> {
        let session_exists = self.session_exists(root, &metadata.session)?;
        let task_root = task_root(root, &metadata.name);
        let signal = read_task_signal(&task_root)?;
        let lifecycle = task_lifecycle(signal, metadata.state, session_exists);
        let worker = worker_runtime(read_worker_signal(&task_root)?, session_exists);
        if !session_exists {
            return Ok(RuntimeState {
                lifecycle,
                worker,
                tmux: "-".into(),
            });
        }

        let output = self.runner.output(
            root,
            "tmux",
            &[
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                format!("{}:", metadata.session),
                "#{session_attached}".into(),
            ],
        )?;
        if !output.success {
            return Err(command_error(
                &format!("inspect tmux session {}", metadata.session),
                output,
            ));
        }
        let clients = output.stdout.trim().parse::<usize>().map_err(|error| {
            DevError(format!(
                "tmux returned an invalid attached-client count for session {}: {:?} ({error})",
                metadata.session,
                output.stdout.trim()
            ))
        })?;
        Ok(RuntimeState {
            lifecycle,
            worker,
            tmux: tmux_attachment(clients),
        })
    }

    fn checked(
        &self,
        current_dir: &Path,
        program: &str,
        args: Vec<String>,
        action: &str,
    ) -> Result<CommandOutput> {
        let output = self.runner.output(current_dir, program, &args)?;
        if output.success {
            Ok(output)
        } else {
            Err(command_error(action, output))
        }
    }
}

struct RuntimeState {
    lifecycle: String,
    worker: String,
    tmux: String,
}

fn tmux_attachment(clients: usize) -> String {
    if clients == 0 {
        "detached".into()
    } else {
        format!("attached ({clients})")
    }
}

fn tmux_attach_action(already_in_tmux: bool) -> &'static str {
    if already_in_tmux {
        "switch-client"
    } else {
        "attach-session"
    }
}

fn select_repositories(config: &ProjectConfig, requested: Vec<String>) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(config.repositories.keys().cloned().collect());
    }

    let unique: BTreeSet<_> = requested.into_iter().collect();
    for name in &unique {
        if !config.repositories.contains_key(name) {
            return Err(DevError(format!(
                "unknown repository {name:?}; configured repositories: {}",
                config
                    .repositories
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(unique.into_iter().collect())
}

fn task_root(root: &Path, name: &str) -> PathBuf {
    root.join("tasks").join(name)
}

fn checked_task_root(root: &Path, name: &str) -> Result<PathBuf> {
    validate_component("task", name)?;
    let path = task_root(root, name);
    if !path.join(METADATA_FILE).is_file() {
        return Err(DevError(format!("task {name:?} does not exist")));
    }
    Ok(path)
}

fn read_metadata(task_root: &Path) -> Result<TaskMetadata> {
    let path = task_root.join(METADATA_FILE);
    let source = fs::read_to_string(&path)
        .map_err(|error| DevError(format!("could not read {}: {error}", path.display())))?;
    toml::from_str(&source)
        .map_err(|error| DevError(format!("invalid task metadata {}: {error}", path.display())))
}

fn write_metadata(task_root: &Path, metadata: &TaskMetadata) -> Result<()> {
    let content = toml::to_string_pretty(metadata)?;
    atomic_write(&task_root.join(METADATA_FILE), &content)
}

fn read_task_signal(task_root: &Path) -> Result<Option<TaskSignal>> {
    let path = task_root.join(STATUS_FILE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DevError(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    let signal = match source.trim() {
        "created" => TaskSignal::Created,
        "working" => TaskSignal::Working,
        "complete" => TaskSignal::Complete,
        "blocked" => TaskSignal::Blocked,
        value => {
            return Err(DevError(format!(
                "invalid task status {}: {value:?}",
                path.display()
            )));
        }
    };
    Ok(Some(signal))
}

fn read_worker_signal(task_root: &Path) -> Result<Option<WorkerSignal>> {
    let path = task_root.join(WORKER_STATUS_FILE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DevError(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    let value = source.trim();
    if value == "running" {
        return Ok(Some(WorkerSignal::Running));
    }
    if let Some(status) = value.strip_prefix("exited:") {
        let status = status.parse::<i32>().map_err(|error| {
            DevError(format!(
                "invalid worker status {}: {value:?} ({error})",
                path.display()
            ))
        })?;
        return Ok(Some(WorkerSignal::Exited(status)));
    }
    Err(DevError(format!(
        "invalid worker status {}: {value:?}",
        path.display()
    )))
}

fn restore_task_signal(task_root: &Path, signal: Option<TaskSignal>) -> Result<()> {
    let path = task_root.join(STATUS_FILE);
    if let Some(signal) = signal {
        return atomic_write(&path, &format!("{signal}\n"));
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DevError(format!(
            "could not remove {} while restoring task status: {error}",
            path.display()
        ))),
    }
}

fn start_error_with_status_rollback(
    task_root: &Path,
    previous_signal: Option<TaskSignal>,
    status_changed: bool,
    error: DevError,
) -> DevError {
    if !status_changed {
        return error;
    }
    match restore_task_signal(task_root, previous_signal) {
        Ok(()) => error,
        Err(rollback_error) => DevError(format!(
            "{error}; additionally failed to restore the previous task status: {rollback_error}"
        )),
    }
}

fn task_lifecycle(
    signal: Option<TaskSignal>,
    metadata_state: TaskState,
    session_exists: bool,
) -> String {
    match signal {
        Some(TaskSignal::Complete) => "complete".into(),
        Some(TaskSignal::Blocked) => "blocked".into(),
        Some(TaskSignal::Created) if !session_exists => "created".into(),
        Some(TaskSignal::Working) if !session_exists => "stopped".into(),
        Some(TaskSignal::Created | TaskSignal::Working) => "running".into(),
        None if session_exists => "running".into(),
        None if matches!(metadata_state, TaskState::Running) => "stopped".into(),
        None => metadata_state.to_string(),
    }
}

fn worker_runtime(signal: Option<WorkerSignal>, session_exists: bool) -> String {
    match signal {
        Some(WorkerSignal::Running) if session_exists => "running".into(),
        Some(WorkerSignal::Running) => "stopped".into(),
        Some(WorkerSignal::Exited(0)) => "exited (0)".into(),
        Some(WorkerSignal::Exited(status)) => format!("failed ({status})"),
        None => "-".into(),
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content)
        .map_err(|error| DevError(format!("could not write {}: {error}", temporary.display())))?;
    fs::rename(&temporary, path)
        .map_err(|error| DevError(format!("could not replace {}: {error}", path.display())))?;
    Ok(())
}

fn objective_document(objective: &str, repositories: &[String]) -> String {
    format!(
        "# Task Objective\n\n{objective}\n\n## Completion contract\n\n- Verify the outcome with the relevant tests, checks, or observable behavior.\n- Preserve existing behavior outside the requested scope.\n- Keep interfaces compatible across {} unless the objective requires a coordinated change.\n- Between iterations, use the latest evidence to choose the next useful action.\n- If blocked, report the attempted paths, evidence, blocker, and input needed.\n",
        repositories.join(", ")
    )
}

fn lifecycle_contract() -> String {
    format!(
        "{LIFECYCLE_CONTRACT_START}\n## Task lifecycle reporting\n\n- `.dev-status` is the machine-readable task status initialized by `dev`. During a run, only the main coordinator may update it; subagents must not modify it.\n- Keep it as exactly `working` while planning, awaiting plan approval, implementing, testing, or otherwise able to continue.\n- Immediately before marking the Goal complete, and only after verifying the completion contract, replace it with exactly `complete`.\n- When genuinely unable to continue without human input, replace it with exactly `blocked` before reporting the blocker and required decision.\n- When user input resolves a blocker, replace it with exactly `working` before continuing.\n{LIFECYCLE_CONTRACT_END}\n"
    )
}

fn ensure_task_lifecycle_contract(task_root: &Path) -> Result<()> {
    let path = task_root.join(AGENTS_FILE);
    let mut source = fs::read_to_string(&path)
        .map_err(|error| DevError(format!("could not read {}: {error}", path.display())))?;
    let has_start = source.contains(LIFECYCLE_CONTRACT_START);
    let has_end = source.contains(LIFECYCLE_CONTRACT_END);
    match (has_start, has_end) {
        (true, true) => return Ok(()),
        (true, false) | (false, true) => {
            return Err(DevError(format!(
                "task instructions {} contain an incomplete dev lifecycle section",
                path.display()
            )));
        }
        (false, false) => {}
    }
    if !source.ends_with('\n') {
        source.push('\n');
    }
    if !source.ends_with("\n\n") {
        source.push('\n');
    }
    source.push_str(&lifecycle_contract());
    atomic_write(&path, &source)
}

fn agents_document(repositories: &[String]) -> String {
    format!(
        "# Task workspace instructions\n\nThis task spans these repositories: {}.\n\nBefore making changes:\n\n- Inspect the relevant repositories and understand their interfaces.\n- In each repository, read its `AGENTS.md`. If that repository has no `AGENTS.md`, read its `CLAUDE.md` when present. These repository-level files supplement this task-level `AGENTS.md`.\n- Identify cross-repository dependencies before changing APIs.\n- Run relevant tests after changes.\n- Keep changes scoped to the task.\n- Report important cross-repository assumptions and unresolved issues.\n- Do not merge task branches; final integration remains under human control.\n\n{}",
        repositories.join(", "),
        lifecycle_contract()
    )
}

fn startup_prompt(plan: bool, multi_agent: bool) -> String {
    let mut context = format!(
        "Read the task-level {AGENTS_FILE} and {OBJECTIVE_FILE}. For each listed repository, read its repository-level AGENTS.md; only when that repository has no AGENTS.md, read its CLAUDE.md if present. Inspect all listed repositories and identify cross-repository dependencies."
    );
    if multi_agent {
        context.push_str(
            " Use Codex's built-in subagents for bounded work where parallelism materially helps. Remain the coordinator and own the overall Goal, cross-repository decisions, integration, and final verification. Begin with independent, read-heavy delegation such as repository and interface exploration, test mapping, or isolated test runs. Give every subagent explicit scope and expected output, wait for all subagents, and synthesize their results before integration. Do not assign concurrent edits to overlapping files. If implementation is delegated, give agents non-overlapping file ownership, then review and test the integrated result.",
        );
    }
    if plan {
        format!(
            "/plan {context} Produce a concrete implementation and verification plan. Wait for explicit user approval before beginning implementation or activating a Goal."
        )
    } else {
        format!(
            "{context} Start a Goal for the documented objective, preserving its boundaries and completion criteria. Then implement, test, and iterate until the Goal is complete or honestly blocked."
        )
    }
}

fn codex_shell_command(task_path: &str, plan: bool, multi_agent: bool, resume: bool) -> String {
    let permissions = if plan {
        "--approve-for-me"
    } else {
        "--sandbox workspace-write --ask-for-approval never"
    };
    let invocation = if resume {
        format!(
            "codex {permissions} -C {} resume --last",
            shell_quote(task_path)
        )
    } else {
        format!(
            "codex {permissions} -C {} {}",
            shell_quote(task_path),
            shell_quote(&startup_prompt(plan, multi_agent))
        )
    };
    let worker_status = shell_quote(&format!("{task_path}/{WORKER_STATUS_FILE}"));
    format!(
        "printf 'running\\n' > {worker_status}; {invocation}; dev_codex_status=$?; printf 'exited:%s\\n' \"$dev_codex_status\" > {worker_status}; printf '\\n[dev] Codex exited with status %s. Starting a diagnostic shell.\\n' \"$dev_codex_status\" >&2; exec \"${{SHELL:-/bin/sh}}\" -l"
    )
}

fn session_name(project: &str, task: &str) -> String {
    let project = sanitize_session_component(project);
    format!("dev-{project}-{task}")
}

fn sanitize_session_component(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = value.trim_matches('-');
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed.chars().take(40).collect()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DevError(format!("path is not valid UTF-8: {}", path.display())))
}

fn prompt(message: &str) -> Result<String> {
    eprint!("{message}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_owned())
}

fn command_error(action: &str, output: CommandOutput) -> DevError {
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if detail.is_empty() {
        DevError(format!("failed to {action}"))
    } else {
        DevError(format!("failed to {action}: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandOutput, CommandRunner};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        fail_tmux_new_session: bool,
        fail_git_worktree_add_for: Option<String>,
        fail_git_worktree_remove: bool,
    }

    impl CommandRunner for RecordingRunner {
        fn output(
            &self,
            _current_dir: &Path,
            program: &str,
            args: &[String],
        ) -> Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            let is_negative_probe = (program == "tmux"
                && args.first().map(String::as_str) == Some("has-session"))
                || (program == "git" && args.iter().any(|argument| argument == "show-ref"));
            let is_forced_failure = self.fail_tmux_new_session
                && program == "tmux"
                && args.first().map(String::as_str) == Some("new-session");
            let is_forced_git_add_failure =
                self.fail_git_worktree_add_for
                    .as_ref()
                    .is_some_and(|repository| {
                        program == "git"
                            && args.iter().any(|argument| argument == "worktree")
                            && args.iter().any(|argument| argument == "add")
                            && args.iter().any(|argument| argument.ends_with(repository))
                    });
            let is_forced_git_remove_failure = self.fail_git_worktree_remove
                && program == "git"
                && args.iter().any(|argument| argument == "worktree")
                && args.iter().any(|argument| argument == "remove");
            Ok(CommandOutput {
                success: !is_negative_probe
                    && !is_forced_failure
                    && !is_forced_git_add_failure
                    && !is_forced_git_remove_failure,
                stdout: String::new(),
                stderr: if is_forced_failure {
                    "forced tmux failure".into()
                } else if is_forced_git_add_failure {
                    "forced worktree add failure".into()
                } else if is_forced_git_remove_failure {
                    "forced worktree remove failure".into()
                } else {
                    String::new()
                },
            })
        }

        fn interactive(&self, _current_dir: &Path, program: &str, args: &[String]) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            Ok(true)
        }
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dev-{label}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn shell_quotes_paths_and_prompts() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }

    #[test]
    fn session_names_are_tmux_friendly() {
        assert_eq!(session_name("My project!", "oauth"), "dev-My-project-oauth");
        assert_eq!(session_name("!!!", "oauth"), "dev-project-oauth");
    }

    #[test]
    fn start_builds_a_two_pane_tmux_session_with_codex() {
        let root = temporary_directory("start");
        fs::write(
            root.join("project.toml"),
            "[project]\nname = \"sample\"\n\n[repositories]\napi = \"../api\"\n",
        )
        .unwrap();

        let runner = RecordingRunner::default();
        let mut app = App::new(root.clone(), runner);
        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::New {
                    name: "oauth".into(),
                    objective: Some("Implement OAuth".into()),
                    repositories: vec![],
                },
            },
        })
        .unwrap();
        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::Stop {
                    name: "oauth".into(),
                },
            },
        })
        .unwrap();
        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::Start {
                    name: "oauth".into(),
                    plan: false,
                    multi_agent: false,
                    new_session: false,
                    attach: false,
                },
            },
        })
        .unwrap();

        let calls = app.runner.calls.lock().unwrap();
        let new_session = calls
            .iter()
            .find(|(program, args)| {
                program == "tmux" && args.first().map(String::as_str) == Some("new-session")
            })
            .expect("tmux new-session call");
        assert!(
            new_session
                .1
                .iter()
                .any(|argument| argument.contains("codex --sandbox workspace-write"))
        );
        assert!(new_session.1.iter().any(|argument| {
            argument.contains("--sandbox workspace-write --ask-for-approval never")
        }));
        assert!(
            new_session
                .1
                .iter()
                .all(|argument| !argument.contains("resume --last"))
        );
        assert!(calls.iter().any(|(program, args)| {
            program == "tmux" && args.first().map(String::as_str) == Some("split-window")
        }));

        drop(calls);

        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::Start {
                    name: "oauth".into(),
                    plan: false,
                    multi_agent: false,
                    new_session: false,
                    attach: false,
                },
            },
        })
        .unwrap();
        let calls = app.runner.calls.lock().unwrap();
        let resumed_session = calls
            .iter()
            .rfind(|(program, args)| {
                program == "tmux" && args.first().map(String::as_str) == Some("new-session")
            })
            .expect("resumed tmux new-session call");
        assert!(
            resumed_session
                .1
                .iter()
                .any(|argument| argument.contains("resume --last"))
        );
        drop(calls);

        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::Start {
                    name: "oauth".into(),
                    plan: false,
                    multi_agent: false,
                    new_session: true,
                    attach: false,
                },
            },
        })
        .unwrap();
        let calls = app.runner.calls.lock().unwrap();
        let fresh_session = calls
            .iter()
            .rfind(|(program, args)| {
                program == "tmux" && args.first().map(String::as_str) == Some("new-session")
            })
            .expect("fresh tmux new-session call");
        assert!(
            fresh_session
                .1
                .iter()
                .all(|argument| !argument.contains("resume --last"))
        );

        drop(calls);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_first_start_restores_created_status() {
        let root = temporary_directory("failed-start");
        fs::write(
            root.join("project.toml"),
            "[project]\nname = \"sample\"\n\n[repositories]\napi = \"../api\"\n",
        )
        .unwrap();

        let runner = RecordingRunner {
            fail_tmux_new_session: true,
            ..RecordingRunner::default()
        };
        let mut app = App::new(root.clone(), runner);
        app.execute(Cli {
            command: Command::Task {
                command: TaskCommand::New {
                    name: "oauth".into(),
                    objective: Some("Implement OAuth".into()),
                    repositories: vec![],
                },
            },
        })
        .unwrap();

        let error = app
            .execute(Cli {
                command: Command::Task {
                    command: TaskCommand::Start {
                        name: "oauth".into(),
                        plan: false,
                        multi_agent: false,
                        new_session: false,
                        attach: false,
                    },
                },
            })
            .unwrap_err();
        assert!(error.to_string().contains("forced tmux failure"));
        assert_eq!(
            fs::read_to_string(root.join("tasks/oauth/.dev-status")).unwrap(),
            "created\n"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_task_creation_surfaces_rollback_failure_and_preserves_workspace() {
        let root = temporary_directory("failed-create-rollback");
        fs::write(
            root.join("project.toml"),
            "[project]\nname = \"sample\"\n\n[repositories]\napi = \"../api\"\nweb = \"../web\"\n",
        )
        .unwrap();

        let runner = RecordingRunner {
            fail_git_worktree_add_for: Some("web".into()),
            fail_git_worktree_remove: true,
            ..RecordingRunner::default()
        };
        let mut app = App::new(root.clone(), runner);
        let error = app
            .execute(Cli {
                command: Command::Task {
                    command: TaskCommand::New {
                        name: "oauth".into(),
                        objective: Some("Implement OAuth".into()),
                        repositories: vec![],
                    },
                },
            })
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("forced worktree add failure"));
        assert!(error.contains("rollback also failed"));
        assert!(error.contains("forced worktree remove failure"));
        assert!(root.join("tasks/oauth").is_dir());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_task_creation_removes_successfully_rolled_back_workspace() {
        let root = temporary_directory("failed-create-clean");
        fs::write(
            root.join("project.toml"),
            "[project]\nname = \"sample\"\n\n[repositories]\napi = \"../api\"\nweb = \"../web\"\n",
        )
        .unwrap();

        let runner = RecordingRunner {
            fail_git_worktree_add_for: Some("web".into()),
            ..RecordingRunner::default()
        };
        let mut app = App::new(root.clone(), runner);
        let error = app
            .execute(Cli {
                command: Command::Task {
                    command: TaskCommand::New {
                        name: "oauth".into(),
                        objective: Some("Implement OAuth".into()),
                        repositories: vec![],
                    },
                },
            })
            .unwrap_err();
        assert!(error.to_string().contains("forced worktree add failure"));
        assert!(!root.join("tasks/oauth").exists());
        assert!(
            app.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(program, args)| {
                    program == "git"
                        && args.iter().any(|argument| argument == "worktree")
                        && args.iter().any(|argument| argument == "remove")
                })
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_start_waits_for_approval_before_goal_activation() {
        let prompt = startup_prompt(true, false);
        assert!(prompt.starts_with("/plan "));
        assert!(prompt.contains("Wait for explicit user approval"));

        let command = codex_shell_command("/tmp/task", true, false, false);
        assert!(command.contains("codex --approve-for-me"));
        assert!(!command.contains("--sandbox"));
        assert!(!command.contains("--ask-for-approval never"));
        assert!(command.contains("Starting a diagnostic shell"));
    }

    #[test]
    fn repository_instructions_fall_back_to_claude_md() {
        let instructions = agents_document(&["api".into()]);
        assert!(instructions.contains("read its `AGENTS.md`"));
        assert!(instructions.contains("no `AGENTS.md`, read its `CLAUDE.md`"));
        assert!(instructions.contains("task-level `AGENTS.md`"));
        assert!(instructions.contains("only the main coordinator may update"));
        assert!(instructions.contains("exactly `complete`"));
        assert!(instructions.contains("exactly `blocked`"));
        assert!(instructions.contains(LIFECYCLE_CONTRACT_START));
        assert!(instructions.contains(LIFECYCLE_CONTRACT_END));

        let prompt = startup_prompt(false, false);
        assert!(prompt.contains("repository-level AGENTS.md"));
        assert!(prompt.contains("no AGENTS.md, read its CLAUDE.md"));
    }

    #[test]
    fn legacy_task_instructions_gain_one_managed_lifecycle_contract() {
        let root = temporary_directory("legacy-agents");
        let path = root.join(AGENTS_FILE);
        fs::write(
            &path,
            "# Existing task instructions\n\nKeep this content.\n",
        )
        .unwrap();

        ensure_task_lifecycle_contract(&root).unwrap();
        ensure_task_lifecycle_contract(&root).unwrap();

        let instructions = fs::read_to_string(&path).unwrap();
        assert!(instructions.starts_with("# Existing task instructions"));
        assert!(instructions.contains("Keep this content."));
        assert_eq!(instructions.matches(LIFECYCLE_CONTRACT_START).count(), 1);
        assert_eq!(instructions.matches(LIFECYCLE_CONTRACT_END).count(), 1);
        assert!(instructions.contains("exactly `complete`"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multi_agent_start_uses_a_bounded_coordinator_contract() {
        let prompt = startup_prompt(false, true);
        assert!(prompt.contains("built-in subagents"));
        assert!(prompt.contains("Remain the coordinator"));
        assert!(prompt.contains("independent, read-heavy delegation"));
        assert!(prompt.contains("wait for all subagents"));
        assert!(prompt.contains("Do not assign concurrent edits to overlapping files"));
        assert!(prompt.contains("non-overlapping file ownership"));

        let command = codex_shell_command("/tmp/task", false, true, false);
        assert!(command.contains("--sandbox workspace-write --ask-for-approval never"));
    }

    #[test]
    fn resumed_codex_session_uses_the_task_scoped_latest_conversation() {
        let command = codex_shell_command("/tmp/task", false, false, true);
        assert!(command.contains("-C '/tmp/task' resume --last"));
        assert!(!command.contains(AGENTS_FILE));
        assert!(!command.contains(OBJECTIVE_FILE));
    }

    #[test]
    fn worker_runtime_distinguishes_codex_from_the_fallback_shell() {
        let command = codex_shell_command("/tmp/task", false, false, false);
        assert!(command.contains("printf 'running\\n' > '/tmp/task/.dev-worker-status'"));
        assert!(command.contains(
            "dev_codex_status=$?; printf 'exited:%s\\n' \"$dev_codex_status\" > '/tmp/task/.dev-worker-status'"
        ));

        let root = temporary_directory("worker-status");
        assert_eq!(read_worker_signal(&root).unwrap(), None);
        fs::write(root.join(WORKER_STATUS_FILE), "running\n").unwrap();
        assert_eq!(
            read_worker_signal(&root).unwrap(),
            Some(WorkerSignal::Running)
        );
        fs::write(root.join(WORKER_STATUS_FILE), "exited:7\n").unwrap();
        assert_eq!(
            read_worker_signal(&root).unwrap(),
            Some(WorkerSignal::Exited(7))
        );

        assert_eq!(worker_runtime(Some(WorkerSignal::Running), true), "running");
        assert_eq!(
            worker_runtime(Some(WorkerSignal::Running), false),
            "stopped"
        );
        assert_eq!(
            worker_runtime(Some(WorkerSignal::Exited(0)), true),
            "exited (0)"
        );
        assert_eq!(
            worker_runtime(Some(WorkerSignal::Exited(7)), true),
            "failed (7)"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tmux_attachment_reports_client_counts() {
        assert_eq!(tmux_attachment(0), "detached");
        assert_eq!(tmux_attachment(1), "attached (1)");
        assert_eq!(tmux_attachment(3), "attached (3)");
    }

    #[test]
    fn attach_switches_existing_tmux_clients_without_nesting() {
        assert_eq!(tmux_attach_action(false), "attach-session");
        assert_eq!(tmux_attach_action(true), "switch-client");
    }

    #[test]
    fn semantic_task_status_takes_precedence_over_tmux_presence() {
        assert_eq!(
            task_lifecycle(Some(TaskSignal::Complete), TaskState::Running, true),
            "complete"
        );
        assert_eq!(
            task_lifecycle(Some(TaskSignal::Blocked), TaskState::Running, true),
            "blocked"
        );
        assert_eq!(
            task_lifecycle(Some(TaskSignal::Working), TaskState::Running, true),
            "running"
        );
        assert_eq!(
            task_lifecycle(Some(TaskSignal::Working), TaskState::Running, false),
            "stopped"
        );
    }
}
