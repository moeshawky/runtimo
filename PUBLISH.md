# Publishing Runtimo v0.1.0-alpha

**Date:** 2026-05-16  
**Version:** 0.1.0-alpha  
**Status:** Ready for publication

## Package Summary

**runtimo-core** - Agent-centric capability runtime for persistent machines

A Rust library providing:
- Capability execution engine with JSON Schema validation
- Two-layer telemetry (hardware + process tracking)
- Resource guards via llmosafe integration
- Write-ahead log for crash recovery
- Backup/undo support for file operations
- CLI tool (`moe`) for capability execution

**Key Features:**
- Hallucination absorption through validation
- Persistent machine awareness (cannot factory reset)
- Full telemetry before/after every execution
- Process lineage tracking (PPID)
- Security-first design (path traversal protection)

## Files Prepared

### Documentation
- [x] `README.md` - Updated with comprehensive guide
- [x] `CHANGELOG.md` - Complete changelog (Keep a Changelog format)
- [x] `RELEASE.md` - Release checklist
- [x] `docs/API.md` - API reference
- [x] `docs/GETTING_STARTED.md` - Getting started guide
- [x] `docs/ARCHITECTURE.md` - Architecture documentation
- [x] `docs/runbooks/` - Operational runbooks

### Code
- [x] `core/src/lib.rs` - Library entry with full documentation
- [x] `core/src/capability.rs` - Capability trait
- [x] `core/src/executor.rs` - Execution pipeline
- [x] `core/src/telemetry.rs` - Hardware telemetry
- [x] `core/src/processes.rs` - Process snapshot with PPID
- [x] `core/src/wal.rs` - Write-ahead log
- [x] `core/src/backup.rs` - Backup manager
- [x] `core/src/llmosafe.rs` - Resource guard
- [x] `core/src/schema.rs` - JSON Schema validator
- [x] `core/src/capabilities/` - Built-in capabilities
- [x] `cli/src/main.rs` - CLI binary (moe)

### Tests
- [x] 13 unit tests
- [x] 31 integration tests
- [x] 7 doc tests
- [x] All tests passing

## Pre-Publish Checklist

### Code Quality
- [x] No compilation errors
- [x] No clippy warnings
- [x] All tests pass
- [x] Documentation builds without warnings
- [x] Examples compile

### Documentation
- [x] README comprehensive
- [x] CHANGELOG follows format
- [x] API docs complete
- [x] Getting started guide tested
- [x] Architecture documented
- [x] Known limitations listed

### Security
- [x] No secrets in code
- [x] No hardcoded credentials
- [x] Path traversal protection implemented
- [x] Resource limits enforced
- [x] Security warnings documented

### Version
- [x] Version: 0.1.0-alpha
- [x] Edition: 2021
- [x] License: MIT
- [x] Authors: Moe

## Publish Steps

### 1. Final Verification
```bash
cd /workspace/runtimo

# Clean build
cargo clean
cargo build --workspace

# Run all tests
cargo test --workspace

# Generate docs
cargo doc -p runtimo-core --no-deps

# Verify examples compile
cargo build --examples -p runtimo-core
```

### 2. Update Cargo.toml (if needed)
```toml
[package]
name = "runtimo-core"
version = "0.1.0"  # Change from 0.1.0-alpha if releasing stable
edition = "2021"
license = "MIT"
authors = ["Moe"]
description = "Agent-centric capability runtime for persistent machines"
repository = "https://github.com/your-org/runtimo"
documentation = "https://docs.rs/runtimo-core"
readme = "README.md"
keywords = ["capability", "runtime", "telemetry", "agent", "sandbox"]
categories = ["development-tools", "system"]
```

### 3. Publish to crates.io
```bash
# Login (first time only)
cargo login <your-api-token>

# Dry run (verify everything)
cargo publish -p runtimo-core --dry-run

# Publish
cargo publish -p runtimo-core
```

### 4. Verify Publication
- Check crates.io: https://crates.io/crates/runtimo-core
- Check docs.rs: https://docs.rs/runtimo-core/0.1.0
- Wait ~5 minutes for docs.rs to build

### 5. Git Tag
```bash
git tag -a v0.1.0-alpha -m "Runtimo v0.1.0-alpha: Initial release

Features:
- Capability execution engine
- Two-layer telemetry (hardware + process)
- Resource guards via llmosafe
- Write-ahead log for crash recovery
- Backup/undo support
- CLI tool (moe)

Known limitations:
- Daemon is placeholder
- No process kill capability
- Timeout enforcement deferred
- Backup cleanup is stub"

git push origin v0.1.0-alpha
```

### 6. GitHub Release
1. Go to https://github.com/your-org/runtimo/releases
2. Create new release
3. Tag version: v0.1.0-alpha
4. Title: Runtimo v0.1.0-alpha
5. Description: Copy from CHANGELOG.md
6. Mark as pre-release
7. Publish

## Post-Publish

### Verify Installation
```bash
# Test fresh installation
cargo install runtimo-core

# Verify CLI works
moe list
moe telemetry
```

### Update Badges (README.md)
```markdown
[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
```

### Announce
- [ ] Post to Rust subreddit
- [ ] Post to Rust Discord
- [ ] Share on Twitter/LinkedIn
- [ ] Notify team members
- [ ] Update project homepage

## Next Release (v0.2.0)

Planned features:
- [ ] Process kill capability
- [ ] ShellExec capability with sandboxing
- [ ] HTTP request capability
- [ ] Concurrent job execution
- [ ] Daemon JSON-RPC server
- [ ] True timeout enforcement
- [ ] Process lineage tracking
- [ ] Alerting on anomalies

## Contact

- **Repository:** https://github.com/your-org/runtimo
- **Issues:** https://github.com/your-org/runtimo/issues
- **Documentation:** https://docs.rs/runtimo-core
- **Discussions:** https://github.com/your-org/runtimo/discussions

## License

MIT License - See LICENSE file for details.

---

**Publish Date:** 2026-05-16  
**Published By:** Moe  
**Approved By:** [Pending]  
**Status:** Ready for publication
