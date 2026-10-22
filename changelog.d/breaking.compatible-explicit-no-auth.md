- **Explicit no-Authorization policy** (`adk-model`): use `OpenAICompatible::new_no_auth`
  for an explicitly authorized endpoint without bearer authentication. It ignores the
  config key and rejects Authorization supplied by defaults, adapters or URL userinfo.
  Authenticated constructors now reject empty, blank or invalid header credentials
  before HTTP instead of silently making anonymous requests. Existing config fields
  and serde remain unchanged; Azure keeps API-key authentication through the explicit policy.
