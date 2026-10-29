mod audit;
mod backend;
mod bundle;
mod dirs;
mod doctor;
mod envs;
mod export;
mod gpu;
mod import;
mod json;
mod kernel;
mod licenses;
mod lock;
mod project;
mod python;
mod sbom;
mod sha256;
mod sign;
mod tool;
mod upgrade;
mod venv;
mod verify;
mod wheelhouse;

use anyhow::{bail, Result};
use backend::{Backend, UvBackend};
use clap::{CommandFactory, Parser, Subcommand};
use project::Project;
use std::path::PathBuf;

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
        /// A library: adds a build backend and a src/<package>/ layout
        #[arg(long)]
        lib: bool,
    },
    /// Add dependencies to the project (checked against policy first)
    Add {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Add to a dependency group, e.g. dev
        #[arg(long)]
        group: Option<String>,
        /// PyTorch build for torch/torchvision/torchaudio: a CUDA version (12.4),
        /// `cpu`, `rocm6.2`, `auto` (match this machine's GPU; the default) or `none` (PyPI as is)
        #[arg(long)]
        cuda: Option<String>,
    },
    /// Create or extend a project from requirements.txt, a conda environment.yml,
    /// a Pipfile or a poetry pyproject.toml
    Import {
        file: PathBuf,
        /// Python version for a new project (default: what the file pins, else newest installed)
        #[arg(long)]
        python: Option<String>,
    },
    /// Everything CI wants in one go: declared deps vs policy, lock freshness,
    /// hashes, licenses, vulnerabilities and a verify run
    Check {
        /// Skip the OSV vulnerability query (offline machines)
        #[arg(long)]
        no_audit: bool,
        /// Skip the import/native-library verification
        #[arg(long)]
        no_verify: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage the shared key that authenticates bundles
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Open an interactive shell with the project environment activated
    Shell {
        /// Also load KEY=VALUE pairs from this file
        #[arg(long)]
        env_file: Option<PathBuf>,
    },
    /// Machine variants of the project (cpu, cu124, rocm6.2 …) that share one lockfile
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Register the project environment as a Jupyter kernel
    Kernel {
        #[command(subcommand)]
        command: KernelCommand,
    },
    /// Show the effective policy and where each rule comes from
    Policy {
        #[arg(long)]
        json: bool,
    },
    /// Remove dependencies from the project
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Resolve dependencies and write the lockfile
    Lock,
    /// Make the environment match the lockfile exactly
    Sync {
        /// Install this profile's packages (`auto` picks the one for this machine's GPU)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Run a command, a `[tool.vivar.scripts]` task, or a PEP 723 script in the
    /// project environment (no activation needed)
    Run {
        /// Also load KEY=VALUE pairs from this file (API keys and the like)
        #[arg(long)]
        env_file: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show which locked packages have newer versions available
    Outdated,
    /// Upgrade locked packages (all, or the ones given) within the declared constraints
    UpgradeDeps { packages: Vec<String> },
    /// Show the dependency tree from the lockfile
    Tree {
        /// Maximum depth to show
        #[arg(long, default_value = "10")]
        depth: usize,
    },
    /// Explain which dependencies pull a package in
    Why { package: String },
    /// List every installed package with its license
    Licenses {
        #[arg(long)]
        json: bool,
    },
    /// Check locked packages against the OSV vulnerability database
    Audit {
        #[arg(long)]
        json: bool,
    },
    /// Build the project's sdist and wheel into dist/
    Build,
    /// Write deployment files for the locked environment
    Export {
        #[command(subcommand)]
        command: ExportCommand,
    },
    /// Show what changed between two bundles (archives or directories)
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Write a delta archive that turns an unpacked `a` into `b` (`vivar unpack --apply`)
        #[arg(long)]
        patch: bool,
        /// Delta file (default: <b>.delta.tar.gz)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Run and install command-line tools in their own environments
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    /// Check this machine's vivar setup and the current project
    Doctor,
    /// Show environments and caches vivar manages and their sizes
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Remove script environments and the package cache
    Clean {
        /// Also remove installed tool environments
        #[arg(long)]
        tools: bool,
    },
    /// Print a shell completion script (bash, zsh, fish, powershell, elvish)
    Completions { shell: clap_complete::Shell },
    /// Archive the synced environment plus its Python for another machine
    Pack {
        /// Output file (default: <name>-<os>-<arch>.tar.gz in the current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Leave out libpython (only embedders need it; saves ~30 MB)
        #[arg(long)]
        slim: bool,
        /// Build for another machine: <os>-<arch>, e.g. linux-x86_64 or macos-aarch64
        #[arg(long)]
        target: Option<String>,
        /// Python series for a --target bundle (default: the project's pin)
        #[arg(long)]
        python: Option<String>,
        /// Compress with zstd (much faster on multi-GB environments; needs zstd on the machine that unpacks)
        #[arg(long)]
        zstd: bool,
        /// Leave out the NVIDIA libraries from nvidia-* wheels; the target machine provides CUDA
        #[arg(long)]
        use_host_cuda: bool,
        /// Profile to bundle (default: the one .venv was last synced with)
        #[arg(long)]
        profile: Option<String>,
        /// Authenticate the bundle with the bundle key (`vivar key generate`)
        #[arg(long)]
        sign: bool,
    },
    /// Extract a bundle made by `vivar pack` and make it activatable here
    Unpack {
        archive: PathBuf,
        /// Directory to unpack into (default: the archive name without .tar.gz)
        dest: Option<PathBuf>,
        /// `archive` is a delta from `vivar diff --patch`; update the unpacked bundle in `dest`
        #[arg(long)]
        apply: bool,
        /// Refuse a bundle that isn't signed with the bundle key
        #[arg(long)]
        require_signature: bool,
        /// Hard-link files identical to this already-unpacked bundle (same filesystem)
        #[arg(long)]
        dedupe_with: Option<PathBuf>,
    },
    /// Check every installed package imports and its native libraries, CUDA
    /// driver, and scripts work on this machine
    Verify {
        /// Project, bundle, or .venv directory (default: the current project)
        path: Option<PathBuf>,
        /// Report a missing GPU/driver as a warning instead of an error
        #[arg(long)]
        allow_missing_gpu: bool,
        /// Also list the packages that passed
        #[arg(short, long)]
        verbose: bool,
        /// Machine-readable report
        #[arg(long)]
        json: bool,
    },
    /// Replace this binary with the latest release (or a given one)
    Upgrade {
        /// Release tag to install, e.g. v0.2.0 (default: latest)
        #[arg(long)]
        version: Option<String>,
        /// Install from a release archive, extracted offline bundle, or URL instead
        #[arg(long)]
        from: Option<String>,
        /// Only report whether a newer release exists
        #[arg(long)]
        check: bool,
    },
    /// Manage Python installations
    Python {
        #[command(subcommand)]
        command: PythonCommand,
    },
}

#[derive(Subcommand)]
enum ExportCommand {
    /// A Dockerfile that installs exactly what uv.lock pins
    Docker {
        /// Output file (default: Dockerfile in the project root; `-` for stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// A software bill of materials from the lockfile
    Sbom {
        /// cyclonedx (default) or spdx
        #[arg(long, default_value = "cyclonedx")]
        format: String,
        /// Output file (default: sbom.<format>.json in the project root; `-` for stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// A pinned, hashed requirements.txt for pip-only consumers
    Requirements {
        /// Output file (default: requirements.txt in the project root)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// The lockfile's exact wheels for a target machine, for offline installs
    Wheelhouse {
        /// Directory to fill (default: ./wheelhouse)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Target machine as <os>-<arch>, e.g. linux-x86_64, windows-x86_64, macos-aarch64 (default: this one)
        #[arg(long)]
        target: Option<String>,
        /// Target Python series, e.g. 3.12 (default: the project's pin)
        #[arg(long)]
        python: Option<String>,
    },
}

#[derive(Subcommand)]
enum ToolCommand {
    /// Run a tool, installing it into its own environment first if needed
    Run {
        /// Package providing the command, when it differs from the command name
        #[arg(long)]
        from: Option<String>,
        /// Python version for the tool's environment
        #[arg(long)]
        python: Option<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install a tool and link its commands into ~/.local/bin
    Install {
        package: String,
        #[arg(long)]
        python: Option<String>,
    },
    /// Remove a tool and its linked commands
    Uninstall { package: String },
    /// List installed tools
    List,
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Create a profile (cpu, cu124, rocm6.2 …) holding these PyTorch-family packages
    Add {
        name: String,
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// List the project's profiles and which one fits this machine
    List,
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Create the bundle key (32 random bytes) in the config directory
    Generate {
        #[arg(long)]
        force: bool,
    },
    /// Print where the key is read from and whether one is configured
    Show,
}

#[derive(Subcommand)]
enum KernelCommand {
    /// Register .venv as a Jupyter kernel (needs ipykernel in the env)
    Install {
        /// Kernel name (default: the project name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a kernel registered by vivar
    Uninstall { name: String },
    /// List Jupyter kernels
    List,
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List environments, Python builds and caches with sizes
    List,
}

#[derive(Subcommand)]
enum PythonCommand {
    /// Install a Python version
    Install { version: String },
    /// Remove an installed Python version
    Uninstall { version: String },
    /// Pin the project to a Python version and rebuild .venv on it
    Pin { version: String },
    /// List installed Python versions (`--all` adds the available downloads)
    List {
        #[arg(long)]
        all: bool,
    },
}

fn main() -> Result<()> {
    upgrade::cleanup();
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;

    // Commands always execute from the current directory so relative paths in
    // `vivar run` keep working; the project is only discovered for policy.
    let project = Project::find(&cwd)?;
    // Outside a project the org policy still applies (tools, scripts).
    let policy = match &project {
        Some(p) => p.policy()?,
        None => project::org_policy()?.map(|(_, p)| p).unwrap_or_default(),
    };
    let backend = UvBackend::new(&policy);

    match cli.command {
        Command::Init {
            name,
            python,
            no_sync,
            lib,
        } => {
            if cwd.join(project::PYPROJECT).exists() {
                bail!(
                    "{} already exists; this is already a project",
                    cwd.display()
                );
            }
            let interpreter = venv::interpreter(python.as_deref())?;
            let name = name.unwrap_or_else(|| project::default_name(&cwd));
            project::init_files(&cwd, &name, interpreter.version, lib)?;
            println!(
                "Initialized project `{name}` on CPython {}",
                interpreter.version_string()
            );
            if !no_sync {
                venv::ensure(&cwd, &name)?;
                if backend::uv_available() {
                    backend.lock(&cwd)?;
                    venv::mark_synced(&cwd);
                } else {
                    println!("note: uv is not installed; uv.lock will be written on the first `vivar add`");
                }
            }
        }
        Command::Add {
            packages,
            group,
            cuda,
        } => {
            let project = require(project, &cwd)?;
            project.check_requirements(&packages)?;
            venv::ensure(project.venv_root(), &project.name())?;
            transact(&project, &backend, &cwd, group.as_deref(), || {
                route_pytorch(&project, &packages, cuda.as_deref())?;
                backend.add(&cwd, &packages, group.as_deref())?;
                venv::mark_synced(project.venv_root());
                after_install(&project, &policy)
            })?;
        }
        Command::Remove { packages } => {
            let project = require(project, &cwd)?;
            venv::ensure(project.venv_root(), &project.name())?;
            backend.remove(&cwd, &packages)?;
            venv::mark_synced(project.venv_root());
        }
        Command::Import { file, python } => {
            let imported = import::read(&file)?;
            let project = match project {
                Some(p) if p.has_project_table() => p,
                Some(p) => {
                    // A poetry-only pyproject: give it PEP 621 metadata, keep the rest.
                    let name = imported.name.clone().unwrap_or_else(|| p.name());
                    let python = python.clone().or(imported.python.clone());
                    project::ensure_pep621(&p.root, &name, python.as_deref())?;
                    if !p.root.join(".python-version").exists() {
                        let interpreter = venv::interpreter(python.as_deref())?;
                        let [major, minor, _] = interpreter.version;
                        std::fs::write(
                            p.root.join(".python-version"),
                            format!("{major}.{minor}\n"),
                        )?;
                    }
                    project::write_defaults(&p.root)?;
                    println!(
                        "added [project] metadata to {}",
                        p.root.join(project::PYPROJECT).display()
                    );
                    Project::require(&p.root)?
                }
                None => {
                    let python = python.or(imported.python.clone());
                    let interpreter = venv::interpreter(python.as_deref())?;
                    let name = imported
                        .name
                        .clone()
                        .unwrap_or_else(|| project::default_name(&cwd));
                    project::init_files(&cwd, &name, interpreter.version, false)?;
                    println!(
                        "Initialized project `{name}` on CPython {}",
                        interpreter.version_string()
                    );
                    Project::require(&cwd)?
                }
            };
            let policy = project.policy()?;
            let backend = UvBackend::new(&policy);
            venv::ensure(project.venv_root(), &project.name())?;
            let all: Vec<String> = imported
                .requirements
                .iter()
                .map(|(_, r)| r.clone())
                .collect();
            project.check_requirements(&all)?;
            let mut groups: Vec<Option<String>> = Vec::new();
            for (g, _) in &imported.requirements {
                if !groups.contains(g) {
                    groups.push(g.clone());
                }
            }
            // One transaction for the whole import: a policy rejection undoes all of it.
            transact(&project, &backend, &project.root, None, || {
                for group in &groups {
                    let reqs: Vec<String> = imported
                        .requirements
                        .iter()
                        .filter(|(g, _)| g == group)
                        .map(|(_, r)| r.clone())
                        .collect();
                    // A conda `pytorch-cuda` pin routes the PyTorch family only.
                    let family: Vec<String> = reqs
                        .iter()
                        .filter(|r| {
                            PYTORCH_FAMILY.contains(
                                &project::normalize(project::requirement_name(r)).as_str(),
                            )
                        })
                        .cloned()
                        .collect();
                    if !family.is_empty() {
                        route_pytorch(&project, &family, imported.cuda.as_deref())?;
                    }
                    let list = std::env::temp_dir()
                        .join(format!("vivar-import-{}.txt", std::process::id()));
                    std::fs::write(&list, reqs.join("\n") + "\n")?;
                    let added = backend.add_requirements(&project.root, &list, group.as_deref());
                    let _ = std::fs::remove_file(&list);
                    added?;
                    println!(
                        "added {} requirement(s){}",
                        reqs.len(),
                        group
                            .as_deref()
                            .map(|g| format!(" to group {g}"))
                            .unwrap_or_default()
                    );
                }
                venv::mark_synced(project.venv_root());
                after_install(&project, &policy)
            })?;
            for s in &imported.skipped {
                println!("skipped: {s}");
            }
            if !imported.skipped.is_empty() {
                println!("(conda-only packages and local sources need a hand; see README)");
            }
        }
        Command::Check {
            no_audit,
            no_verify,
            json,
        } => {
            let project = require(project, &cwd)?;
            std::process::exit(run_check(
                &project, &policy, &backend, no_audit, no_verify, json,
            )?);
        }
        Command::Key { command } => match command {
            KeyCommand::Generate { force } => {
                let path = sign::generate(force)?;
                println!(
                    "wrote bundle key to {} (copy it to the machines that unpack)",
                    path.display()
                );
            }
            KeyCommand::Show => {
                let configured = sign::key()?.is_some();
                println!(
                    "bundle key: {} (VIVAR_BUNDLE_KEY or {})",
                    if configured { "configured" } else { "none" },
                    sign::key_path()?.display()
                );
            }
        },
        Command::Shell { env_file } => {
            let project = require(project, &cwd)?;
            venv::warn_if_stale(project.venv_root());
            let extra = runtime_env(&policy, env_file.as_deref())?;
            std::process::exit(venv::shell(project.venv_root(), &project.name(), &extra)?);
        }
        Command::Profile { command } => match command {
            ProfileCommand::Add { name, packages } => {
                let project = require(project, &cwd)?;
                project.check_requirements(&packages)?;
                venv::ensure(project.venv_root(), &project.name())?;
                transact(&project, &backend, &cwd, Some(&name), || {
                    project::declare_profile(&project.root, &name, &packages)?;
                    // uv adds to the group without installing it; a new profile is
                    // installed and made the active one, so policy sees the packages.
                    backend.add(&cwd, &packages, Some(&name))?;
                    backend.sync_group(&cwd, Some(&name))?;
                    venv::mark_profile(project.venv_root(), Some(&name));
                    venv::mark_synced(project.venv_root());
                    after_install(&project, &policy)
                })?;
                println!(
                    "profile `{name}` installed and active; `vivar sync --profile <name>` or `--profile auto` switches"
                );
            }
            ProfileCommand::List => {
                let project = require(project, &cwd)?;
                let profiles = project.profiles();
                if profiles.is_empty() {
                    println!(
                        "no profiles; `vivar profile add cu124 torch torchvision` creates one"
                    );
                }
                let gpu = gpu::detect();
                let pick = gpu::profile_for(&gpu, &profiles);
                for p in &profiles {
                    println!(
                        "{p}{}",
                        if Some(p) == pick {
                            "  (fits this machine)"
                        } else {
                            ""
                        }
                    );
                }
                println!("this machine: {}", gpu::describe(&gpu));
            }
        },
        Command::Kernel { command } => match command {
            KernelCommand::Install { name } => {
                let project = require(project, &cwd)?;
                kernel::install(project.venv_root(), &project.name(), name.as_deref())?;
            }
            KernelCommand::Uninstall { name } => kernel::uninstall(&name)?,
            KernelCommand::List => kernel::list()?,
        },
        Command::Policy { json } => print_policy(project.as_ref(), &policy, json)?,
        Command::Lock => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            backend.lock(&cwd)?;
            check_hashes(&project, &policy)?;
        }
        Command::Sync { profile } => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            venv::ensure(project.venv_root(), &project.name())?;
            let group = match profile.as_deref() {
                None => None,
                Some("auto") => {
                    let profiles = project.profiles();
                    let gpu = gpu::detect();
                    let pick = gpu::profile_for(&gpu, &profiles).cloned();
                    match &pick {
                        Some(p) => println!("profile `{p}` for {}", gpu::describe(&gpu)),
                        None => println!(
                            "no profile fits {}; syncing the default groups",
                            gpu::describe(&gpu)
                        ),
                    }
                    pick
                }
                Some(name) => {
                    if !project.profiles().iter().any(|p| p == name) {
                        bail!("no profile named {name}; `vivar profile list` shows them");
                    }
                    Some(name.to_owned())
                }
            };
            backend.sync_group(&cwd, group.as_deref())?;
            venv::mark_profile(project.venv_root(), group.as_deref());
            venv::mark_synced(project.venv_root());
            after_install(&project, &policy)?;
        }
        Command::Run { env_file, args } => {
            let first = args[0].as_str();
            let extra = runtime_env(&policy, env_file.as_deref())?;
            // A PEP 723 script carries its own environment.
            let script = std::path::Path::new(first);
            if first.ends_with(".py")
                && script.is_file()
                && tool::script_metadata(script)?.is_some()
            {
                std::process::exit(tool::run_script(
                    &backend,
                    &policy,
                    script,
                    &args[1..],
                    &extra,
                )?);
            }
            let project = require(project, &cwd)?;
            project.check_declared()?;
            venv::warn_if_stale(project.venv_root());
            let args = match project.script(first) {
                Some(task) => {
                    let mut full = split_command(&task);
                    full.extend_from_slice(&args[1..]);
                    full
                }
                None => args,
            };
            std::process::exit(venv::run(project.venv_root(), &args, &extra)?);
        }
        Command::Outdated => {
            require(project, &cwd)?;
            backend.upgrade(&cwd, &[], true)?;
        }
        Command::UpgradeDeps { packages } => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            backend.upgrade(&cwd, &packages, false)?;
            check_hashes(&project, &policy)?;
            backend.sync(&cwd)?;
            venv::mark_synced(project.venv_root());
            after_install(&project, &policy)?;
        }
        Command::Tree { depth } => {
            let project = require(project, &cwd)?;
            print!("{}", lock::Lock::read(project.venv_root())?.tree(depth));
        }
        Command::Why { package } => {
            let project = require(project, &cwd)?;
            let name = project::normalize(&package);
            let lock = lock::Lock::read(project.venv_root())?;
            let paths = lock.why(&name);
            if paths.is_empty() {
                bail!("{name} is not in the lockfile");
            }
            for path in paths {
                let chain: Vec<String> = path
                    .iter()
                    .map(|p| format!("{} v{}", p.name, p.version))
                    .collect();
                println!("{}", chain.join(" -> "));
            }
        }
        Command::Licenses { json } => {
            let project = require(project, &cwd)?;
            let venv = project.venv_root().join(".venv");
            let dists = licenses::inventory(&venv)?;
            if json {
                let items: Vec<String> = dists
                    .iter()
                    .map(|d| {
                        format!(
                            "{{\"name\":{},\"version\":{},\"license\":{}}}",
                            json::quote(&d.name),
                            json::quote(&d.version),
                            json::quote(&d.license)
                        )
                    })
                    .collect();
                println!("[{}]", items.join(","));
                return Ok(());
            }
            for d in dists {
                println!(
                    "{:<32} {:<14} {}",
                    d.name,
                    d.version,
                    if d.license.is_empty() {
                        "(none declared)"
                    } else {
                        &d.license
                    }
                );
            }
        }
        Command::Audit { json } => {
            let project = require(project, &cwd)?;
            let findings = audit::run(project.venv_root())?;
            if json {
                let items: Vec<String> = findings
                    .iter()
                    .map(|f| {
                        let ids: Vec<String> = f.ids.iter().map(|i| json::quote(i)).collect();
                        format!(
                            "{{\"name\":{},\"version\":{},\"vulnerabilities\":[{}]}}",
                            json::quote(&f.name),
                            json::quote(&f.version),
                            ids.join(",")
                        )
                    })
                    .collect();
                println!("[{}]", items.join(","));
                if !findings.is_empty() {
                    std::process::exit(1);
                }
                return Ok(());
            }
            if findings.is_empty() {
                println!("no known vulnerabilities in the locked packages");
            } else {
                for f in &findings {
                    println!("{} {}: {}", f.name, f.version, f.ids.join(", "));
                }
                bail!("{} vulnerable package(s)", findings.len());
            }
        }
        Command::Build => {
            require(project, &cwd)?;
            backend.build(&cwd)?;
        }
        Command::Export { command } => match command {
            ExportCommand::Sbom { format, output } => {
                let project = require(project, &cwd)?;
                let fmt = match format.to_lowercase().as_str() {
                    "cyclonedx" | "cdx" => sbom::Format::CycloneDx,
                    "spdx" => sbom::Format::Spdx,
                    other => bail!("unknown SBOM format {other}; use cyclonedx or spdx"),
                };
                let text = sbom::generate(&project, fmt)?;
                match output.as_deref().and_then(|p| p.to_str()) {
                    Some("-") => print!("{text}"),
                    _ => {
                        let path = output.unwrap_or_else(|| {
                            project
                                .root
                                .join(format!("sbom.{}.json", format.to_lowercase()))
                        });
                        std::fs::write(&path, text)?;
                        println!("wrote {}", path.display());
                    }
                }
            }
            ExportCommand::Requirements { output } => {
                let project = require(project, &cwd)?;
                let path = output.unwrap_or_else(|| project.root.join("requirements.txt"));
                backend.export_requirements(&cwd, &path)?;
                println!("wrote {}", path.display());
            }
            ExportCommand::Wheelhouse {
                output,
                target,
                python,
            } => {
                let project = require(project, &cwd)?;
                let python = python
                    .or_else(|| {
                        std::fs::read_to_string(project.venv_root().join(".python-version"))
                            .ok()
                            .map(|s| s.trim().to_owned())
                    })
                    .unwrap_or_else(|| "3.12".to_owned());
                let target = wheelhouse::Target::new(target.as_deref(), &python)?;
                let out = output.unwrap_or_else(|| cwd.join("wheelhouse"));
                let summary = wheelhouse::export(project.venv_root(), &target, &out)?;
                println!("{summary}");
                println!(
                    "on the target machine set [tool.vivar.policy] index-url = {:?}",
                    out.display().to_string()
                );
            }
            ExportCommand::Docker { output } => {
                let project = require(project, &cwd)?;
                let text = export::dockerfile(&project, &policy)?;
                match output.as_deref().and_then(|p| p.to_str()) {
                    Some("-") => print!("{text}"),
                    _ => {
                        let path = output.unwrap_or_else(|| project.root.join("Dockerfile"));
                        std::fs::write(&path, text)?;
                        println!("wrote {}", path.display());
                    }
                }
            }
        },
        Command::Diff {
            a,
            b,
            patch,
            output,
        } => {
            if patch {
                let out = output.unwrap_or_else(|| {
                    let name = b.file_name().unwrap_or_default().to_string_lossy();
                    PathBuf::from(format!(
                        "{}.delta.tar.gz",
                        name.trim_end_matches(".tar.gz")
                            .trim_end_matches(".tar.zst")
                    ))
                });
                println!("{}", bundle::make_delta(&a, &b, &out)?);
                return Ok(());
            }
            let out = bundle::diff(&a, &b)?;
            if out.is_empty() {
                println!("no differences");
            } else {
                print!("{out}");
            }
        }
        Command::Tool { command } => match command {
            ToolCommand::Run { from, python, args } => {
                let code = tool::run(
                    &backend,
                    &policy,
                    from.as_deref(),
                    &args[0],
                    &args[1..],
                    python.as_deref(),
                )?;
                std::process::exit(code);
            }
            ToolCommand::Install { package, python } => {
                tool::install(&backend, &policy, &package, python.as_deref())?
            }
            ToolCommand::Uninstall { package } => tool::uninstall(&package)?,
            ToolCommand::List => tool::list()?,
        },
        Command::Doctor => std::process::exit(doctor::run(&cwd)?),
        Command::Env { command } => match command {
            EnvCommand::List => envs::list(&cwd)?,
        },
        Command::Clean { tools } => envs::clean(tools)?,
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "vivar", &mut std::io::stdout());
        }
        Command::Pack {
            output,
            slim,
            target,
            python,
            zstd,
            use_host_cuda,
            profile,
            sign,
        } => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            let profile = profile.or_else(|| venv::current_profile(project.venv_root()));
            if let Some(p) = &profile {
                if !project.profiles().iter().any(|x| x == p) {
                    bail!("no profile named {p}; `vivar profile list` shows them");
                }
            }
            let opts = bundle::PackOptions {
                slim,
                zstd,
                host_cuda: use_host_cuda,
                sign,
            };
            let archive = match target {
                Some(target) => {
                    let python = python
                        .or_else(|| {
                            std::fs::read_to_string(project.venv_root().join(".python-version"))
                                .ok()
                                .map(|s| s.trim().to_owned())
                        })
                        .unwrap_or_else(|| "3.12".to_owned());
                    let t = wheelhouse::Target::new(Some(&target), &python)?;
                    check_hashes(&project, &policy)?;
                    bundle::pack_for_target(
                        &backend,
                        &policy,
                        project.venv_root(),
                        &project.name(),
                        output.as_deref(),
                        t.os(),
                        t.arch(),
                        &python,
                        profile.as_deref(),
                        opts,
                    )?
                }
                None => {
                    venv::ensure(project.venv_root(), &project.name())?;
                    backend.sync_group(&cwd, profile.as_deref())?;
                    venv::mark_synced(project.venv_root());
                    venv::mark_profile(project.venv_root(), profile.as_deref());
                    after_install(&project, &policy)?;
                    bundle::pack(
                        project.venv_root(),
                        &project.name(),
                        output.as_deref(),
                        opts,
                    )?
                }
            };
            let size = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            println!(
                "Packed {} ({:.1} MB). Unpack elsewhere with `vivar unpack {}`",
                archive.display(),
                size as f64 / 1e6,
                archive.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        Command::Unpack {
            archive,
            dest,
            apply,
            require_signature,
            dedupe_with,
        } => {
            let require_signature = require_signature || policy.require_signed_bundles;
            if apply {
                let dir = dest.ok_or_else(|| {
                    anyhow::anyhow!("--apply needs the unpacked bundle directory")
                })?;
                bundle::apply_delta(&archive, &dir, require_signature)?;
                return Ok(());
            }
            let dest = dest.unwrap_or_else(|| {
                let stem = archive.file_name().unwrap_or_default().to_string_lossy();
                PathBuf::from(
                    stem.trim_end_matches(".tar.gz")
                        .trim_end_matches(".tgz")
                        .trim_end_matches(".tar.zst"),
                )
            });
            let dest = bundle::unpack(&archive, &dest, require_signature, dedupe_with.as_deref())?;
            let bin = dest.join(".venv").join(venv::BIN);
            if cfg!(windows) {
                println!(
                    "Unpacked to {}\n  activate:  {}\n  or run:    {}",
                    dest.display(),
                    bin.join("activate").display(),
                    bin.join("python.exe").display()
                );
            } else {
                println!(
                    "Unpacked to {}\n  activate:  source {}\n  or run:    {}",
                    dest.display(),
                    bin.join("activate").display(),
                    bin.join("python").display()
                );
            }
        }
        Command::Verify {
            path,
            allow_missing_gpu,
            verbose,
            json,
        } => {
            let base = match path {
                Some(p) => p,
                None => project
                    .as_ref()
                    .map(|p| p.venv_root().to_path_buf())
                    .unwrap_or(cwd),
            };
            let venv = verify::find_venv(&base)?;
            let (mut code, mut report) = verify::run(&venv, allow_missing_gpu, verbose, json)?;
            // An unpacked bundle also carries checksums: report anything changed since packing.
            let bundle_dir = venv.parent().unwrap_or(&venv);
            if bundle_dir.join(bundle::CHECKSUMS).is_file() {
                let bad = bundle::verify_checksums(bundle_dir)?;
                if json {
                    // Fold the integrity result into the report's single JSON object.
                    let items: Vec<String> = bad.iter().map(|b| json::quote(b)).collect();
                    let body = report
                        .trim_end()
                        .strip_suffix('}')
                        .unwrap_or("{")
                        .to_owned();
                    report = format!(
                        "{body},\n  \"integrity\": {{\"changed\": [{}]}}\n}}\n",
                        items.join(",")
                    );
                } else if bad.is_empty() {
                    println!("integrity: all bundled files match their checksums");
                } else {
                    println!(
                        "integrity: {} file(s) changed since the bundle was packed:",
                        bad.len()
                    );
                    for b in bad.iter().take(20) {
                        println!("  {b}");
                    }
                }
                if !bad.is_empty() {
                    code = 1;
                }
            }
            print!("{report}");
            std::process::exit(code);
        }
        Command::Upgrade {
            version,
            from,
            check,
        } => {
            if check {
                upgrade::check()?
            } else {
                upgrade::run(version.as_deref(), from.as_deref())?
            }
        }
        Command::Python { command } => match command {
            PythonCommand::Install { version } => {
                python::install(&version)?;
            }
            PythonCommand::Uninstall { version } => python::uninstall(&version)?,
            PythonCommand::Pin { version } => {
                let project = require(project, &cwd)?;
                project.check_declared()?;
                let interpreter = venv::interpreter(Some(&version))?;
                // The pin lives where the env lives (the workspace root for members).
                let root = project.venv_root().to_path_buf();
                project::pin_python(&root, &project.root, interpreter.version)?;
                println!(
                    "pinned {} to CPython {}",
                    project.name(),
                    interpreter.version_string()
                );
                let venv_dir = root.join(".venv");
                let current =
                    std::fs::read_to_string(venv_dir.join("pyvenv.cfg")).unwrap_or_default();
                let [major, minor, _] = interpreter.version;
                if venv_dir.exists()
                    && !current.contains(&format!("version_info = {major}.{minor}."))
                {
                    // The env is regenerable; rebuild it on the new interpreter.
                    std::fs::remove_dir_all(&venv_dir)?;
                    venv::ensure(&root, &project.name())?;
                    if backend::uv_available() && root.join(lock::LOCKFILE).exists() {
                        backend.lock(&cwd)?;
                        backend.sync(&cwd)?;
                        venv::mark_synced(&root);
                        after_install(&project, &policy)?;
                    }
                }
            }
            PythonCommand::List { all } => python::list(all)?,
        },
    }
    Ok(())
}

/// `vivar check`: every gate in one pass. Returns the exit code.
fn run_check(
    project: &Project,
    policy: &project::Policy,
    backend: &UvBackend,
    no_audit: bool,
    no_verify: bool,
    json: bool,
) -> Result<i32> {
    let mut results: Vec<(&'static str, &'static str, String)> = Vec::new();
    record(
        &mut results,
        "policy",
        project
            .check_declared()
            .map(|()| "declared dependencies pass the deny list".to_owned()),
    );
    record(
        &mut results,
        "lock",
        backend
            .lock_check(&project.root)
            .map(|()| "uv.lock is present and up to date".to_owned()),
    );
    record(
        &mut results,
        "hashes",
        check_hashes(project, policy).map(|()| {
            if policy.require_hashes {
                "all artifacts hashed and from the index".to_owned()
            } else {
                "not required by policy".to_owned()
            }
        }),
    );
    let venv_dir = project.venv_root().join(".venv");
    let has_env = venv_dir.join("pyvenv.cfg").is_file();
    record(
        &mut results,
        "env",
        if has_env {
            Ok("present".to_owned())
        } else {
            Err(anyhow::anyhow!("no .venv; run `vivar sync`"))
        },
    );
    record(
        &mut results,
        "licenses",
        licenses::check(
            &venv_dir,
            policy.allow_licenses.as_deref(),
            &policy.license_exceptions,
        )
        .map(|()| {
            if policy.allow_licenses.is_some() {
                "all within the allowlist".to_owned()
            } else {
                "no allowlist set".to_owned()
            }
        }),
    );
    if no_audit {
        results.push(("audit", "skipped", "--no-audit".to_owned()));
    } else {
        record(
            &mut results,
            "audit",
            audit::run(project.venv_root()).and_then(|f| {
                if f.is_empty() {
                    Ok("no known vulnerabilities".to_owned())
                } else {
                    let list: Vec<String> = f
                        .iter()
                        .map(|x| format!("{} {} ({})", x.name, x.version, x.ids.join(", ")))
                        .collect();
                    Err(anyhow::anyhow!(
                        "{} vulnerable package(s): {}",
                        f.len(),
                        list.join("; ")
                    ))
                }
            }),
        );
    }
    if no_verify {
        results.push(("verify", "skipped", "--no-verify".to_owned()));
    } else if has_env {
        // An unpacked bundle also gets its checksums re-verified, as `vivar verify` does.
        let integrity = if project.venv_root().join(bundle::CHECKSUMS).is_file() {
            bundle::verify_checksums(project.venv_root()).unwrap_or_else(|e| vec![e.to_string()])
        } else {
            Vec::new()
        };
        record(
            &mut results,
            "verify",
            verify::run(&venv_dir, false, false, true).and_then(|(code, _)| {
                if !integrity.is_empty() {
                    Err(anyhow::anyhow!(
                        "{} bundled file(s) changed since packing",
                        integrity.len()
                    ))
                } else if code == 0 {
                    Ok("every package imports and loads".to_owned())
                } else {
                    Err(anyhow::anyhow!(
                        "verify found problems; run `vivar verify` for details"
                    ))
                }
            }),
        );
    } else {
        results.push(("verify", "skipped", "no environment".to_owned()));
    }
    let failed = results.iter().filter(|(_, s, _)| *s == "fail").count();
    if json {
        let items: Vec<String> = results
            .iter()
            .map(|(n, st, d)| {
                format!(
                    "{{\"check\":{},\"status\":{},\"detail\":{}}}",
                    json::quote(n),
                    json::quote(st),
                    json::quote(d)
                )
            })
            .collect();
        println!(
            "{{\"ok\":{},\"checks\":[{}]}}",
            failed == 0,
            items.join(",")
        );
    } else {
        for (n, st, d) in &results {
            let tag = match *st {
                "ok" => "ok  ",
                "fail" => "FAIL",
                _ => "skip",
            };
            println!("{tag} {n:<9} {d}");
        }
        println!(
            "{}",
            if failed == 0 {
                "all checks passed"
            } else {
                "checks failed"
            }
        );
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

/// Runs `op` (an add or a profile add) so that a failure, including a policy
/// rejection after install, leaves the project files and the environment as
/// they were: pyproject.toml and uv.lock are restored, and the env is re-synced
/// only if something actually changed in it.
fn record(
    results: &mut Vec<(&'static str, &'static str, String)>,
    name: &'static str,
    r: Result<String>,
) {
    match r {
        Ok(detail) => results.push((name, "ok", detail)),
        Err(e) => results.push((name, "fail", e.to_string())),
    }
}

fn transact(
    project: &Project,
    backend: &UvBackend,
    cwd: &std::path::Path,
    group: Option<&str>,
    op: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let venv_dir = project.venv_root().join(".venv");
    let files = [
        project.root.join(project::PYPROJECT),
        project.venv_root().join(lock::LOCKFILE),
    ];
    let snapshot: Vec<Option<Vec<u8>>> = files.iter().map(|p| std::fs::read(p).ok()).collect();
    let env_before = venv::fingerprint(&venv_dir);
    let profile_before = venv::current_profile(project.venv_root());
    let Err(e) = op() else {
        return Ok(());
    };
    println!("rolling back");
    let lock_changed = std::fs::read(&files[1]).ok() != snapshot[1];
    for (path, contents) in files.iter().zip(snapshot) {
        match contents {
            Some(bytes) => std::fs::write(path, bytes)?,
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if lock_changed || venv::fingerprint(&venv_dir) != env_before {
        // Back to what was installed before: the previously active profile (it
        // belongs to the project that owns the env, the workspace root for a
        // member) plus the plain group being added to. Never a second profile:
        // profiles exclude each other and the sync would be refused.
        let member = Project::load(&project.root).ok();
        let owner = Project::load(project.venv_root()).ok();
        let profiles: Vec<String> = owner
            .iter()
            .chain(member.iter())
            .flat_map(|p| p.profiles())
            .collect();
        let exists = |g: &str| {
            owner.as_ref().is_some_and(|p| p.has_group(g))
                || member.as_ref().is_some_and(|p| p.has_group(g))
        };
        let profile = profile_before.as_deref().filter(|p| exists(p));
        let mut groups: Vec<&str> = profile.into_iter().collect();
        if let Some(g) =
            group.filter(|g| exists(g) && !groups.contains(g) && !profiles.iter().any(|p| p == g))
        {
            groups.push(g);
        }
        let _ = backend.sync_groups(cwd, &groups);
        venv::mark_profile(project.venv_root(), profile);
        venv::mark_synced(project.venv_root());
    }
    Err(e)
}

/// Environment for `run`/`shell`: the policy's variables, then the env file's.
fn runtime_env(
    policy: &project::Policy,
    env_file: Option<&std::path::Path>,
) -> Result<Vec<(String, String)>> {
    let mut vars: Vec<(String, String)> = policy
        .runtime_env()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if let Some(path) = env_file {
        vars.extend(venv::read_env_file(path)?);
    }
    Ok(vars)
}

/// Packages that come from PyTorch's own indexes rather than PyPI.
const PYTORCH_FAMILY: &[&str] = &["torch", "torchvision", "torchaudio"];

/// `vivar add torch`: pick the PyTorch build for this machine's GPU unless told
/// otherwise (`--cuda 12.4`, `cpu`, `rocm6.2`, or `none` to leave PyPI alone).
fn route_pytorch(project: &Project, packages: &[String], cuda: Option<&str>) -> Result<()> {
    let family: Vec<String> = packages
        .iter()
        .filter(|p| {
            PYTORCH_FAMILY.contains(&project::normalize(project::requirement_name(p)).as_str())
        })
        .cloned()
        .collect();
    let (variant, targets) = match cuda {
        Some("none") => (None, Vec::new()),
        Some("auto") | None => {
            if family.is_empty() {
                return Ok(());
            }
            let gpu = gpu::detect();
            let variant = gpu::pytorch_variant(&gpu);
            match &variant {
                Some(v) => println!("{}: using PyTorch's {v} wheels", gpu::describe(&gpu)),
                None => println!(
                    "{}: using PyPI's default PyTorch wheels",
                    gpu::describe(&gpu)
                ),
            }
            (variant, family)
        }
        Some(explicit) => (Some(explicit.to_owned()), packages.to_vec()),
    };
    if let (Some(variant), false) = (variant, targets.is_empty()) {
        project::route_to_pytorch_index(&project.root, &targets, &variant, None)?;
    }
    Ok(())
}

/// `vivar policy`: the merged policy, then each layer that contributed.
fn print_policy(project: Option<&Project>, policy: &project::Policy, json: bool) -> Result<()> {
    let org = project::org_policy()?;
    if json {
        let list = |v: &[String]| {
            let items: Vec<String> = v.iter().map(|s| json::quote(s)).collect();
            format!("[{}]", items.join(","))
        };
        let path_json = |p: &std::path::Path| json::quote(&p.display().to_string());
        println!(
            "{{\"deny\":{},\"index-url\":{},\"allow-licenses\":{},\"license-exceptions\":{},\"require-hashes\":{},\"require-wheels\":{},\"telemetry\":{},\"network\":{},\"sources\":{{\"project\":{},\"workspace\":{},\"org\":{}}}}}",
            list(&policy.deny),
            policy.index_url.as_deref().map_or("null".to_owned(), json::quote),
            policy.allow_licenses.as_deref().map_or("null".to_owned(), list),
            list(&policy.license_exceptions),
            policy.require_hashes,
            policy.require_wheels,
            if policy.telemetry_off { "\"off\"" } else { "\"on\"" },
            if policy.offline { "\"offline\"" } else { "\"online\"" },
            project.map_or("null".to_owned(), |p| path_json(&p.root.join(project::PYPROJECT))),
            project
                .and_then(|p| p.workspace.as_deref())
                .map_or("null".to_owned(), |w| path_json(&w.root.join(project::PYPROJECT))),
            org.as_ref().map_or("null".to_owned(), |(p, _)| path_json(p)),
        );
        return Ok(());
    }
    println!("effective policy");
    println!(
        "  deny:               {}",
        if policy.deny.is_empty() {
            "(none)".to_owned()
        } else {
            policy.deny.join(", ")
        }
    );
    println!(
        "  index-url:          {}",
        policy.index_url.as_deref().unwrap_or("PyPI")
    );
    println!(
        "  allow-licenses:     {}",
        match &policy.allow_licenses {
            None => "(any)".to_owned(),
            Some(l) if l.is_empty() =>
                "(nothing: the policies have no license in common)".to_owned(),
            Some(l) => l.join(", "),
        }
    );
    if !policy.license_exceptions.is_empty() {
        println!(
            "  license-exceptions: {}",
            policy.license_exceptions.join(", ")
        );
    }
    println!("  require-hashes:     {}", policy.require_hashes);
    println!("  require-wheels:     {}", policy.require_wheels);
    println!(
        "  telemetry:          {}",
        if policy.telemetry_off { "off" } else { "on" }
    );
    println!(
        "  network:            {}",
        if policy.offline { "offline" } else { "online" }
    );
    println!("from");
    match project {
        Some(p) => {
            println!("  project:   {}", p.root.join(project::PYPROJECT).display());
            if let Some(w) = &p.workspace {
                println!("  workspace: {}", w.root.join(project::PYPROJECT).display());
            }
        }
        None => println!("  project:   (not inside a project)"),
    }
    match org {
        Some((path, _)) => println!("  org:       {}", path.display()),
        None => println!(
            "  org:       (no policy file; VIVAR_POLICY or {})",
            dirs::config()?.join("policy.toml").display()
        ),
    }
    Ok(())
}

/// Policy checks that need the environment installed: licenses, and the
/// lockfile's hashes/sources when `require-hashes` is on.
fn after_install(project: &Project, policy: &project::Policy) -> Result<()> {
    check_hashes(project, policy)?;
    licenses::check(
        &project.venv_root().join(".venv"),
        policy.allow_licenses.as_deref(),
        &policy.license_exceptions,
    )
}

fn check_hashes(project: &Project, policy: &project::Policy) -> Result<()> {
    if !policy.require_hashes {
        return Ok(());
    }
    let lock = lock::Lock::read(project.venv_root())?;
    let tree = std::fs::canonicalize(project.venv_root()).unwrap_or_default();
    let mut bad = Vec::new();
    for p in &lock.packages {
        // The project and its workspace members are the code being locked, not
        // dependencies; an editable checkout from elsewhere is a dependency.
        let own = p.source_kind == "virtual"
            || (p.source_kind == "editable"
                && std::fs::canonicalize(project.venv_root().join(&p.source))
                    .map(|path| path.starts_with(&tree))
                    .unwrap_or(false));
        if own {
            continue;
        }
        if p.source_kind != "registry" {
            bad.push(format!(
                "{} {} comes from a {} source",
                p.name, p.version, p.source_kind
            ));
        } else if !p.hashed {
            bad.push(format!("{} {} has unhashed artifacts", p.name, p.version));
        } else if let Some(index) = policy.index_url.as_deref().filter(|i| i.contains("://")) {
            if p.source.trim_end_matches('/') != index.trim_end_matches('/') {
                bad.push(format!(
                    "{} {} comes from {} instead of {index}",
                    p.name, p.version, p.source
                ));
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        bail!("require-hashes policy violated:\n  {}", bad.join("\n  "))
    }
}

/// Splits a task string into arguments, honouring single and double quotes.
fn split_command(task: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut pending = false;
    for c in task.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                pending = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || pending {
                    out.push(std::mem::take(&mut cur));
                    pending = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || pending {
        out.push(cur);
    }
    out
}

fn require(project: Option<Project>, cwd: &std::path::Path) -> Result<Project> {
    match project {
        Some(p) => Ok(p),
        None => Project::require(cwd),
    }
}

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn splits_tasks_like_a_shell() {
        assert_eq!(
            split_command("pytest -x tests/"),
            vec!["pytest", "-x", "tests/"]
        );
        assert_eq!(
            split_command("python -c 'print(\"hi there\")' \"\" done"),
            vec!["python", "-c", "print(\"hi there\")", "", "done"]
        );
        assert!(split_command("   ").is_empty());
    }
}
