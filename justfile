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
    cargo fmt

# Run static checks
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings

# Run tests
test *ARGS:
    cargo test --all-targets {{ ARGS }}
    cargo test --doc --all-features

# Run tests with coverage (requires cargo-llvm-cov)
cov:
    cargo llvm-cov test --lcov --output-path lcov.info -- --no-capture

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

# Run pre-commit on all files
pre-commit:
    uvx prek run --all-files

# Display project information
info:
    @echo "=== Graft ==="
    @echo "Rust: $(rustc --version)"
    @echo "Cargo: $(cargo --version)"
    @echo ""
    @echo "Workspace members:"
    @cargo metadata --no-deps --format-version 1 2>/dev/null | jq -r '.packages[].name' 2>/dev/null || echo "  (install jq for detailed info)"
