# Runs inside the environment being verified (python -I - [flags]).
# Checks that every installed package is actually usable on this machine.
import ctypes, importlib, importlib.metadata as md, os, platform, re, signal, subprocess, sys, sysconfig

VERBOSE = "--verbose" in sys.argv
ALLOW_MISSING_GPU = "--allow-missing-gpu" in sys.argv
JSON = "--json" in sys.argv
IMPORT_TIMEOUT = 120
SITE = sysconfig.get_paths()["purelib"]
CUDA_LIB = re.compile(r"^lib(cuda|cudart|cublas|cublasLt|cudnn|cufft|curand|cusparse|cusolver|nvrtc|nccl|nvJitLink|nvToolsExt)\b")

errors, warnings, records = [], [], []
warnings_only = []  # CUDA notes that are never errors
ok_count = 0


def say(text):
    if not JSON:
        print(text)


def report(kind, what, detail):
    (errors if kind == "ERR" else warnings).append(f"{what}: {detail}")
    records.append({"level": "error" if kind == "ERR" else "warning", "subject": what, "detail": detail})
    say(f"  {kind:4} {what}: {detail}")


def top_levels(dist):
    names = set()
    text = dist.read_text("top_level.txt")
    if text:
        names.update(l.strip() for l in text.splitlines() if l.strip())
    else:
        for f in dist.files or ():
            parts = f.parts
            if not parts or parts[0] in ("..", "__pycache__") or parts[0].endswith((".dist-info", ".data")):
                continue
            if len(parts) == 1:
                stem = parts[0].split(".")[0]
                if parts[0].endswith((".py", ".so", ".pyd")):
                    names.add(stem)
            elif "." not in parts[0]:
                names.add(parts[0])
    return sorted(n for n in names if n.isidentifier() and n not in ("test", "tests"))


def try_import(name):
    """Import in an isolated child; returns None on success or an error string."""
    if not hasattr(os, "fork"):
        try:
            r = subprocess.run([sys.executable, "-I", "-c", f"import {name}"],
                               capture_output=True, text=True, timeout=IMPORT_TIMEOUT)
        except subprocess.TimeoutExpired:
            return f"import timed out after {IMPORT_TIMEOUT}s"
        return None if r.returncode == 0 else (r.stderr.strip().splitlines() or ["import failed"])[-1]

    rd, wr = os.pipe()
    pid = os.fork()
    if pid == 0:  # child
        os.close(rd)
        try:
            signal.alarm(IMPORT_TIMEOUT)
            null = os.open(os.devnull, os.O_WRONLY)
            os.dup2(null, 1)
            os.dup2(null, 2)
            importlib.import_module(name)
            os._exit(0)
        except BaseException as e:  # noqa: BLE001
            os.write(wr, f"{type(e).__name__}: {e}".encode(errors="replace")[:2000])
            os._exit(1)
    os.close(wr)
    msg = os.read(rd, 4096).decode(errors="replace")
    os.close(rd)
    _, status = os.waitpid(pid, 0)
    if os.WIFSIGNALED(status):
        sig = os.WTERMSIG(status)
        if sig == signal.SIGALRM:
            return f"import timed out after {IMPORT_TIMEOUT}s"
        return f"crashed with {signal.Signals(sig).name} during import"
    return None if os.WEXITSTATUS(status) == 0 else (msg or "import failed")


def native_libs():
    """(path, [needed libs not found by the loader]) for every ELF in site-packages."""
    found = []
    for root, _, files in os.walk(SITE):
        found.extend(os.path.join(root, f) for f in files if ".so" in f)
    bundled = {os.path.basename(p) for p in found}
    missing, needed = {}, set()
    for i in range(0, len(found), 200):
        chunk = found[i:i + 200]
        try:
            out = subprocess.run(["ldd", *chunk], capture_output=True, text=True).stdout
        except OSError:
            return {}, set()
        current = None
        for line in out.splitlines():
            if not line.startswith("\t"):
                current = line.rstrip(":")
            else:
                lib = line.strip().split(" ")[0]
                needed.add(lib)
                if "not found" in line and lib not in bundled:
                    missing.setdefault(current, []).append(lib)
    return missing, needed


