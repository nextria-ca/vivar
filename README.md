# vivar

Fast, license-safe Python environment manager. One tool for Python versions,
virtual environments, dependencies, locking, and running, with **policy built in**
(package deny list, internal mirror, and a hard default of license-safe sources).

See [DESIGN.md](DESIGN.md) for the why and the roadmap, and
[python_env_managers.md](python_env_managers.md) for the survey of existing tools it draws on.

## Status

Phase 0 (MVP) is implemented and tested. Projects, virtual environments, Python
builds, running, bundles, verification and upgrades are vivar's own code; only
dependency resolution and installation (`add`, `remove`, `lock`, `sync`) are
delegated to [`uv`](https://docs.astral.sh/uv/) for now. A vivar project is a
plain, standards-based Python project: `pyproject.toml` (PEP 621),
`.python-version`, a normal `.venv`, and `uv.lock`. Anyone without vivar can
still use it with `uv` or `pip`.

Later phases swap `uv` out for an in-process engine (and add conda-forge native
dependencies) behind the same CLI. See the roadmap in DESIGN.md.

## Install

Prebuilt binaries for Linux (x86_64, arm64), macOS (Apple Silicon, Intel) and
Windows (x86_64, arm64) are attached to each [GitHub release](https://github.com/nextria-ca/vivar/releases).
The installers also install `uv`, which vivar drives under the hood, if it's missing.

```sh
# Linux / macOS
curl -LsSf https://raw.githubusercontent.com/nextria-ca/vivar/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/nextria-ca/vivar/main/install.ps1 | iex
```

While the repository is private, GitHub won't serve those URLs anonymously. Log in
with the `gh` CLI once and use it to fetch the installer; it then downloads the
release through your account:

```sh
gh api repos/nextria-ca/vivar/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

```powershell
gh api repos/nextria-ca/vivar/contents/install.ps1 -H "Accept: application/vnd.github.raw" | iex
```

Both installers take `VIVAR_VERSION`, `VIVAR_INSTALL_DIR`, `VIVAR_ARCHIVE`
(install from a local file or internal URL instead of GitHub) and `VIVAR_SKIP_UV`.

### Upgrading

```sh
vivar upgrade                      # latest release, through gh or curl + GITHUB_TOKEN
vivar upgrade --version v0.2.0
vivar upgrade --from vivar-offline-x86_64-unknown-linux-musl   # extracted bundle, archive, or URL
```

It swaps the running binary in place and leaves everything else alone.

### Offline install

Each release also carries `vivar-offline-<target>.tar.gz` / `.zip` bundles with
everything a machine without internet needs: vivar, uv and a CPython 3.12 runtime.
Copy one over (USB stick, file share), extract it, and run the installer inside it.
It touches nothing on the network:

```sh
tar -xzf vivar-offline-x86_64-unknown-linux-musl.tar.gz
sh vivar-offline-x86_64-unknown-linux-musl/install.sh
```

```powershell
Expand-Archive vivar-offline-x86_64-pc-windows-msvc.zip -DestinationPath .
powershell -ExecutionPolicy Bypass -File vivar-offline-x86_64-pc-windows-msvc\install.ps1
```

The bundle carries Python 3.12 and 3.13 (the list is one line in the release
workflow). The runtimes land in uv's managed-Python directory, so `vivar init` and
`vivar python list` find them right away. Drop a `policy.toml` next to the installer
before distributing the bundle and every machine gets the org policy too
(`VIVAR_POLICY_URL` does the same for the online installer). Packages come from one
of two places:

- **Whole environments**: `vivar pack` on a connected machine, `vivar unpack` on the
  offline one. The bundle includes the interpreter and needs nothing else.
- **A wheelhouse**: a directory of wheels. On a connected machine with the project,
  `vivar export wheelhouse --target linux-x86_64 --python 3.12` downloads the exact
  wheels `uv.lock` pins for that machine and checks each against the lock's hash. Copy
  the folder over, set `index-url` in `[tool.vivar.policy]` to it (absolute, or relative
  to the project), and vivar resolves from it alone, never PyPI.

From source, with a Rust toolchain: `cargo install --git https://github.com/nextria-ca/vivar`.

### IT rollout

Each release also carries a per-machine **MSI** (`vivar-<version>-<target>.msi`, adds
`C:\Program Files\vivar` to the system PATH; suitable for Intune or Group Policy) and
**.deb** / **.rpm** packages that install `/usr/bin/vivar`. uv and Python runtimes are
still fetched on first use, or come from an offline bundle.

### Proxies and private CAs

Downloads go through `curl` and `uv`; both honour `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY`.
For a corporate CA set `SSL_CERT_FILE=/path/to/ca-bundle.pem` once: curl, uv and
Python all read it (`VIVAR_CA_BUNDLE` overrides it for vivar's own downloads only).
`vivar doctor` shows what is in effect.

Release archives are checked against the release's `SHA256SUMS` by `vivar upgrade`,
and Python runtimes against python-build-standalone's published checksums.

### Platforms

| | Linux | macOS | Windows |
|---|---|---|---|
| init / add / lock / sync / run / python | ✅ | ✅ | ✅ |
| activate scripts | sh/bash/zsh, PowerShell | sh/bash/zsh, PowerShell | cmd, PowerShell |
| pack / unpack | ✅ | ✅ | ✅ (`tar.exe` ships with Windows 10+) |
| verify | ✅ full | imports, CUDA, scripts (no `ldd` pass) | imports, CUDA (`nvcuda.dll`), scripts |

CI runs the full test suite on all three. A bundle only unpacks on the OS and
CPU architecture it was packed on.

## Quick start

```sh
mkdir my-app && cd my-app
vivar init --python 3.12        # pyproject.toml, .python-version, .venv, uv.lock; nothing else
vivar add numpy "pandas>=2"     # checked against policy, then added + installed
vivar add --group dev pytest    # dependency groups
vivar run python app.py         # runs in the project env; no activation
vivar run pytest
vivar remove pandas
vivar lock                      # re-resolve and write the lockfile
vivar sync                      # make .venv match the lockfile exactly
vivar python install 3.13       # prebuilt standalone Python, no compiling
vivar python list               # --all adds what can be downloaded
```

More, all from the same binary:

```sh
vivar shell                     # an interactive shell with the env activated
vivar run test                  # a [tool.vivar.scripts] task; extra args are appended
vivar run tool.py               # a PEP 723 script: its inline deps get a cached env of their own
vivar tool run ruff check .     # tools in isolated envs (pipx-style); `tool install` links them into ~/.local/bin
vivar tree                      # dependency tree from the lockfile
vivar why urllib3               # every chain that pulls a package in
vivar outdated                  # what a re-resolve would upgrade
vivar upgrade-deps [pkg…]       # upgrade within the declared constraints, then sync
vivar licenses                  # every installed package with its license
vivar audit                     # known vulnerabilities (OSV) for the locked packages
vivar build                     # sdist + wheel into dist/
vivar export docker             # a Dockerfile that installs exactly what uv.lock pins
vivar export requirements       # pinned, hashed requirements.txt for pip-only consumers
vivar export wheelhouse --target windows-x86_64 --python 3.12   # the lock's wheels, for offline installs
vivar policy                    # the effective policy and which file each rule came from
vivar add torch --cuda 12.4     # route packages to the PyTorch index for that CUDA version
vivar python pin 3.13           # repin the project and rebuild .venv on it
vivar diff a.tar.gz b.tar.gz    # what changed between two bundles
vivar doctor                    # what's installed, configured, broken
vivar env list / vivar clean    # what vivar keeps on disk; drop script envs and caches
vivar completions zsh           # shell completion (bash, zsh, fish, powershell, elvish)
vivar upgrade                   # vivar itself (`--check` only reports)
```

`audit`, `licenses`, `verify` and `policy` take `--json` for CI and compliance tooling.
`vivar run` prints a one-line note when `uv.lock` changed since the last sync.
Activate scripts exist for sh/bash/zsh, fish, csh/tcsh, PowerShell and cmd.

Tasks live in `pyproject.toml`:

```toml
[tool.vivar.scripts]
test = "pytest -x"
lint = "ruff check ."
```

Workspaces (`[tool.uv.workspace]`) work as in uv: members share the root `.venv`
and inherit the root's policy.

Commands can be run from any subdirectory of the project. `vivar run` keeps
relative paths relative to where you ran it.

## Moving an environment to another machine

A venv is not portable on its own: it hardcodes absolute paths and depends on a
Python install outside itself. `vivar pack` fixes both.

```sh
vivar pack                                   # on machine A -> my-app-linux-x86_64.tar.gz (~35 MB)
scp my-app-linux-x86_64.tar.gz machine-b:
vivar unpack my-app-linux-x86_64.tar.gz      # on machine B -> ./my-app-linux-x86_64/
source my-app-linux-x86_64/.venv/bin/activate
```

Every bundle carries a SHA-256 list of its files. `unpack` refuses an archive whose
contents don't match it, and `vivar verify` on an unpacked bundle reports any file
changed since packing. `vivar diff` compares two bundles' Python, platform and packages.

The archive holds the synced `.venv`, `pyproject.toml`, `uv.lock`,
`.python-version`, and the exact standalone Python the venv uses, minus
everything a frozen environment doesn't need: bytecode and tool caches, uv's venv
marker files, and the runtime's own test suite, pip/ensurepip, IDLE and Tk demos. `unpack` extracts it and rewrites
the paths in `pyvenv.cfg`, the interpreter symlinks, console-script shebangs,
and the activate scripts to the new location. On Windows the `.exe` launchers
embed the interpreter path in a resource; `unpack` patches it to a relative
path, which makes them location-independent from then on. Machine B needs neither `uv` nor
network, only `tar` and the same OS and CPU architecture (`unpack` refuses a
mismatch). It uses `tar.gz` rather than `.zip` because zip drops the symlinks
and executable bits a venv relies on.

## Verifying an environment works on this machine

```sh
vivar verify            # current project
vivar verify ./my-app   # an unpacked bundle, or a .venv directly
```

`verify` runs inside the environment's own Python, so it needs no `uv` and works
on an unpacked bundle. It exits non-zero if anything is broken:

- **Imports**: every top-level module of every installed package is imported in
  an isolated child process. A wheel built against a library the host lacks
  fails here with the loader's message, e.g. `libcudart.so.12: cannot open shared object file`.
  Crashes and hangs are contained and reported per package.
- **Native libraries** (Linux): every extension module in site-packages is run
  through `ldd`; a needed library that exists neither on the system nor inside
  the environment is an error.
- **CUDA**: if the environment contains CUDA packages (`nvidia-*` or anything
  linked against the CUDA runtime), the host must have a working NVIDIA driver
  and at least one GPU. Missing driver is an error; `--allow-missing-gpu`
  downgrades it to a warning for CPU-only CI.
- **Scripts**: console scripts whose shebang points at an interpreter that no
  longer exists (a venv copied by hand instead of `vivar unpack`).

`--verbose` also lists what passed. `VIVAR_CUDA_LIB` overrides the driver
library name if yours isn't `libcuda.so.1`.

## Configuration

Everything lives in `pyproject.toml`. `vivar init` writes only an empty deny list;
settings for later phases (channels, licenses) appear when they do something.

```toml
[tool.vivar.policy]
deny = []                         # packages vivar refuses to add, lock, sync, or run with
# index-url = "https://pypi.internal.example/simple"   # internal mirror instead of PyPI, or a wheelhouse directory
```

### Policy behaviour

- **`deny`**: names are matched after PEP 503 normalisation, so `Requests`,
  `requests` and `REQUESTS` are the same package. `vivar add` refuses denied
  packages up front. `vivar lock`, `sync` and `run` also check every dependency
  declared in `pyproject.toml` (`[project] dependencies`, optional dependencies,
  and dependency groups), so hand-editing the file doesn't bypass the policy.
- **`index-url`**: when set, all resolution goes through that index instead of PyPI. A
  local directory of wheels works too and is then the only source (see Offline install).
- **`allow-licenses`**: SPDX identifiers (e.g. `["MIT", "Apache-2.0", "BSD-3-Clause"]`).
  After every install, each package's declared license (`License-Expression`, else its
  classifiers) must satisfy the list; `OR` needs one side allowed, `AND` needs both. A
  package that fails, or declares nothing, fails the command, and `vivar add` rolls the
  addition back. `license-exceptions = ["name"]` waives it per package.
- **`require-hashes = true`**: every locked package must be a hashed artifact from a
  registry, and from `index-url` when one is set. Git, URL and path sources are refused.

An **org-wide policy** applies on top of every project: `VIVAR_POLICY=/path/policy.toml`,
else `~/.config/vivar/policy.toml` (`%APPDATA%\vivar\policy.toml` on Windows), else
`/etc/vivar/policy.toml`. Same keys under a `[policy]` table. Deny lists add up,
`require-hashes` sticks, license allowlists intersect, and a project may still point
`index-url` at a wheelhouse. Ship it inside the offline bundle or by configuration
management; `vivar doctor` shows which file is in effect.
- Policy is read from the nearest `pyproject.toml` above the current directory.

Existing `[tool.vivar]` sections are never overwritten by `vivar init`.

## Footprint

Three direct dependencies (`clap` with minimal features, `toml_edit`, `anyhow`),
a release build under 1 MB, and no runtime beyond the `uv` binary it drives.
`vivar init` creates no sample code, README, or git repo.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests + end-to-end tests (need `uv` on PATH and network on first run)
```

Regenerate the license inventory after changing dependencies:

```sh
cargo metadata --format-version 1 > /tmp/meta.json
python3 scripts/licenses.py /tmp/meta.json > THIRD_PARTY_LICENSES.md
```

Layout:

- `src/main.rs`: CLI (clap) and command wiring.
- `src/project.rs`: project discovery, `[tool.vivar]` defaults, policy parsing and checks.
- `src/backend.rs`: the `Backend` trait and the phase 0 `UvBackend`.
- `src/bundle.rs`: `pack`/`unpack` portable bundles (shells out to `tar`).
- `src/verify.rs` + `src/verify.py`: `verify`, a Python driver run by the env's own interpreter.
- `tests/cli.rs`: end-to-end tests against the built binary.
