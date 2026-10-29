# vivar — Design Doc

> Status: phase 0 implemented, 2026-10-28. See [README.md](README.md) for usage.

## 1. Why

Existing tools each solve part of the problem (see [python_env_managers.md](python_env_managers.md)):

- **Licensing risk**: Anaconda's `defaults` channel and Docker Desktop need paid licenses at our size, and it's easy to hit them by accident.
- **Not ours**: we don't control the roadmap, defaults, or behavior of uv/conda/Poetry, and uv is VC-backed.
- **No single tool does it all**: uv is fast but PyPI-only. conda handles native deps but is slow, bloated, and breaks when mixed with pip.

`vivar` is **our own** environment manager. It keeps what each tool does well, fixes their known pain points, and adds the thing none of them do: **built-in policy** (licenses, approved packages, mirrors).

## 2. Goals

1. **One tool** for Python versions, virtual environments, dependencies, locking, and running.
2. **Fast**: Rust, global cache with hardlinks, parallel downloads.
3. **Reproducible**: one cross-platform lockfile covering PyPI and conda-forge packages.
4. **Native deps without conda's pain**: CUDA, GDAL, compilers via conda-forge, solved *together* with PyPI deps.
5. **License-safe by default**: conda-forge + PyPI only. `defaults` is blocked unless explicitly allowed.
6. **Standards-first**: PEP 621 `pyproject.toml`, a normal `.venv`, PEP 723 inline scripts. No lock-in: a project still works with plain pip/uv.
7. **Policy built in**: license allowlist, package deny list, vulnerability audit, internal/offline mirror.
8. **Zero shell magic**: no rc-file edits, no auto-activated `base`. `vivar run` just works.
9. **Portable**: a synced environment can be packed, copied to another machine of the same OS/arch, unpacked, and activated with no network and no uv.

## 3. Non-goals (for now)

- Replacing Docker for full OS isolation (we *export* to Docker instead).
- Building our own package index or rebuilding packages.
- Supporting non-conda-forge conda channels.
- A GUI.

## 4. Keep / Fix matrix

| From | Keep ✅ | Fix ❌ |
|---|---|---|
| venv | Standard `.venv` layout, IDE compatibility | Can't manage Python or native libs |
| pip | Direct PyPI access, newest releases | No real lockfile, inconsistent incremental installs |
| conda | Prebuilt native binaries, Python management, conda-forge ecosystem | Slow solver, disk bloat, pip mixing, channel conflicts, licensing, shell init |
| Poetry | Dependency groups, build/publish | Slow, used a non-standard format, `^` over-pinning |
| pyenv | Per-folder `.python-version` | Compiles from source |
| uv | Speed, `run`, workspaces, PEP 723, tool runner, global cache | PyPI-only, not ours |
| pixi | Unified conda + PyPI lock, tasks | Small community, not ours |
| Docker/Nix | Reproducibility | Weight / learning curve → offer `vivar export docker` |

## 5. User experience

```sh
vivar init                    # new project: pyproject.toml, .venv, .python-version, company defaults
vivar init --no-sync          # project files only, no .venv yet
vivar add numpy "pandas>=2"   # add PyPI deps (checked against policy first)
vivar add --native cuda-toolkit gdal   # add conda-forge deps (v2)
vivar add --group dev pytest  # dependency groups
vivar remove pandas
vivar lock                    # write vivar.lock
vivar sync                    # make .venv match the lock exactly
vivar run python app.py       # sync if needed, then run (no activation)
vivar pack                    # .venv + its Python -> <name>-<os>-<arch>.tar.gz
vivar unpack app.tar.gz dir   # extract on another machine, rewrite paths, activate
vivar verify                  # every installed package imports; native libs, CUDA driver, scripts OK here
vivar python install 3.12     # prebuilt standalone Python
vivar python list
vivar tool run ruff check     # throwaway tool envs (later)
vivar audit                   # license + vulnerability report (v3)
vivar export docker           # Dockerfile from the lock (v3)
```

## 6. Configuration

