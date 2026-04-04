# Claude Development Guidelines for Agent Tmux Manager

## Core Principles

### 1. Safe, Idiomatic, Panic-Free Rust

**This codebase prioritizes safety and reliability above all else.**

#### No Panics in Production Code

```rust
// ❌ NEVER use these in production code:
.unwrap()           // Panics on None/Err
.expect("msg")      // Panics with message
panic!("msg")       // Explicit panic
unreachable!()      // Panics if reached
todo!()             // Panics unconditionally
array[index]        // Panics on out-of-bounds

// ✅ ALWAYS use these instead:
.ok()               // Convert Result to Option
.unwrap_or(default) // Provide fallback value
.unwrap_or_default()// Use Default trait
.unwrap_or_else(|| compute()) // Lazy fallback
?                   // Propagate errors up
if let Some(x) = .. // Pattern match safely
.get(index)         // Returns Option for arrays/slices
```

#### Error Handling Over Panicking

```rust
// ❌ BAD: Panics on invalid input
fn get_session(id: &str) -> Session {
    self.sessions.get(id).unwrap()
}

// ✅ GOOD: Returns Result, caller decides
fn get_session(id: &str) -> Result<Session, RegistryError> {
    self.sessions.get(id)
        .cloned()
        .ok_or_else(|| RegistryError::SessionNotFound(id.to_string()))
}
```

#### Idiomatic Patterns

- **Use `?` operator** for error propagation, not `.unwrap()`
- **Use `Option`/`Result`** return types, not sentinel values
- **Use iterators** over manual indexing
- **Use `impl Trait`** for return types when appropriate
- **Use `#[must_use]`** on functions where ignoring return is likely a bug
- **Derive traits** (`Debug`, `Clone`, `Default`) liberally
- **Use newtypes** for type safety (`SessionId(String)` not `String`)

#### Allowed Exceptions

`unwrap()`/`expect()` are acceptable ONLY in:
- **Tests** (`#[cfg(test)]` modules)
- **Infallible operations** (e.g., `Regex::new` with literal that's known-valid)
- **Setup code** where failure means "cannot proceed anyway"
- **After explicit validation** with a comment explaining why it's safe

```rust
// ✅ OK: After validation, with explanation
let port: u16 = port_str.parse().expect("validated by clap");

// ✅ OK: Compile-time known-valid regex
static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid regex literal")
});
```

#### Async-Specific Safety

- **Never block the async runtime** - use `spawn_blocking` for sync work
- **Always use timeouts** on external operations
- **Handle channel closure gracefully** - receivers return `None`/`Err`
- **Use `CancellationToken`** for cooperative shutdown

---

### 2. Always Verify and Test Before Moving Forward

**Critical Lesson from Week 1 Integration Testing:**

When implementing integrations or building on external APIs/systems:

1. ✅ **Read official documentation FIRST**
   - Never assume API behavior
   - Check actual JSON schemas, not assumed ones
   - Verify event names, field names, data types

2. ✅ **Test assumptions BEFORE writing production code**
   - Create minimal test scripts
   - Validate with actual data
   - Log everything to understand actual behavior

3. ✅ **Document discrepancies immediately**
   - What was assumed vs. what's real
   - Impact on architecture
   - Required changes to plans

4. ✅ **Fix broken assumptions early**
   - Update all affected documentation
   - Correct planning documents
   - Revise architecture if needed

### Example: Week 1 Integration Validation

**What we assumed (WRONG):**
- Status Line provides `context_window` data with token counts
- `PermissionRequest` hook event exists
- Scripts should use `while read` loop

**What we found (RIGHT):**
- Status Line provides cost/duration/lines, NO token data
- `PreToolUse` event exists, NOT `PermissionRequest`
- Scripts should use `input=$(cat)` single-read pattern

**Impact:** Avoided weeks of implementation time on features that couldn't work.

**Result:** Week 1 validation saved us from building on false assumptions.

---

## Testing Philosophy

### Test Early, Test Often

- Write test scripts before production code
- Validate integrations in isolation first
- Use real data, not mock data, for integration tests
- Keep test logs to understand actual behavior

### Documentation is Truth

- If official docs conflict with assumptions, **docs win**
- Update plans immediately when assumptions proven wrong
- Maintain a findings document (like `CLAUDE_CODE_INTEGRATION.md`)

### Fail Fast, Fix Fast

- Better to discover issues in Day 1 than Week 3
- Week of validation > Weeks of rework
- Architecture changes are cheaper during planning

---

## Week 1 Validation Template

For any external integration:

1. **Day 1: Integration Validation**
   - Set up test environment
   - Create minimal test scripts
   - Validate data structures match assumptions
   - Document actual behavior

2. **Day 2: Architecture Adjustment**
   - Update plans based on findings
   - Revise data models
   - Correct protocol specifications
   - Update resource limits if needed

3. **Day 3: Final Specifications**
   - Complete architecture docs
   - Finalize domain model
   - Lock in protocol design

**Only after validation complete → Proceed to implementation**

---

## Status: Lessons Learned

✅ Week 1 validation completed
✅ Critical assumptions corrected
✅ Architecture updated to match reality
✅ Ready for Phase 1 implementation with confidence

**Confidence Level:** 🟢 HIGH (because we validated first)

---

## Compaction

Write a summary prompt that loads sufficient context and kick starts the next stage. No more than 3 sentences before I clear this context.
