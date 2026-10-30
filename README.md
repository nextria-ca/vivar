# vivar

Fast, license-safe Python environment manager. One tool for Python versions,
virtual environments, dependencies, locking, and running, with **policy built in**
(package deny list, internal mirror, and a hard default of license-safe sources).

See [DESIGN.md](DESIGN.md) for the why and the roadmap, and
[python_env_managers.md](python_env_managers.md) for the survey of existing tools it draws on.

## Status

**Usable today, pre-1.0.** Phases 0, 3 and 4 of the [design](DESIGN.md) are implemented,
tested on Linux, macOS and Windows in CI (112 tests), and reviewed. No release has been
tagged yet; see Install for how to get it in the meantime.

What works:

- **Projects and environments**: `init`, `add`, `remove`, `lock`, `sync`, `run`, `shell`,
  tasks, PEP 723 scripts, tools, workspaces, Python install/pin/uninstall, Jupyter kernels.
- **Portable bundles**: `pack`/`unpack` with checksums, `pack --target` for another
  machine, slim and zstd variants, `verify` on the destination (imports, native libs,
  CUDA driver and a real torch GPU check), `diff` between bundles.
- **Offline**: offline installer bundles (vivar + uv + Python), wheelhouse export from the
  lockfile, org policy delivered with the installer.
- **Policy**: deny list, mirror, license allowlist, hash and wheels-only requirements,
  telemetry/offline switches, org-wide policy file, OSV audit, machine-readable reports.
- **AI/GPU**: GPU detection, PyTorch index selection, machine profiles (`cpu`, `cu124`…)
  sharing one lockfile, CUDA-aware Docker export.
- **Rollout**: online installers, MSI/deb/rpm packages, self-upgrade with verified
  downloads, shell completions, `doctor`.
- **Coming from elsewhere**: `import` for requirements.txt, conda environments, Pipfiles
  and poetry projects; `check` for CI; SBOM export; delta and signed bundles.

