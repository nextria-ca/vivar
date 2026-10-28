mod backend;
mod project;

use anyhow::Result;
use backend::{Backend, UvBackend};
use clap::{Parser, Subcommand};
use project::Project;

#[derive(Parser)]
#[command(
    name = "vivar",
    version,
    about = "Fast, license-safe Python environment manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new project in the current directory
    Init {
        /// Project name (defaults to the directory name)
        name: Option<String>,
        /// Python version to use, e.g. 3.12
        #[arg(long)]
        python: Option<String>,
        /// Only write project files; don't create .venv or the lockfile yet
        #[arg(long)]
        no_sync: bool,
    },
    /// Add dependencies to the project (checked against policy first)
    Add {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Add to a dependency group, e.g. dev
        #[arg(long)]
        group: Option<String>,
    },
    /// Remove dependencies from the project
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Resolve dependencies and write the lockfile
    Lock,
    /// Make the environment match the lockfile exactly
    Sync,
    /// Run a command in the project environment (no activation needed)
    Run {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage Python installations
    Python {
        #[command(subcommand)]
        command: PythonCommand,
    },
}

#[derive(Subcommand)]
enum PythonCommand {
    /// Install a Python version
    Install { version: String },
    /// List installed and available Python versions
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;

    // Commands always execute from the current directory so relative paths in
    // `vivar run` keep working; the project is only discovered for policy.
    let project = Project::find(&cwd)?;
    let policy = project.as_ref().map(Project::policy).unwrap_or_default();
    let backend = UvBackend::new(&policy);

    match cli.command {
        Command::Init {
            name,
            python,
            no_sync,
        } => {
            backend.init(&cwd, name.as_deref(), python.as_deref())?;
            project::write_defaults(&cwd)?;
            if !no_sync {
                // Reload: init may have run in a directory with no project before.
                let project = Project::require(&cwd)?;
                let backend = UvBackend::new(&project.policy());
                backend.sync(&cwd)?;
            }
        }
        Command::Add { packages, group } => {
            let project = require(project, &cwd)?;
            project.check_requirements(&packages)?;
            backend.add(&cwd, &packages, group.as_deref())?;
        }
        Command::Remove { packages } => {
            require(project, &cwd)?;
            backend.remove(&cwd, &packages)?;
        }
        Command::Lock => {
            require(project, &cwd)?.check_declared()?;
            backend.lock(&cwd)?;
        }
        Command::Sync => {
            require(project, &cwd)?.check_declared()?;
            backend.sync(&cwd)?;
        }
        Command::Run { args } => {
            // `run` also works outside a project (uv falls back to a plain
            // interpreter), so the policy check only applies when there is one.
            if let Some(project) = &project {
                project.check_declared()?;
            }
            std::process::exit(backend.run(&cwd, &args)?);
        }
        Command::Python { command } => match command {
            PythonCommand::Install { version } => backend.python_install(&version)?,
            PythonCommand::List => backend.python_list()?,
        },
    }
    Ok(())
}

fn require(project: Option<Project>, cwd: &std::path::Path) -> Result<Project> {
    match project {
        Some(p) => Ok(p),
        None => Project::require(cwd),
    }
}
