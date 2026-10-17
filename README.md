# Cargo Guard

Cargo build script allow list

## Setup / Configuration

### 1. Install Cargo Guard

```sh
cargo install cargo-guard
```

### 2. Configure your workspace

Add the following to `.cargo/config.toml` in your project's workspace root:

```toml
[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = "cargo-guard"

# `relative = true` makes Cargo resolve `.` to the directory containing `.cargo`.
# This tells Cargo Guard where to find the approval file, even when Cargo runs it
# from a dependency's source directory.
[env]
CARGO_GUARD_WORKSPACE_ROOT = { value = ".", relative = true }
```

### 3. Approve build scripts

Create `approved-builds.toml` in the workspace root.

Example:

```toml
[approved]
parking_lot_core = "0.9.12"
```

This build your project normally with `cargo build` and you will get an error if an unapproved build script tries to run.

