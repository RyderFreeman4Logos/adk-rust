- **Response schemas are preserved instead of silently narrowed** (`adk-model`):
  OpenAI-compatible clients default to local strict admission of explicitly closed,
  all-required schemas and reject unsupported shapes before HTTP. Call
  `with_response_schema_strict(false)` to send broader schemas unchanged with
  `strict: false` to a supporting provider. Extensions and request adapters cannot
  replace a configured response format; tool-schema normalization is unchanged.
