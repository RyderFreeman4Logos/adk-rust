- Retain allowlisted raw OpenAI-compatible response observations under
  `LlmResponse.provider_metadata.openai`, including IDs, known metrics and prompt/
  choice token IDs. Streaming responses retain observations through their existing
  finish/usage-trailer delivery point; later metadata does not delay completion or
  emit another final. Partial content/control flags are unchanged.
  Missing, null, empty and malformed observations are not normalized into counts.
  Provider support, live availability and consumer adoption remain separate.
