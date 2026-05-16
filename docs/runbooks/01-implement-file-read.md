# Runbook: Implement FileRead Capability

**Purpose:** Add the first working capability (`FileRead`) to Runtimo  
**Last Verified:** Not yet run (planned)  
**Time to Complete:** ~30 minutes

## Prerequisites

- [ ] Rust toolchain installed (`rustc --version`)
- [ ] Workspace builds: `cd /workspace/runtimo && cargo build`
- [ ] Text editor (VS Code, vim, etc.)

## Steps

### Step 1: Create capability file

```bash
cd /workspace/runtimo/core/src
mkdir -p capabilities
touch capabilities/mod.rs
touch capabilities/file_read.rs
```

**Expected:** New directory `core/src/capabilities/` with two empty files

### Step 2: Implement FileRead capability

**File:** `core/src/capabilities/file_read.rs`

```rust
use crate::capability::{Capability, Context, Output};
use crate::Result;
use serde_json::json;

pub struct FileRead;

impl FileRead {
    pub fn new() -> Self { Self }
}

impl Capability for FileRead {
    fn name(&self) -> &'static str {
        "FileRead"
    }
    
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                }
            },
            "required": ["path"]
        })
    }
    
    fn validate(&self, args: &serde_json::Value) -> Result<()> {
        // Validate path exists in args
        if !args.get("path").is_some() {
            return Err(crate::Error::SchemaValidationFailed(
                "Missing required field: path".to_string()
            ));
        }
        Ok(())
    }
    
    fn execute(&self, args: &serde_json::Value, _ctx: &Context) -> Result<Output> {
        let path_str = args["path"].as_str()
            .ok_or_else(|| crate::Error::ExecutionFailed("Invalid path".to_string()))?;
        
        let path = std::path::Path::new(path_str);
        
        if !path.exists() {
            return Err(crate::Error::ExecutionFailed(
                format!("File not found: {}", path_str)
            ));
        }
        
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))?;
        
        Ok(Output {
            success: true,
            data: json!({ "content": content }),
            message: Some(format!("Read {} bytes", content.len())),
        })
    }
}
```

**Expected:** File compiles with no errors

### Step 3: Export capability from module

**File:** `core/src/capabilities/mod.rs`

```rust
mod file_read;

pub use file_read::FileRead;
```

### Step 4: Register capability in daemon

**File:** `daemon/src/main.rs`

```rust
use runtimo_core::capability::{CapabilityRegistry, FileRead};

fn main() {
    let mut registry = CapabilityRegistry::new();
    registry.register(FileRead::new());
    
    println!("Registered capabilities: {:?}", registry.list());
    // Expected: ["FileRead"]
}
```

### Step 5: Verify implementation

```bash
cd /workspace/runtimo
cargo build
# Expected: 0 errors

cargo run --bin runtimo
# Expected: "Registered capabilities: [\"FileRead\"]"
```

## Verification

```bash
cd /workspace/runtimo
cargo test -p runtimo-core -- file_read
# Expected: test_file_read passes
```

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `module not found` | Missing `mod file_read;` in mod.rs | Add `mod file_read;` |
| `trait method not implemented` | Missing impl block | Implement all 4 trait methods |
| `cannot find type 'Output'` | Missing import | `use crate::capability::Output;` |

## Rollback

If this fails, revert:
```bash
cd /workspace/runtimo
git checkout -- core/src/capabilities/
git checkout -- daemon/src/main.rs
```

## Next Steps

After FileRead works:
1. Implement FileWrite (with backup)
2. Add CLI command: `moe run FileRead --args '{"path":"/tmp/test.txt"}'`
3. Add WAL logging for FileRead execution

---

**Runbook Source:** Based on capability trait design (`core/src/capability.rs:17-27`)  
**Last Updated:** 2026-05-15 (planned)  
**Next Review:** After FileRead implementation
