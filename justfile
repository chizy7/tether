# One place for the checks CI runs, so "passes here" means "passes there".
# Install just from https://just.systems, then run `just` to list the
# recipes. Every recipe is a plain command you can also run by hand.

# List the recipes.
default:
    @just --list

# What the main CI job runs: format, lint, prose and tests for the Rust workspace.
check: fmt-check lint prose test

# Everything CI checks, SDKs included. Needs Python, Node and Docker.
all: check docker python typescript

# Format the Rust code.
fmt:
    cargo fmt --all

# Fail if the Rust code is not formatted.
fmt-check:
    cargo fmt --all -- --check

# Clippy over the whole workspace, warnings as errors.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# The Rust tests. They need no API key and no network.
test:
    cargo test --workspace

# No em dashes in prose, docs or comments (see CONTRIBUTING.md).
prose:
    #!/bin/sh
    dash=$(printf '\342\200\224')
    if git grep -nI "$dash"; then
        echo "error: the lines above contain an em dash; write the sentence without one" >&2
        exit 1
    fi

# The Docker sandbox backend. Needs Docker running.
docker:
    cargo clippy -p tether-sandbox --all-targets --features docker -- -D warnings
    cargo test -p tether-sandbox --features docker

# The oldest supported Rust still builds the workspace. Needs `rustup toolchain install 1.89`.
msrv:
    cargo +1.89 check --workspace

# The Python SDK and examples: lint, format check and tests.
python: python-lint python-test

# Needs `pip install -e "sdks/python[dev]"`.
python-lint:
    ruff check .
    ruff format --check .

# The SDK's tests drive a real daemon, so it is built first.
python-test:
    cargo build -p tetherd
    python -m pytest sdks/python

# Format the Python code and apply the safe lint fixes.
python-fmt:
    ruff check --fix .
    ruff format .

# The TypeScript SDK: lint, format check and tests.
typescript: typescript-lint typescript-test

# Needs `npm ci` in sdks/typescript.
typescript-lint:
    cd sdks/typescript && npm run lint

# The SDK's tests drive a real daemon, so it is built first.
typescript-test:
    cargo build -p tetherd
    cd sdks/typescript && npm test

# Format the TypeScript code and apply the safe lint fixes.
typescript-fmt:
    cd sdks/typescript && npm run format

# Regenerate the Python gRPC stubs after changing proto/tether.proto.
proto:
    sh sdks/python/scripts/gen_proto.sh
