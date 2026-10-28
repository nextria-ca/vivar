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

Prerequisites: a Rust toolchain (`rustup`) and `uv` on your PATH.

```sh
curl -LsSf https://astral.sh/uv/install.sh | sh      # if you don't have uv
cargo install --path .                              # installs ~/.cargo/bin/vivar
vivar --version
```

Linux and WSL are tested. macOS should work unchanged; Windows is untested.

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

## Configuration

Everything lives in `pyproject.toml`. `vivar init` writes only an empty deny list;
settings for later phases (channels, licenses) appear when they do something.

```toml
[tool.vivar.policy]
deny = []                         # packages vivar refuses to add, lock, sync, or run with
# index-url = "https://pypi.internal.example/simple"   # use an internal mirror instead of PyPI
```

### Policy behaviour

- **`deny`**: names are matched after PEP 503 normalisation, so `Requests`,
  `requests` and `REQUESTS` are the same package. `vivar add` refuses denied
  packages up front. `vivar lock`, `sync` and `run` also check every dependency
  declared in `pyproject.toml` (`[project] dependencies`, optional dependencies,
  and dependency groups), so hand-editing the file doesn't bypass the policy.
- **`index-url`**: when set, all resolution goes through that index instead of PyPI.
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
- `tests/cli.rs`: end-to-end tests against the built binary.
