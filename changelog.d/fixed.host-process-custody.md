- **Compiler and runtime artifacts survive cancellation** (`adk-code`): host
  compiler, check/build, and binary phases retain inputs and outputs until owned
  processes and stdio settle. Original work deadlines and error priorities stay
  unchanged. Shutdown uses a separate five-second cooperative cleanup return
  budget; incomplete cleanup retains resources in the active Linux Tokio runtime.
  Local container CLI cleanup does not certify daemon-container termination.
