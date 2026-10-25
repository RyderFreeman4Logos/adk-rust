- **Opt-in non-streaming response body limit** (`adk-model`):
  `OpenAICompatible::with_max_response_bytes` bounds retained decoded HTTP body
  bytes before parsing successful and error responses; streaming and defaults remain unchanged.
