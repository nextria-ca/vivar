import json, sys
meta = json.load(open(sys.argv[1]))
root = meta["resolve"]["root"]
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
# Walk only normal (non-dev, non-build-only) deps of the root so the inventory
# matches what ships in the binary.
seen, stack = set(), [root]
while stack:
    pid = stack.pop()
    if pid in seen: continue
    seen.add(pid)
    for d in nodes[pid]["deps"]:
        if any(k["kind"] in (None, "build") for k in d["dep_kinds"]):
            stack.append(d["pkg"])
seen.discard(root)
rows = sorted((pkgs[i]["name"], pkgs[i]["version"], pkgs[i].get("license") or "?", pkgs[i].get("repository") or "") for i in seen)
bad = [r for r in rows if r[2] == "?" or any(x in r[2] for x in ("GPL", "AGPL", "SSPL", "BUSL"))]
out = ["# Third-party licenses", "",
"Everything vivar links against must be permissively licensed (design doc section 10).",
"This inventory is generated from `cargo metadata` (normal + build dependencies);",
"regenerate it after changing `Cargo.toml`:", "",
"```sh", "cargo metadata --format-version 1 > /tmp/meta.json && python3 scripts/licenses.py /tmp/meta.json > THIRD_PARTY_LICENSES.md", "```", "",
"## Rust crates compiled into the binary", "",
"| Crate | Version | License | Source |", "|---|---|---|---|"]
for n, v, l, r in rows:
    out.append(f"| {n} | {v} | {l} | {r} |")
out += ["", "## External programs invoked at runtime (not linked)", "",
"| Program | License | Notes |", "|---|---|---|",
"| uv | MIT OR Apache-2.0 | Phase 0 backend. Shelled out to, never linked. Replaced in phase 1. |",
"| python-build-standalone (via `uv python install`) | PSF-2.0 + component licenses | Interpreters installed on request. |", ""]
if bad:
    out += ["## Needs review", ""] + [f"- {n} {v}: {l}" for n, v, l, _ in bad] + [""]
print("\n".join(out))
