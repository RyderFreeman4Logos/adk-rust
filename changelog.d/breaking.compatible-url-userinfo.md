- OpenAI-compatible and Azure clients reject target URL userinfo before HTTP with
  nonretryable `InvalidInput` code `model.openai_compat.url_userinfo_forbidden`.
  Callers that relied on implicit Basic plus configured Bearer now receive an
  explicit error; remove URL credentials and configure the intended authentication
  header. Empty userinfo normalized away by URL parsing remains accepted. Valid
  bearer, explicit NoAuth, Azure API-key and adapter header replacement are unchanged.