Everything lives in a standard `pyproject.toml`. vivar-specific settings go under `[tool.vivar]`:

```toml
[project]
name = "my-app"
requires-python = ">=3.12"
dependencies = ["numpy", "pandas>=2"]

[dependency-groups]
dev = ["pytest"]

[tool.vivar.policy]
deny = ["some-banned-package"]      # refused by `vivar add`/`lock`/`sync`/`run`
index-url = "https://pypi.internal.example/simple"        # mirror (phase 0)

[tool.vivar]                        # phase 2, not written until then
channels = ["conda-forge"]          # native dep sources
blocked-channels = ["defaults"]     # licensing guardrail
native-dependencies = { cuda-toolkit = "12.*", gdal = "*" }
allowed-licenses = ["MIT", "BSD-3-Clause", "Apache-2.0"]  # v3
```

`vivar init` only writes keys that are enforced today. No placeholder config.

An org-wide policy file (from a git repo or URL) can be layered on top of project settings (v3).

## 7. Architecture

```
┌──────────────────────────── vivar CLI (clap) ───────────────────────────┐
│ init · add · remove · lock · sync · run · python · audit · export       │
└─────────────┬───────────────────────────────────────────────────────────┘
              │
     ┌────────▼────────┐    ┌──────────────────┐
     │ Project/config  │    │ Policy engine    │  ← our differentiator
     │ (pyproject.toml)│    │ deny/licenses/   │
     └────────┬────────┘    │ mirrors/audit    │
              │             └────────┬─────────┘
     ┌────────▼──────────────────────▼─────────┐
     │          Backend trait                  │
     └───────┬───────────────────────┬─────────┘
   phase 0   │                       │ phase 1+
 ┌───────────▼─────────┐   ┌─────────▼──────────────────────────┐
 │ UvBackend           │   │ NativeBackend (in-process)         │
 │ shells out to `uv`  │   │  • pubgrub resolver (MPL-2.0)      │
 │ (MIT/Apache)        │   │  • rattler: conda-forge (BSD-3)    │
 └─────────────────────┘   │  • python-build-standalone         │
                           │  • unified lock + global cache     │
                           └────────────────────────────────────┘
```

**Key decision:** all commands go through a `Backend` trait.

- **Phase 0** ships quickly by delegating to the `uv` binary. We own the CLI, config, and policy from day one.
- **Phase 1+** replaces it piece by piece with in-process, permissively licensed Rust libraries. Users see no change.

We never write our own SAT/PubGrub resolver or wheel/conda package parsers. Those are solved problems, and getting them wrong is how homemade tools fail.

## 8. Lockfile (`vivar.lock`)

- TOML, human-diffable, deterministic ordering.
- Covers **all target platforms** (linux-64, osx-arm64, win-64...) in one file.
- Each package records its source (`pypi` | `conda-forge`), version, URL, hashes, and license.
- Records the solved native packages first, then PyPI packages solved *against* them. This prevents pip clobbering conda packages.
- Phase 0 uses `uv.lock` underneath. `vivar.lock` arrives with the native backend.

## 9. Environment layout

- Project env: `./.venv` (a standard venv layout, so IDEs/pip keep working). With native deps, it's a conda-style prefix that still has the venv structure.
- Global cache: `~/.cache/vivar` (packages hardlinked into envs).
- Pythons: `~/.local/share/vivar/python/<version>`.
- No shell rc edits. An optional `vivar shell` spawns an activated subshell.

## 10. Licensing

- Dependencies must be permissively licensed (MIT, Apache-2.0, BSD, MPL-2.0 file-level) and recorded in `THIRD_PARTY_LICENSES`.
- `repo.anaconda.com` is hard-blocked unless a user explicitly sets `allow-anaconda-defaults = true`.
- vivar's own license: **TBD** (proprietary internal, or MIT/Apache-2.0 if we open-source it).

## 11. Roadmap

