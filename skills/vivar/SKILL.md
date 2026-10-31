---
name: vivar
description: Manage Python environments with vivar (a slim, policy-aware environment manager that drives uv and can run without it). Use when a project has a pyproject.toml managed by vivar, when the user asks to create/lock/sync a Python environment, add or remove packages, bundle an environment for an offline or air-gapped machine, verify that an environment (CUDA, GPU, native libraries) works on this machine, work with conda-forge native dependencies, or apply an organisation policy (deny lists, mirrors, licenses, signed bundles).
---

# vivar

vivar is a single binary. It keeps everything in `pyproject.toml` and `uv.lock`
(uv-compatible), never needs activation, and enforces policy on every install.
Prefer vivar commands over `pip`, `uv` or `conda` in a vivar project: policy
checks, the lockfile and the native store only stay consistent that way.

## Is vivar installed?

```sh
vivar --version || curl -LsSf https://raw.githubusercontent.com/nextria-ca/vivar/main/install.sh | sh
```

Windows: `irm https://raw.githubusercontent.com/nextria-ca/vivar/main/install.ps1 | iex`.
The installers verify the release checksums and install `uv` if it is missing.
`vivar doctor` shows what is installed, configured or broken; run it first when
anything looks off.

## Everyday workflow

```sh
vivar init --python 3.12          # new project: pyproject.toml, .python-version, .venv, uv.lock
vivar add numpy "pandas>=2"       # policy-checked, then locked and installed
vivar add --group dev pytest      # dependency groups
vivar add ./local-pkg --editable  # a local project as a path source
vivar remove pandas
vivar lock                        # re-resolve, write uv.lock
vivar sync                        # make .venv match uv.lock exactly (--profile gpu, --extra viz)
vivar run python app.py           # run inside the env, no activation
vivar run pytest                  # console scripts too; `vivar run <task>` runs a [tool.vivar.scripts] entry
vivar shell                       # a shell with the env active
vivar tree / vivar why numpy      # dependency tree, why a package is present
vivar outdated / vivar upgrade-deps   # what would change; upgrade the lock (`vivar upgrade` updates vivar itself)
vivar python install 3.13         # prebuilt standalone interpreters
vivar tool install ruff           # isolated tool envs with linked commands
vivar run script.py               # PEP 723 inline-metadata scripts get their own cached env
```

Rules of thumb:

- Never edit `uv.lock` by hand; change `pyproject.toml` (or use `vivar add`) and run `vivar lock`.
- If a command is refused by policy (deny list, license, locked index), do not
  work around it with pip or by editing the lock. Report the policy message to
  the user; `vivar policy` shows which file the rule comes from.
- `VIVAR_BACKEND=native` forces vivar's own resolver and installer (no uv).
  A locked policy or native dependencies select it automatically.
- GPU projects: `vivar add torch` routes to the right PyTorch index for the
  machine's accelerator; `vivar sync --profile auto` picks a profile by GPU.

## Moving an environment to another machine

```sh
vivar pack                              # <name>-<platform>.tar.gz: .venv + its Python + lock; unpacks anywhere
vivar pack --slim --sign --zstd         # smaller, signed (Ed25519), zstd
vivar pack --target linux-x86_64 --python 3.12   # build for another OS/arch from here
vivar unpack app.tar.gz [dir]           # no uv, no network needed on the target
vivar unpack --require-signature --not-before 2026-01-01 app.tar.gz
vivar diff a.tar.gz b.tar.gz            # what changed; --patch makes a delta, `unpack --apply` applies it
vivar export requirements|sbom|wheelhouse   # requirements.txt, CycloneDX/SPDX SBOM, a directory of wheels
```

Air-gapped flow: build and `vivar pack` where there is network, copy one archive,
`vivar unpack` then `vivar verify` on the target.

## Verifying that an environment actually works here

```sh
vivar verify                     # imports every package in isolation, checks native libs, CUDA driver, GPU kernels
vivar verify --min-vram 48       # also fail if a GPU has less than 48 GB free
vivar verify --allow-missing-gpu # CPU-only CI
vivar verify --json              # machine-readable report
vivar verify ./unpacked-bundle   # a bundle, or --static for one built for another platform
vivar check                      # policy + lock + env + licenses + audit (OSV) in one go
```

Read the per-line results: `ERR` lines are what to fix; `WARN` lines (e.g. PTX-only
kernels for a newer GPU) are worth telling the user about.

## Native (conda-forge) dependencies

```sh
vivar add --native tzdata openssl   # solved from conda-forge into .venv (a conda prefix with its own Python)
vivar remove --native openssl
```

They are recorded in `vivar-native.lock`; `vivar sync`, `pack`, `verify`,
`licenses` and `sbom` cover them. Anaconda's commercial channels are refused
unless policy allows them.

## Policy

Project policy lives under `[tool.vivar.policy]` in `pyproject.toml`; an org
policy file (`VIVAR_POLICY`, `~/.config/vivar/policy.toml`, and always
`/etc/vivar/policy.toml` or `%ProgramData%\vivar\policy.toml`) applies on top.
Keys: `deny`, `index-url` (a mirror URL or a wheelhouse directory),
`allow-licenses`, `license-exceptions`, `require-hashes`, `require-wheels`,
`telemetry = "off"`, `network = "offline"`, `require-signed-bundles`,
`trusted-keys`, `blocked-channels`, `allow-anaconda-defaults`, `conda-url`,
`artifact-hosts`, and `locked = true` (an administrator's index, mirror and
keys become the only ones; vivar's own resolver then does every install).

```sh
vivar policy          # the effective policy and where each rule comes from (--json for tooling)
vivar doctor          # also checks the system policy file is root-owned and not world-writable
```

## When something fails

1. `vivar doctor` first.
2. Read the error: vivar names the file, package or policy rule involved.
3. Resolution problems: `vivar lock` output names the conflict; `vivar why <pkg>`.
4. Import or GPU problems: `vivar verify --verbose`.
5. Disk: `vivar env list` shows every environment and cache; `vivar clean` removes
   script envs, caches and unpacked wheels no environment links (`--tools`, `--store`).

Full documentation: https://github.com/nextria-ca/vivar (README).
