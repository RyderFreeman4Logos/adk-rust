#!/usr/bin/env bash
# Source before local gates so children inherit the verified SSD temporary root.
set -euo pipefail
[[ -z "${CARGO_TARGET_DIR+x}" && -z "${CARGO_BUILD_TARGET_DIR+x}" && -z "${CARGO_BUILD_BUILD_DIR+x}"
    && -z "${CARGO_BUILD_JOBS+x}" && -z "${CARGO_HOME+x}" ]] || {
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

# Template manifests are temporary source; all build output stays with the SDK.
adk_template_cargo() {
    local operation="$1" manifest="$2" destination
    local -a options=()
    case "$operation" in
        check) ;;
        metadata) options=(--format-version 1 --no-deps) ;;
        *) echo 'error: template Cargo operation must be check or metadata' >&2; return 1 ;;
    esac
    cd "$ADK_GATE_ROOT"
    [[ -L target ]] || { echo 'error: template gate requires the existing SDK target link' >&2; return 1; }
    adk_prepare_target "$ADK_GATE_ROOT"
    destination="$(python3 - "$manifest" "$(readlink target)" <<'PYTHON'
import json
import os
from pathlib import Path
import stat
import sys
manifest = Path(sys.argv[1])
base = Path(os.environ["TMPDIR"])
if not manifest.is_absolute() or ".." in manifest.parts or manifest.name != "Cargo.toml":
    sys.exit("error: template manifest must be an absolute temporary Cargo.toml")
try:
    relative = manifest.relative_to(base)
except ValueError:
    sys.exit("error: template manifest must be under verified SSD temporary root")
if len(relative.parts) < 2 or not manifest.is_file():
    sys.exit("error: template manifest must exist in a private temporary directory")
private = base / relative.parts[0]
if stat.S_IMODE(private.stat().st_mode) != 0o700:
    sys.exit("error: template source directory must be private")
for path in (manifest, *manifest.parents):
    if path == base:
        break
    info = path.lstat()
    if path.is_symlink() or info.st_uid != os.getuid() or info.st_mode & 0o022:
        sys.exit("error: unsafe template source ancestry")
print(json.dumps(sys.argv[2]))
PYTHON
)"
    exec ionice -c 3 bash "$ADK_GATE_ROOT/scripts/cargo-pinned.sh" \
        --config "build.target-dir=$destination" --config "build.build-dir=$destination" \
        "$operation" --manifest-path "$manifest" "${options[@]}"
}
