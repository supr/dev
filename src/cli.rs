use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "dev", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create and manage isolated task workspaces.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Create a task workspace and its repository worktrees.
    New {
        /// Task name. Use letters, numbers, hyphens, and underscores.
        name: String,

        /// High-level outcome for Codex. Prompts interactively when omitted.
        #[arg(short, long)]
        objective: Option<String>,

        /// Comma-separated repository names. Defaults to every repository.
        #[arg(short, long, value_delimiter = ',')]
        repositories: Vec<String>,
    },

    /// Start the task's tmux session and Codex worker.
    Start {
        name: String,

        /// Inspect and plan without modifying files or activating the Goal.
        #[arg(long)]
        plan: bool,

        /// Let Codex coordinate bounded work through its built-in subagents.
        #[arg(long)]
        multi_agent: bool,

        /// Start a new Codex conversation instead of resuming this task's latest one.
        #[arg(long)]
        new_session: bool,

        /// Attach after starting the session.
        #[arg(long)]
        attach: bool,
    },

    /// List task workspaces.
    List,

    /// Show a task's lifecycle and repository state.
    Status { name: String },

    /// Attach to a running task session.
    Attach { name: String },

    /// Stop a running task session without deleting its workspace.
    Stop { name: String },

    /// Remove a task's worktrees and metadata.
    Destroy {
        name: String,

        /// Skip confirmation and remove worktrees even when they are dirty.
        #[arg(long)]
        force: bool,
    },
}
