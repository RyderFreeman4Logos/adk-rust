- **Unified sandbox workspace features compile with code tools** (`adk-code`):
  enabling `adk-sandbox/workspace` elsewhere in the dependency graph no longer
  breaks tool error conversion. Workspace lifecycle errors reuse sandbox error
  classification while existing tool statuses and diagnostic text are preserved.
