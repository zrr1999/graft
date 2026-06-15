# List all available commands
default:
    @just --list

# Install local development hooks and the CLI into the active Python environment
install:
    uvx prek install --install-hooks --hook-type pre-commit --hook-type commit-msg
    uvx maturin develop

# Build a platform wheel for the packaged CLI into dist/
package-cli:
    uvx maturin build --release --locked --out dist

# Build a source distribution for the packaged CLI into dist/
package-cli-sdist:
    uvx maturin sdist --out dist

# Format all code
format:
    just --fmt --unstable
    cargo fmt --all

# Run static checks
check:
    lean formal/kernel.lean
    cargo fmt --all -- --check
    cargo clippy --locked --workspace --all-targets -- -D warnings

# Run locked workspace tests and doc tests
test *ARGS:
    cargo test --locked --workspace --all-targets {{ ARGS }}
    cargo test --locked --doc --workspace

# Run end-to-end CLI smoke tests
smoke:
    @set -e; for script in tests/*.sh; do echo "==> $script"; bash "$script"; done

# Run locked tests with coverage (requires cargo-llvm-cov)
cov:
    cargo llvm-cov test --locked --workspace --all-targets --lcov --output-path lcov.info -- --no-capture

# Check the declared MSRV where rustup is available; otherwise check with the active 1.95-compatible cargo
msrv:
    @if command -v rustup >/dev/null 2>&1; then \
      cargo +1.95 check --all-targets; \
    else \
      cargo check --all-targets; \
    fi

# Clean build artifacts
clean:
    cargo clean
    rm -rf dist
    rm -f lcov.info

# Run prek on all files
prek:
    uvx prek run --all-files

# Alias for the prek all-files gate
pre-commit: prek

# Display project information
info:
    @echo "=== Graft ==="
    @echo "Rust: $(rustc --version)"
    @echo "Cargo: $(cargo --version)"
    @echo ""
    @echo "Workspace members:"
    @cargo metadata --no-deps --format-version 1 2>/dev/null | jq -r '.packages[].name' 2>/dev/null || echo "  (install jq for detailed info)"