| Phase | Scope | Exit criteria |
|---|---|---|
| **0: MVP** ✅ | CLI + `[tool.vivar]` defaults + deny policy, `UvBackend` for init/add/remove/lock/sync/run/python | Team can use it daily for pure-Python projects |
| **1: Own the core** | In-process resolver + installer, `vivar.lock`, global cache (venvs, Python downloads, run ✅) | No `uv` binary needed, speed on par with uv |
| **2: Native deps** | rattler integration, `--native`, unified conda-forge + PyPI solve | GPU/GDAL project fully reproducible from the lock |
| **3: Policy** ✅ | License allowlist, vulnerability audit (OSV), mirrors/offline, org policy file, `export docker`, `require-hashes` | `vivar audit` runs in CI and passes |
| **4: Polish** ✅ | Workspaces, PEP 723 scripts, `tool run`, tasks, installers, `tree`/`why`, `outdated`, `doctor`, completions, bundle integrity + `diff` | — |

## 12. Implementation status

Phase 0 is done (`src/`, 14 tests, CI in `.github/workflows/ci.yml`):

- `init`, `run`, `python install|list` and `.venv` creation are vivar's own: projects are
  written with `toml_edit`, venvs are built by hand on uv-compatible managed CPython builds
  (python-build-standalone, downloaded via the release's `SHA256SUMS` list), `run` execs
  with the env first on PATH. Only system Pythons are never used, so every env is relocatable.
- `add [--group]`, `remove`, `lock`, `sync` via `UvBackend`; `init` falls back gracefully
  when uv is absent (no lockfile until the first `add`).
- `init` creates exactly `pyproject.toml`, `.python-version`, `.venv`, `uv.lock` (`--no-sync` skips the last two). No sample code, README or git init.
- Deny list enforced on `add` **and** on every dependency declared in `pyproject.toml`
  at `lock`/`sync`/`run`, so hand edits can't bypass it. Project discovery walks up
  from the current directory like uv does.
- `pack`/`unpack`: tarball of `.venv` + project files + the exact python-build-standalone
  install, pruned of caches (`__pycache__`, `*.pyc`, pytest/mypy/ruff caches, venv marker
  files) and runtime baggage (stdlib tests, pip/ensurepip, IDLE, Tk demos; ~35 MB for a small app). `unpack`
  relinks `bin/python*`, rewrites `pyvenv.cfg`, shebangs and activate scripts, and refuses
  a different OS/arch. Target machine needs only `tar`.
- `verify`: a Python script embedded in the binary runs with the env's interpreter (no uv):
  imports every package in a forked child, `ldd`s every extension, requires an NVIDIA driver
  when CUDA packages are present (`--allow-missing-gpu` to warn), checks script shebangs.
- `[tool.vivar.policy] index-url` already works (pulled forward from phase 3): a mirror URL
  replaces PyPI as the default index; a directory of wheels becomes the only source (offline).
- `channels`/`blocked-channels` are not written until the native backend can enforce them (phase 2).
- Linux, macOS and Windows: CI runs the full suite on all three; release workflow ships
  static binaries for six targets plus `install.sh` / `install.ps1`, and per-target offline
  bundles (vivar + uv + a CPython runtime) whose installer needs no network at all.
- `upgrade`: replaces the running binary from the latest (or a named) GitHub release via
  `gh`/`curl`, or from a local archive/bundle/URL for offline machines.
- Three direct crates, minimal clap features, size-optimised release profile (<1 MB static binary).
  Inventory in `THIRD_PARTY_LICENSES.md` (all MIT/Apache-2.0/Unlicense), checked in CI.

### Phase 3/4 delivery plan (2026-10-28)

Everything below is in, pure Rust on the existing three crates plus `clap_complete`:

| Area | Commands / settings | Where |
|---|---|---|
| Tasks | `[tool.vivar.scripts]`, `vivar run <task> [args]` | `project.rs`, `main.rs` |
| Inline scripts | PEP 723 `# /// script` blocks, cached env per dependency set | `tool.rs` |
| Tools | `tool run/install/uninstall/list`, console scripts linked into `~/.local/bin` | `tool.rs` |
| Workspaces | members share the root `.venv`; root policy applies to members | `project.rs` |
| Graph | `tree`, `why`, from `uv.lock` | `lock.rs` |
| Upgrades | `outdated` (dry-run re-resolve), `upgrade-deps`, `build` | `backend.rs` |
| Integrity | `vivar-bundle.sha256` written by `pack`, checked by `unpack` and `verify`; `diff` | `bundle.rs`, `sha256.rs` |
| Deploy | `export docker` | `export.rs` |
| Licenses | `allow-licenses`, `license-exceptions`, `licenses` | `licenses.rs` |
| Vulnerabilities | `audit` via OSV querybatch (`VIVAR_OSV_URL` for a mirror) | `audit.rs`, `json.rs` |
| Org policy | `VIVAR_POLICY` / config dir / `/etc/vivar/policy.toml`, merged | `project.rs` |
| Provenance | `require-hashes` | `main.rs` |
| Ops | `doctor`, `env list`, `clean`, `completions` | `doctor.rs`, `envs.rs` |

Follow-up batch (2026-10-29), also done: `export wheelhouse` (lock → exact wheels per
target, hash-checked), checksum verification of Python downloads and release archives,
`run` drift note, `policy` with sources, `--json` on audit/licenses/verify/policy, two
Python versions per offline bundle, `python uninstall|pin`, `shell`, fish/csh activate,
MSI/deb/rpm packages, `SSL_CERT_FILE`/`VIVAR_CA_BUNDLE` for private CAs, org policy
delivered by the installers, `upgrade --check`, `add --cuda` (PyTorch index routing) and a
driver-version check in `verify`, `export requirements`, `pack --slim`, `init --lib`.

AI batch (2026-10-29): GPU detection (`gpu.rs`) driving `add torch`, profiles
(`profile add/list`, `sync --profile auto`, uv conflicting groups + per-group sources),
torch accelerator probes in `verify`, cross-target `pack --target` (download target
Python, `uv pip install --python-platform --target`, hand-built venv under a fake root
that `unpack` rewrites), `--zstd`, `--use-host-cuda`, `require-wheels`, `telemetry`/
`network` policy keys, `--env-file`, index credentials, `kernel install`, CUDA docker base.

Open-source batch (2026-10-29): `import` (requirements/conda/Pipfile/poetry, `import.rs`),
`check`, `export sbom` (CycloneDX/SPDX, `sbom.rs`), parallel wheelhouse downloads,
`unpack --dedupe-with` (hard links by checksum), delta bundles (`diff --patch`,
`unpack --apply`), HMAC-authenticated bundles (`sign.rs`, `key generate`, `pack --sign`,
`require-signed-bundles`).

In-house batch (2026-10-29): the `uv`-free path. `zip.rs` (inflate + zip reader),
`pep.rs` (PEP 440/508 versions, specifiers, requirements, markers), `install.rs` (wheel
installer: RECORD, console scripts, uninstall), `index.rs` (PEP 691/503 listings, PEP 658
metadata, cached), `resolve.rs` (backtracking resolver, newest-first, prefers the existing
lock, universal markers, writes `uv.lock` that `uv lock --check` accepts), `native.rs`
(`NativeBackend`: sync/add/remove/lock/upgrade/install/export/cross-target layout, and
`build` as a PEP 517 driver in a throwaway environment).
`backend_for` picks uv when it is on PATH, native otherwise or with `VIVAR_BACKEND=native`.
Bundle signatures moved from HMAC to Ed25519 (`sha512.rs`, `ed25519.rs`, RFC 8032
vectors): `key generate` writes `bundle.key` + `bundle.pub`, verifiers trust
`trusted-keys` in policy, `VIVAR_TRUSTED_KEYS`, or their own keys.

Deferred: native dependencies via conda-forge (phase 2), Windows launcher stubs for
cross-target Windows bundles, man pages.

## 13. Open questions

- Our own license (internal vs. open source).
- Where does the org policy file live?
- Do we already have an internal package mirror (Artifactory/Nexus/devpi)? `index-url` is ready for it.
