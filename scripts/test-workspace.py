"""Run every workspace package's tests with the features it ships with."""

import subprocess
import sys

# CLI integration tests launch the sibling shim binary. Build and test it first,
# in a separate Cargo invocation so it doesn't acquire the CLI's install feature.
for packages in (["-p", "osdk-shim"], ["--workspace", "--exclude", "osdk-shim"]):
    result = subprocess.run(["cargo", "test", "--locked", *packages, *sys.argv[1:]])
    if result.returncode:
        sys.exit(result.returncode)