def cuda_problem(runtime):
    """runtime: the CUDA version the env's packages were built for, as (major, minor) or None."""
    lib = os.environ.get("VIVAR_CUDA_LIB", "nvcuda.dll" if sys.platform == "win32" else "libcuda.so.1")
    try:
        cuda = ctypes.CDLL(lib)
    except OSError:
        return f"no NVIDIA driver on this machine ({lib} not found)"
    code = cuda.cuInit(0)
    if code != 0:
        return f"NVIDIA driver present but cuInit failed with code {code}"
    n = ctypes.c_int(0)
    cuda.cuDeviceGetCount(ctypes.byref(n))
    if n.value == 0:
        return "NVIDIA driver present but no GPU found"
    if runtime:
        # A driver for an older major version cannot run these packages at all.
        # Within a major, NVIDIA's minor-version compatibility usually works but
        # not for every feature, so that is reported separately as a warning.
        v = ctypes.c_int(0)
        if cuda.cuDriverGetVersion(ctypes.byref(v)) == 0 and v.value:
            have = (v.value // 1000, v.value % 1000 // 10)
            if have[0] < runtime[0]:
                return (f"the driver supports CUDA {have[0]}.{have[1]} but the packages need "
                        f"CUDA {runtime[0]}.{runtime[1]} (update the NVIDIA driver)")
            if have < runtime:
                warnings_only.append(f"the driver supports CUDA {have[0]}.{have[1]}, the packages were built for "
                                     f"CUDA {runtime[0]}.{runtime[1]}: relying on minor-version compatibility")
    return None


def cuda_runtime_version(dists):
    """(major, minor) of the bundled CUDA runtime, from nvidia-cuda-runtime-cuNN."""
    for d in dists:
        name = d.metadata["Name"].lower()
        if name.startswith("nvidia-cuda-runtime"):
            m = re.match(r"(\d+)\.(\d+)", d.version or "")
            if m:
                return int(m.group(1)), int(m.group(2))
    return None


def main():
    global ok_count
    venv = sys.prefix
    say(f"vivar verify: {venv} (Python {sys.version.split()[0]}, {sys.platform}-{platform.machine()})")

    # 1. Every package imports.
    dists = sorted(md.distributions(), key=lambda d: d.metadata["Name"].lower())
    for dist in dists:
        name = dist.metadata["Name"]
        mods = top_levels(dist)
        failed = [(m, e) for m in mods if (e := try_import(m))]
        if failed:
            for m, e in failed:
                report("ERR", name, f"import {m} failed: {e}")
        else:
            ok_count += 1
            if VERBOSE:
                say(f"  ok   {name} ({', '.join(mods) or 'no modules'})")

    # 2. Native libraries resolve (Linux).
    needed = set()
    if sys.platform.startswith("linux"):
        missing, needed = native_libs()
        for path, libs in sorted(missing.items()):
            report("ERR", os.path.relpath(path, SITE), f"needs {', '.join(libs)}, not found on this machine")

    # 3. CUDA packages need a driver.
    cuda_dists = [d.metadata["Name"] for d in dists if d.metadata["Name"].lower().startswith("nvidia-")]
    cuda_libs = sorted(l for l in needed if CUDA_LIB.match(l))
    if cuda_dists or cuda_libs:
        problem = cuda_problem(cuda_runtime_version(dists))
        if problem:
            why = ", ".join((cuda_dists or cuda_libs)[:5])
            report("WARN" if ALLOW_MISSING_GPU else "ERR", "CUDA", f"environment contains CUDA packages ({why}) but {problem}")
        elif VERBOSE:
            say(f"  ok   CUDA driver and GPU present for {', '.join((cuda_dists or cuda_libs)[:5])}")
        for note in warnings_only:
            report("WARN", "CUDA", note)

    # 4. Scripts point at an interpreter that exists.
    bindir = os.path.join(venv, "Scripts" if os.name == "nt" else "bin")
    for entry in sorted(os.listdir(bindir)):
        path = os.path.join(bindir, entry)
        if os.path.islink(path) or not os.path.isfile(path):
            continue
        with open(path, "rb") as fh:
            first = fh.readline(512)
        if first.startswith(b"#!"):
            interp = first[2:].strip().split(b" ")[0].decode(errors="replace")
            if re.match(r"/|[A-Za-z]:\\", interp) and not os.path.exists(interp):
                report("ERR", entry, f"shebang points at missing {interp} (moved without `vivar unpack`?)")

    if JSON:
        import json
        print(json.dumps({
            "venv": venv,
            "python": sys.version.split()[0],
            "platform": f"{sys.platform}-{platform.machine()}",
            "packages": len(dists),
            "ok": ok_count,
            "findings": records,
        }, indent=2))
    else:
        print(f"summary: {len(dists)} packages, {ok_count} ok, {len(errors)} errors, {len(warnings)} warnings")
    sys.exit(1 if errors else 0)


main()
