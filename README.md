# Cargo Guard

[![CI Status](https://github.com/ranger-ross/cargo-guard/workflows/Test/badge.svg)](https://github.com/ranger-ross/cargo-guard/actions)
[![Crates.io](https://img.shields.io/crates/v/cargo-guard.svg)](https://crates.io/crates/cargo-guard)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/ranger-ross/reqwest-metrics/blob/master/LICENSE)

Cargo build script allowlist.
Just `cargo build`, no wrappers needed.

## Setup

### 1. Install Cargo Guard

```sh
cargo install cargo-guard
# or
cargo binstall cargo-guard
```

### 2. Configure Cargo

You have 2 options:
1. Workspace approved builds (that can be committed to version control)
2. Global approved builds (runs allow list on all workspaces)

To setup your project add the following to `.cargo/config.toml` in your project's workspace root:

```toml
[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = "cargo-guard"
```

To setup cargo guard globally add the following to your `~/.cargo/config.toml`

```toml
[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = "cargo-guard"

[env]
CARGO_GUARD_GLOBAL_APPROVED_BUILDS_PATH = { value = ".cargo/approved-builds.toml", relative = true }
```

This configuration will tell Cargo to build your projects will cargo-guard as middleware between your Cargo and your build scripts.
You will get an error if an unapproved build script tries to run.

## Usage

Create `approved-builds.toml` in the workspace root.

Example:

```toml
[approved]
foo = "0.9.12"
```

It is recommended to use `cargo guard review` to automate this process.

## Approval Syntax

```toml
[approved]
# Allow a single version
foo = "0.9.12"

# Allow multiple versions
foo = ["1.0.0", "1.2.0"]

# Allow all version from this crate
foo = "approved"
```

## Security

Regarding the security of these options, they all come with trades offs.
Pinning to a single version is the most secure, but the most demanding in terms of maintenance.
Everytime you update a dependency (including transitively) that has a build script, you must re-review
and approve it even if it has not changed. This can to reviewers rubber stamping approvals due to [alarm fatigue](https://en.wikipedia.org/wiki/Alarm_fatigue).
`cargo guard review` diffs the build script against the previously approved version to keep re-reviews small.

On the other end of the spectrum is `"approved"` which blanket approves the build scripts for a given
crate regardless of the version. This is the least secure and will not protect you if an attack takes control
of a crate with an existing build script and adds malicious code to it.

## Motivations

The goal of this project is to reduce the risk of a malious build script harming the Rust ecosystem.
Build scripts are a known attack vector in the Rust supply chain and it [not thoeritical](https://blog.rust-lang.org/2026/08/20/supply-chain-attack-on-arrayref/).
This project is an initial attempt at this problem taking inspiration from npm/pnpm's [allow scripts](https://docs.npmjs.com/cli/v11/commands/npm-approve-scripts) feature.
Long term, Cargo itself should probably handle this in a first class way. (see [cargo#13681](https://github.com/rust-lang/cargo/issues/13681))

## Limitations

* Requires nightly Rust to use `host.runner`
* Does not handle proc-macros

