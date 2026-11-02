import subprocess
for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]:
    graph = subprocess.check_output(["cargo", "tree", "--locked", "--target", target, "--edges", "normal,build", "--prefix", "none"], text=True)
    forbidden = [line for line in graph.splitlines() if line.startswith(("rsa ", "atomic-polyfill ", "h2 v0.3."))]
    if forbidden:
        raise SystemExit(f"Unexpected vulnerable dependency in {target}: {forbidden}")
print("Supported deployment graphs exclude RSA, embedded atomic-polyfill, and old h2")
