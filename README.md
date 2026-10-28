# vivar

Fast, license-safe Python environment manager. One tool for Python versions,
virtual environments, dependencies, locking, and running, with **policy built in**
(package deny list, internal mirror, and a hard default of license-safe sources).

See [DESIGN.md](DESIGN.md) for the why and the roadmap, and
[python_env_managers.md](python_env_managers.md) for the survey of existing tools it draws on.

## Status

Phase 0 (MVP) is implemented and tested. Under the hood it delegates to
[`uv`](https://docs.astral.sh/uv/), so a vivar project is a plain, standards-based
Python project: `pyproject.toml` (PEP 621), `.python-version`, a normal `.venv`,
and `uv.lock`. Anyone without vivar can still use it with `uv` or `pip`.

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

The runtime lands in uv's managed-Python directory, so `vivar init` and
`vivar python list` find it right away. Packages come from one of two places:

- **Whole environments**: `vivar pack` on a connected machine, `vivar unpack` on the
  offline one. The bundle includes the interpreter and needs nothing else.
- **A wheelhouse**: a directory of wheels. Set `index-url` in `[tool.vivar.policy]` to that
  directory (absolute, or relative to the project) and vivar resolves from it alone,
  never PyPI. Fill it on a connected machine with pip, for example
  `uvx pip download -d wheelhouse numpy pandas` (add `--platform`/`--python-version`
  `--only-binary=:all:` when the target machine differs).

From source, with a Rust toolchain: `cargo install --git https://github.com/nextria-ca/vivar`.

### Platforms

| | Linux | macOS | Windows |
|---|---|---|---|
| init / add / lock / sync / run / python | ✅ | ✅ | ✅ |
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
vivar python list
```

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
