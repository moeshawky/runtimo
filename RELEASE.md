# Release Checklist - Runtimo v0.1.0-alpha

## Pre-Release Verification

### Build & Test
- [x] `cargo build --workspace` - Compiles without errors
- [x] `cargo test -p runtimo-core` - 13 unit tests pass
- [x] `cargo test -p runtimo-core --test integration` - 31 integration tests pass
- [x] `cargo test --doc -p runtimo-core` - 7 doc tests pass
- [x] `cargo doc -p runtimo-core --no-deps` - Documentation generates

### Code Quality
- [x] No unused variables (except intentional)
- [x] No clippy warnings
- [x] All error types documented
- [x] Public API has doc comments

### Documentation
- [x] README.md updated
- [x] CHANGELOG.md created
- [x] GETTING_STARTED.md created
- [x] API.md updated
- [x] ARCHITECTURE.md exists
- [x] Runbooks exist
- [x] docs.rs documentation ready

### Fixed Issues (v0.1.0)
- [x] F1: Execution timeout - Added timeout parameter (enforcement deferred)
- [x] F2: PPID tracking - Changed ps format to include parent PIDs
- [x] F3: Shell safety - Added security documentation

### Known Limitations (Documented)
- [ ] Daemon is placeholder (Unix socket, JSON-RPC not implemented)
- [ ] No process kill capability
- [ ] WAL defaults to /tmp (not persistent)
- [ ] Backup cleanup is stub
- [ ] No ShellExec capability
- [ ] No HTTP capability
- [ ] No concurrent job execution
- [ ] Timeout enforcement deferred

## Release Steps

### 1. Version Bump
```bash
# Update workspace version in Cargo.toml
# Current: 0.1.0-alpha
# Target: 0.1.0
```

### 2. Final Test Run
```bash
cargo clean
cargo build --workspace
cargo test --workspace
cargo doc --workspace --no-deps
```

### 3. Publish to crates.io
```bash
# Publish core library first
cargo publish -p runtimo-core

# Verify on crates.io
# https://crates.io/crates/runtimo-core
```

### 4. Tag Release
```bash
git tag -a v0.1.0-alpha -m "Runtimo v0.1.0-alpha: Initial release"
git push origin v0.1.0-alpha
```

### 5. GitHub Release
- [ ] Create release on GitHub
- [ ] Attach release notes from CHANGELOG.md
- [ ] Mark as pre-release (alpha)

## Post-Release

### Verify Publication
- [ ] Check crates.io page
- [ ] Verify docs.rs build
- [ ] Test installation: `cargo install runtimo-core`

### Announce
- [ ] Post to community
- [ ] Update project README with version badge
- [ ] Notify stakeholders

## Upgrade Path (Future)

For v0.2.0 and beyond:
- Check CHANGELOG.md for breaking changes
- Review migration guide if applicable
- Test with existing code

## Rollback Plan

If issues discovered post-release:
1. Yank crate: `cargo yank runtimo-core@0.1.0`
2. Fix issues in main branch
3. Release as 0.1.1 or 0.2.0 (depending on severity)
4. Document known issues in CHANGELOG

## Release Notes Template

```markdown
## [0.1.0-alpha] - 2026-05-16

### Added
- Capability trait with validation and execution
- Two-layer telemetry (hardware + process)
- Resource guards via llmosafe
- Write-ahead log with fsync
- Backup/undo support
- FileRead and FileWrite capabilities
- CLI with 7 commands

### Fixed
- F1: Added timeout parameter to executor
- F2: PPID tracking in process snapshot
- F3: Shell command security documentation

### Known Issues
- Daemon is placeholder
- No process kill capability
- Timeout enforcement deferred
- Backup cleanup is stub

### Documentation
- README.md with quick start
- CHANGELOG.md
- GETTING_STARTED.md
- API.md
- Runbooks
```

## Checklist Sign-Off

- [ ] All tests pass
- [ ] Documentation complete
- [ ] CHANGELOG updated
- [ ] Version tags applied
- [ ] Published to crates.io
- [ ] GitHub release created
- [ ] Community notified

**Release Date:** 2026-05-16  
**Release Manager:** [Name]  
**Approved By:** [Name]
