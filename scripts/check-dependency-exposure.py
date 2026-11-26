import subprocess
for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]:
    graph = subprocess.check_output(["cargo", "tree", "--locked", "--target", target, "--edges", "normal,build", "--prefix", "none"], text=True)
    forbidden = [line for line in graph.splitlines() if line.startswith(("rsa ", "atomic-polyfill ", "h2 v0.3."))]
    if forbidden:
        raise SystemExit(f"Unexpected vulnerable dependency in {target}: {forbidden}")
    # RUSTSEC-2026-0594 affects only cookie::PrivateJar, which exists only with
    # cookie's "private" or "secure" feature. Fail if either is ever enabled.
    features = subprocess.check_output(["cargo", "tree", "--locked", "--target", target, "--edges", "features", "--invert", "cookie", "--prefix", "none"], text=True)
    enabled = {line.split('"')[1] for line in features.splitlines() if line.startswith('cookie feature "')}
    if enabled & {"private", "secure"}:
        raise SystemExit(f"cookie private jars are compiled in {target}: {sorted(enabled)}")
print("Supported deployment graphs exclude RSA, embedded atomic-polyfill, old h2 and cookie private jars")
