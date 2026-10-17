# Cargo Guard

Cargo build script allow list


## TODO

- [ ] Handle multiple build scripts per crate
- [ ] Support approving by hash. What needs to be hashed, need to be careful about edge cases allowing sneaking in code into the build script. Could we use dep-info for this? Should we support hashing the output binary? Would that be annoying due to absolute paths and having to handle multiple platforms?
- [ ] Create a TUI for reviewing and approving new build scripts
  - [ ] Compare to previously approve build script

