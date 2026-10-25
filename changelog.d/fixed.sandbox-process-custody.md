- **Subprocess cleanup remains owned after cancellation** (`adk-sandbox`):
  process execution and local workspace commands retain owned resources until
  inline stdio has closed, the child has actually been waited, and its original
  process group is absent. The new
  `SandboxBackend::execute_with_keepalive` retains caller artifacts;
  `ProcessBackend` transfers them into physical process custody. Shutdown uses
  one five-second cooperative cleanup return budget; incomplete cleanup keeps
  the physical owner and resources in the active Linux Tokio runtime.
