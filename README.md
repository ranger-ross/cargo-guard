# Cargo Guard

Cargo build script allow list

## Setup

### 1. Install Cargo Guard

```sh
cargo install cargo-guard
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

# `relative = true` makes Cargo resolve `.` to the directory containing `.cargo`.
# This tells Cargo Guard where to find the approval file, even when Cargo runs it
# from a dependency's source directory.
[env]
CARGO_GUARD_WORKSPACE_ROOT = { value = ".", relative = true }
```

To setup cargo guard globally add the following to your `~/.cargo/config.toml`

```toml
[unstable]
host-config = true
target-applies-to-host = true

[host]
runner = "cargo-guard"
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

# Allow all build scripts that match a fingerprint of the build script (not tied to single package version)
# The format is {build.rs hash}+{metadata}#{package version}
#
# The build.rs hash is the sha256 hash of the build script source file.
#
# Metadata here means anything that can influence build script that is not in the build.rs.
# This can include files included with env!() / include!(), build-dependencies, etc
#
# The `#{version}` is only a hint for `cargo guard review` to give better diffs in the UI.
# It does not pin to a specific version
foo = "f7609b919173e7121493266d0f89db724a1e47bb105ebd05397f869639603eec+61d8c10f3feb6de7#1.1.5"

# Allow multiple versions
foo = ["1.0.0", "1.2.0"]

# Allow multiple version and fingeprints mixed
foo = ["1.0.0", "f7609b919173e7121493266d0f89db724a1e47bb105ebd05397f869639603eec+61d8c10f3feb6de7#1.1.5"]

# Allow all version from this crate
foo = "approved"
```

### Security and Limitations

Regarding the security of these options, they all come with trades offs.
Pinning to a single version is the most secure, but the most demanding in terms of maintenance.
Everytime you update a dependency (including transitively) that has a build script, you must re-review
and approve it even if it has not changed. This can to reviewers rubber stamping approvals due to [alarm fatigue](https://en.wikipedia.org/wiki/Alarm_fatigue).

On the other end of the spectrum is `"approved"` which blanket approves the build scripts for a given
crate regardless of the version. This is the least secure and will not protect you if an attack takes control
of a crate with an existing build script and adds malicious code to it.

The fingerprint is meant to serve as a middleground between version pinning and blanket approvals by
taking a fingerprint of a build script and only requiring a re-approval if it has changed.
The fingerprint attempts to track the most common attack vectors for build.rs attacks but does not guarantee to
capture everything.