How it is built: everything is vivar's own code, with no runtime dependencies beyond
the OS. That includes the wheel installer, the index client, the dependency resolver,
the lockfile writer, and the Ed25519 signatures. When [`uv`](https://docs.astral.sh/uv/)
is on PATH, vivar still delegates resolution and installation to it (it is faster and
more battle-tested); without it, or with `VIVAR_BACKEND=native`, vivar does the same
work itself. Both write the same `uv.lock`. A vivar project is a plain, standards-based
Python project: `pyproject.toml` (PEP 621), `.python-version`, a normal `.venv`, and
`uv.lock`. Anyone without vivar can still use it with `uv` or `pip`.

Without uv, direct references (`pkg @ https://…/pkg.whl`, a `.tar.gz`, or
`git+https://…@rev#subdirectory=sub`) and packages that ship only a source distribution
are built by vivar's own PEP 517 driver and cached. Every form of `[tool.uv.sources]`
resolves natively: an index (per group, per extra, or under a marker, which forks the
lock), git (`rev`, `tag` or `branch`, with `subdirectory`), a URL, a local wheel or archive
(`path`), and a local project directory (built, or `editable`). Profiles
(`cpu` / `cu124` / `rocm6.2`) over dependency groups or extras, forks and workspaces
combine in one lock, and `vivar add ./dist/x.whl` or `vivar add -e ../lib` writes the
`path` source the way uv does. Nothing in the day-to-day flow needs uv, and CI runs the
whole suite on both backends. Not yet: native (conda-forge) dependencies, and
model-weight management (out of scope by decision).

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
vivar import requirements.txt   # or environment.yml, Pipfile, poetry pyproject.toml: new project or add to this one
vivar check                     # CI in one command: policy, lock freshness, hashes, licenses, audit, verify (--json)
vivar shell                     # an interactive shell with the env activated
vivar run test                  # a [tool.vivar.scripts] task; extra args are appended
vivar run tool.py               # a PEP 723 script: its inline deps get a cached env of their own
vivar tool run ruff check .     # tools in isolated envs (pipx-style); `tool install` links them into ~/.local/bin
vivar tree                      # dependency tree from the lockfile (--json for tools)
vivar why urllib3               # every chain that pulls a package in (--json)
vivar outdated                  # what a re-resolve would upgrade (--json)
vivar upgrade-deps [pkg…]       # upgrade within the declared constraints, then sync
vivar licenses                  # every installed package with its license
vivar audit                     # known vulnerabilities (OSV) for the locked packages
vivar build                     # sdist + wheel into dist/
vivar publish                   # upload dist/ to PyPI or an internal index (--repository-url, VIVAR_PUBLISH_TOKEN)
vivar export docker             # a Dockerfile that installs exactly what uv.lock pins
vivar export requirements       # pinned, hashed requirements.txt for pip-only consumers
vivar export wheelhouse --target windows-x86_64 --python 3.12   # the lock's wheels, for offline installs
vivar policy                    # the effective policy and which file each rule came from
vivar add torch                 # picks the PyTorch build for this machine's GPU (--cuda 12.4 / cpu / rocm6.2 / none)
vivar profile add cu124 torch torchvision   # a machine variant; `profile add cpu torch` another, same lockfile
vivar sync --profile auto       # install the profile that fits this machine's GPU
vivar sync --extra viz          # also install an extra of the project (profiles can be extras too)
vivar add ./dist/x.whl ../lib   # local wheels, archives and project dirs become `path` sources (-e: editable)
vivar pack --target linux-x86_64 --python 3.12   # a bundle for another machine, built here
vivar pack --zstd --use-host-cuda               # faster compression; leave CUDA libs to the target
vivar kernel install            # register .venv as a Jupyter kernel (needs ipykernel)
vivar run --env-file .env train.py
vivar python pin 3.13           # repin the project and rebuild .venv on it
vivar verify app-linux-x86_64.tar.gz        # a bundle for another machine, checked without running it
vivar diff a.tar.gz b.tar.gz    # what changed between two bundles
vivar diff --patch a.tar.gz b.tar.gz        # a delta archive; `vivar unpack --apply delta.tar.gz <unpacked-a>` upgrades in place
vivar unpack --dedupe-with ./other b.tar.gz # hard-link files identical to an already unpacked bundle
vivar key generate && vivar pack --sign     # sign bundles (Ed25519); `unpack --require-signature` or policy enforces it
vivar export sbom --format spdx # CycloneDX (default) or SPDX bill of materials from the lock
vivar doctor                    # what's installed, configured, broken
vivar env list / vivar clean    # what vivar keeps on disk; drop script envs and caches
vivar completions zsh           # shell completion (bash, zsh, fish, powershell, elvish); installers set it up
vivar man ~/.local/share/man/man1   # man pages, one per subcommand (packages and archives ship them)
vivar upgrade                   # vivar itself (`--check` only reports)
```

`audit`, `licenses`, `verify` and `policy` take `--json` for CI and compliance tooling.

### GPUs

`vivar doctor` reports the accelerator (NVIDIA driver and the CUDA version it
supports, VRAM, compute capability; ROCm; Apple Silicon). `vivar add torch` uses that
to route torch, torchvision and torchaudio to the matching PyTorch index: the newest
CUDA build the driver can run, ROCm on AMD, the CPU index on a Linux box without a GPU
(PyPI's Linux wheels drag in 2+ GB of CUDA libraries), and PyPI's own wheels on macOS
and Windows. `--cuda 12.4`, `--cuda cpu`, `--cuda rocm6.2` override it; `--cuda none`
leaves PyPI alone.

**Profiles** are machine variants of one project that share a lockfile:
`vivar profile add cu124 torch torchvision` and `vivar profile add cpu torch` create
mutually exclusive dependency groups routed to the right indexes, and
`vivar sync --profile auto` installs the one that fits the current machine. `pack`
bundles the profile `.venv` was last synced with (`--profile` overrides).

`vivar verify` goes beyond the driver check when torch is installed: it runs a matmul
on the GPU (or MPS), reports cuDNN and NCCL, and compares the driver's CUDA version
with the one the packages were built for (major mismatch is an error, minor is a
warning: NVIDIA's minor-version compatibility usually covers it).

**Cross-target bundles.** `vivar pack --target linux-x86_64` builds a bundle for a
Linux or macOS machine from any Linux or macOS host: the target's Python build is
downloaded, the lock's wheels are laid out for it, and the venv is assembled by hand.
Nothing from the target is executed, so wheels only (`require-wheels` enforces that
at add time). Windows bundles can be built from Linux or macOS too: they get vivar's
own layout (the bundled Python's venv launchers and `.cmd` shims). `--zstd` compresses multi-GB
environments several times faster (the unpacking machine needs zstd too);
`--use-host-cuda` leaves out the NVIDIA libraries from `nvidia-*` wheels when the
target provides CUDA itself.

Policy for ML boxes is cooperative, not a sandbox: `telemetry = "off"` and
`network = "offline"` set the environment variables that well-behaved libraries honour
(Hugging Face, uv, pip, and friends); a process that ignores them can still reach the
network. For a hard guarantee, run the environment on a host that has none.
`telemetry = "off"` silences Hugging Face, Gradio, Weights & Biases and friends;
`network = "offline"` puts them in offline mode; `require-wheels = true` refuses
source distributions. Private
registries take `VIVAR_INDEX_TOKEN` or `VIVAR_INDEX_USERNAME`/`VIVAR_INDEX_PASSWORD`
(or a `~/.netrc` entry). `export docker` uses an `nvidia/cuda` base image when the
lock contains CUDA packages.
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

**Deltas.** `vivar diff --patch old.tar.gz new.tar.gz` writes an archive holding only
the files that changed (plus the new manifest and checksums). On the other machine,
`vivar unpack --apply delta.tar.gz <dir>` upgrades the unpacked old bundle in place,
refuses to apply onto anything but the exact base, and re-verifies every checksum
afterwards. A torch upgrade travels as megabytes instead of gigabytes.

**Signed bundles.** `vivar key generate` creates an Ed25519 signing key (`bundle.key`,
private, or `VIVAR_BUNDLE_KEY`) and its public key (`bundle.pub`) in the config directory;
`vivar key show` prints the public key. `pack --sign` signs the checksum list and the
inventory of everything else in the bundle (launchers, activate scripts, the manifest).
Machines that unpack trust public keys from `trusted-keys = ["<hex>"]` in policy (the
org policy file is the natural place), `VIVAR_TRUSTED_KEYS`, their own `bundle.pub`, or
their own signing key. `unpack --require-signature`, or `require-signed-bundles = true`
in policy, refuses bundles that aren't signed by a trusted key. The private key never
leaves the machine that packs.

**Dedupe.** `unpack --dedupe-with <other-unpacked-bundle>` hard-links every file that is
byte-identical (per both checksum lists) instead of storing it twice. Same filesystem
only; it stops quietly otherwise.

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
on an unpacked bundle. Given a bundle archive, or a bundle built for another
platform (`vivar verify --static` forces it), it checks what the files alone can
tell: integrity against the checksum lists, the interpreter and venv layout for that
platform, every package's wheel tags and compiled extensions against it, and where
the launchers point. The import, library and GPU checks still happen on the target.
Run on this machine, it exits non-zero if anything is broken:

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
a release build under 1 MB, and no runtime beyond the OS (`curl`, `tar`, `git` for
git references) and, optionally, the `uv` binary it drives. The native installer
unpacks each wheel once into `~/.cache/vivar/unpacked/<sha256>/` and hard-links its
files into every environment (`VIVAR_LINK_MODE=copy` turns that off), so ten
environments with torch cost one torch on disk. `vivar init` creates no sample code,
README, or git repo.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests + end-to-end tests (need `uv` on PATH and network on first run)
VIVAR_BACKEND=native cargo test --test cli -- --include-ignored   # the same suite on the in-house backend (CI runs both)
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
- `src/native.rs`, `src/resolve.rs`, `src/install.rs`, `src/index.rs`, `src/pep.rs`, `src/zip.rs`: the uv-free backend.
- `src/publish.rs`: `publish`, the legacy upload API through curl.
- `src/bundle.rs`: `pack`/`unpack` portable bundles (shells out to `tar`).
- `src/verify.rs` + `src/verify.py`: `verify`, a Python driver run by the env's own interpreter.
- `tests/cli.rs`: end-to-end tests against the built binary.

## License

vivar is released under the [Apache License 2.0](LICENSE). Licenses of the Rust
crates it is built from are listed in [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).
