set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set positional-arguments
set dotenv-load := false

root := justfile_directory()

default: pre-commit-fast

# Every Cargo subprocess, including those launched by validation scripts, enters here.
cargo +args:
    #!/usr/bin/env bash
    set -euo pipefail
    ADK_GATE_ROOT='{{root}}'
    source "$ADK_GATE_ROOT/scripts/local-env.sh"
    workspace="$PWD"
    previous=''
    for argument in "$@"; do
        [[ "$argument" != -- ]] || break
        case "$argument" in
            --target-dir*|--config*|--jobs*|-j*|+*) echo 'error: target, job and toolchain overrides are forbidden' >&2; exit 1 ;;
        esac
        if [[ "$previous" == --manifest-path ]]; then workspace="$(dirname "$argument")"; fi
        if [[ "$argument" == --manifest-path=* ]]; then workspace="$(dirname "${argument#*=}")"; fi
        previous="$argument"
    done
    adk_prepare_target "$workspace"
    exec ionice -c 3 bash "$ADK_GATE_ROOT/scripts/cargo-pinned.sh" "$@"

fmt:
    just cargo fmt --all

fmt-check:
    just cargo fmt --all -- --check

lock-check:
    just cargo metadata --format-version 1 --locked > /dev/null

check:
    just cargo check --workspace --all-targets --locked

clippy:
    just cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    just cargo nextest run --workspace --locked --profile ci

feature-coverage:
    bash scripts/check-feature-coverage.sh all

examples:
    bash scripts/check-examples-compile.sh

docs-build *args:
    RUSTDOCFLAGS="-D warnings" just cargo doc {{args}}

docs:
    just docs-build --workspace --no-deps --locked
    just cargo test --workspace --doc --exclude adk-rust --locked
    just cargo test -p adk-rust --features full --doc --locked

documentation-contracts:
    bash scripts/check-example-name-collisions.sh
    bash scripts/check-doc-examples.sh
    bash scripts/check-doc-versions.sh
    bash scripts/check-release-consistency.sh
    bash scripts/changelog-assemble.sh --check
    bash scripts/check-publish-order.sh
    bash scripts/check-feature-growth.sh
    python3 -m unittest discover -s scripts/tests
    bash scripts/check-cargo-adk-templates.sh

semver:
    #!/usr/bin/env bash
    set -euo pipefail
    for package in adk-core adk-agent adk-model adk-gemini adk-tool adk-runner adk-session adk-server adk-graph adk-memory adk-anthropic; do
        just cargo semver-checks check-release -p "$package" --default-features --release-type minor
    done
    for package in adk-artifact adk-auth adk-telemetry adk-guardrail adk-plugin adk-skill adk-cli adk-realtime adk-browser adk-eval adk-audio adk-acp adk-awp awp-types adk-action adk-code adk-deploy adk-managed adk-enterprise adk-payments adk-rag adk-sandbox adk-retry-reflect adk-mistralrs adk-bench cargo-adk; do
        if ! just cargo semver-checks check-release -p "$package" --default-features --release-type minor; then
            printf 'WARNING: Beta/Experimental semver breakage in %s\n' "$package" >&2
        fi
    done

webui:
    cd adk-server/webui && npm ci && npm run verify
    git diff --exit-code -- adk-server/assets/webui

computer-use-evidence:
    #!/usr/bin/env bash
    set -euo pipefail
    ADK_GATE_ROOT='{{root}}'
    source "$ADK_GATE_ROOT/scripts/local-env.sh"
    node scripts/generate-computer-use-v8-evidence.mjs --subject-version 7.0.0 --output "$TMPDIR/adk-rust-computer-use-v8-evidence.json" --compact

shellcheck:
    shellcheck --severity=warning scripts/local-env.sh scripts/check-examples-compile.sh scripts/check-feature-coverage.sh scripts/check-cargo-adk-templates.sh scripts/check-example-name-collisions.sh

test-local-gates:
    #!/usr/bin/env bash
    set -euo pipefail
    ADK_GATE_ROOT='{{root}}'
    source "$ADK_GATE_ROOT/scripts/local-env.sh"
    python3 scripts/test-local-gates.py

check-branch:
    python3 scripts/local-gates.py check-branch

pre-commit-fast: fmt-check lock-check clippy shellcheck test-local-gates
    bash scripts/check-feature-growth.sh

_quality-gates: pre-commit-fast check test feature-coverage docs documentation-contracts examples semver webui computer-use-evidence

quality-gates:
    #!/usr/bin/env bash
    set -euo pipefail
    ADK_GATE_ROOT='{{root}}'
    source "$ADK_GATE_ROOT/scripts/local-env.sh"
    python3 scripts/local-gates.py produce

pre-push:
    #!/usr/bin/env bash
    set -euo pipefail
    ADK_GATE_ROOT='{{root}}'
    source "$ADK_GATE_ROOT/scripts/local-env.sh"
    python3 scripts/local-gates.py pre-push
