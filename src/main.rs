mod audit;
mod backend;
mod bundle;
mod dirs;
mod doctor;
mod envs;
mod export;
mod json;
mod licenses;
mod lock;
mod project;
mod python;
mod sha256;
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
        /// Take these packages from the PyTorch index for this CUDA version, e.g. 12.4
        #[arg(long)]
        cuda: Option<String>,
    },
    /// Open an interactive shell with the project environment activated
    Shell,
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
    Sync,
    /// Run a command, a `[tool.vivar.scripts]` task, or a PEP 723 script in the
    /// project environment (no activation needed)
    Run {
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
    Diff { a: PathBuf, b: PathBuf },
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
    },
    /// Extract a bundle made by `vivar pack` and make it activatable here
    Unpack {
        archive: PathBuf,
        /// Directory to unpack into (default: the archive name without .tar.gz)
        dest: Option<PathBuf>,
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
            // Snapshot what `add` rewrites, so a policy failure can be undone exactly.
            let snapshot: Vec<(std::path::PathBuf, Option<Vec<u8>>)> = [
                project.root.join(project::PYPROJECT),
                project.venv_root().join(lock::LOCKFILE),
            ]
            .into_iter()
            .map(|p| (p.clone(), std::fs::read(&p).ok()))
            .collect();
            if let Some(cuda) = &cuda {
                project::route_to_cuda_index(&project.root, &packages, cuda)?;
            }
            let env_before = venv::fingerprint(&project.venv_root().join(".venv"));
            // A failed add (bad index, unresolvable spec) is rolled back like a policy failure.
            let added = backend
                .add(&cwd, &packages, group.as_deref())
                .and_then(|()| {
                    venv::mark_synced(project.venv_root());
                    after_install(&project, &policy)
                });
            if let Err(e) = added {
                println!("rolling back the add");
                // Resync only if the env actually changed: uv may restore the lock
                // itself after a half-finished install, so the lock alone can't tell.
                let lock_path = project.venv_root().join(lock::LOCKFILE);
                let lock_changed = snapshot
                    .iter()
                    .find(|(p, _)| *p == lock_path)
                    .is_some_and(|(_, before)| std::fs::read(&lock_path).ok() != *before);
                let env_touched = lock_changed
                    || venv::fingerprint(&project.venv_root().join(".venv")) != env_before;
                for (path, contents) in snapshot {
                    match contents {
                        Some(bytes) => std::fs::write(&path, bytes)?,
                        None => {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                // Keep the group that was being added to installed, as before the
                // add; a group the add itself created no longer exists to sync.
                if env_touched {
                    let group = group
                        .as_deref()
                        .filter(|g| Project::load(&project.root).is_ok_and(|p| p.has_group(g)));
                    let _ = backend.sync_group(&cwd, group);
                    venv::mark_synced(project.venv_root());
                }
                return Err(e);
            }
        }
        Command::Remove { packages } => {
            let project = require(project, &cwd)?;
            venv::ensure(project.venv_root(), &project.name())?;
            backend.remove(&cwd, &packages)?;
            venv::mark_synced(project.venv_root());
        }
        Command::Shell => {
            let project = require(project, &cwd)?;
            venv::warn_if_stale(project.venv_root());
            std::process::exit(venv::shell(project.venv_root(), &project.name())?);
        }
        Command::Policy { json } => print_policy(project.as_ref(), &policy, json)?,
        Command::Lock => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            backend.lock(&cwd)?;
            check_hashes(&project, &policy)?;
        }
        Command::Sync => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            venv::ensure(project.venv_root(), &project.name())?;
            backend.sync(&cwd)?;
            venv::mark_synced(project.venv_root());
            after_install(&project, &policy)?;
        }
        Command::Run { args } => {
            let first = args[0].as_str();
            // A PEP 723 script carries its own environment.
            let script = std::path::Path::new(first);
            if first.ends_with(".py")
                && script.is_file()
                && tool::script_metadata(script)?.is_some()
            {
                std::process::exit(tool::run_script(&backend, &policy, script, &args[1..])?);
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
            std::process::exit(venv::run(project.venv_root(), &args)?);
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
        Command::Diff { a, b } => {
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
        Command::Pack { output, slim } => {
            let project = require(project, &cwd)?;
            project.check_declared()?;
            venv::ensure(project.venv_root(), &project.name())?;
            backend.sync(&cwd)?;
            venv::mark_synced(project.venv_root());
            after_install(&project, &policy)?;
            let archive = bundle::pack(
                project.venv_root(),
                &project.name(),
                output.as_deref(),
                slim,
            )?;
            let size = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            println!(
                "Packed {} ({:.1} MB). Unpack elsewhere with `vivar unpack {}`",
                archive.display(),
                size as f64 / 1e6,
                archive.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        Command::Unpack { archive, dest } => {
            let dest = dest.unwrap_or_else(|| {
                let stem = archive.file_name().unwrap_or_default().to_string_lossy();
                PathBuf::from(stem.trim_end_matches(".tar.gz").trim_end_matches(".tgz"))
            });
            let dest = bundle::unpack(&archive, &dest)?;
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
            "{{\"deny\":{},\"index-url\":{},\"allow-licenses\":{},\"license-exceptions\":{},\"require-hashes\":{},\"sources\":{{\"project\":{},\"workspace\":{},\"org\":{}}}}}",
            list(&policy.deny),
            policy.index_url.as_deref().map_or("null".to_owned(), json::quote),
            policy.allow_licenses.as_deref().map_or("null".to_owned(), list),
            list(&policy.license_exceptions),
            policy.require_hashes,
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
