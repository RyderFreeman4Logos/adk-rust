#!/usr/bin/env bash
# Source before local gates so children inherit the verified SSD temporary root.
set -euo pipefail
[[ -z "${CARGO_TARGET_DIR+x}" && -z "${CARGO_BUILD_JOBS+x}" && -z "${CARGO_HOME+x}" ]] || {
    echo 'error: Cargo target, home and job overrides are forbidden for local gates' >&2; exit 1;
}
adk_tmp="$(readlink -f "$HOME/tmp")"
python3 - "$adk_tmp" <<'PY'
import os
from pathlib import Path
import stat
import sys
p = Path(sys.argv[1])
if not p.is_dir() or not os.access(p, os.W_OK) or p.stat().st_dev != Path('/mnt/ssd').stat().st_dev:
    sys.exit('error: local gates require writable SSD $HOME/tmp')
for ancestor in (p, *p.parents):
    mode = ancestor.stat().st_mode
    if mode & 0o022 and not mode & stat.S_ISVTX:
        sys.exit(f'error: unsafe temporary ancestor: {ancestor}')
if stat.S_IMODE(p.stat().st_mode) != 0o700:
    sys.exit('error: temporary root must have mode 0700')
PY
export TMPDIR="$adk_tmp" TMP="$adk_tmp" TEMP="$adk_tmp"

adk_prepare_target() {
    local workspace="$1" expected existing
    workspace="$(cd "$workspace" && pwd -L)"
    expected="/ssd/mirror-rootfs${workspace}/target"
    if [[ -L "$workspace/target" ]]; then
        existing="$(readlink "$workspace/target")"
        # Preserve the pre-existing checkout link's configured double slash.
        [[ "$existing" == "$expected" || ( "$workspace" == "$ADK_GATE_ROOT" && "$existing" == "/ssd/mirror-rootfs/${workspace}/target" ) ]] || {
            echo "error: unexpected target link at $workspace/target" >&2; return 1;
        }
        [[ -d "$existing" && -w "$existing" ]] || { echo 'error: target is unavailable' >&2; return 1; }
    elif [[ -e "$workspace/target" ]]; then
        echo "error: preserve existing target; expected a symlink at $workspace/target" >&2
        return 1
    else
        mkdir -p "$expected"
        ln -s "$expected" "$workspace/target"
    fi
}
