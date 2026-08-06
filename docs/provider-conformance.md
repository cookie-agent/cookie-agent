# Family Registry 1

Managed models.dev providers are classified by catalog `npm`, not provider ID.
There is no provider review table or executable metadata comparison. The catalog
is authoritative for endpoints, nested model overrides, shapes, capabilities,
limits, modalities, and environment aliases. New providers using a known npm
family therefore need no code change.

## Families

| Family | npm values | Adapter/default | Auth |
|---|---|---|---|
| OpenAI-compatible chat | `@ai-sdk/openai-compatible`, Groq, Mistral, xAI, Cerebras, Together, DeepInfra, Perplexity, Venice, OpenRouter, QVAC | provider-scoped `OpenAiCompatibleChatModel`; catalog API or package default | bearer |
| Anthropic | `@ai-sdk/anthropic` | `AnthropicCompatibleModel`; `https://api.anthropic.com/v1` | `x-api-key` or bearer |
| OpenAI | `@ai-sdk/openai` | Responses by default, Chat by shape; `https://api.openai.com/v1` | bearer |
| Google | `@ai-sdk/google` | `GoogleModel`; Gemini v1beta | `x-goog-api-key` |
| Vertex | `@ai-sdk/google-vertex` | `GoogleVertexModel`; project/location publisher endpoint | access token |
| Vertex Anthropic | `@ai-sdk/google-vertex/anthropic` | Vertex Anthropic publisher resource | access token |
| Bedrock | `@ai-sdk/amazon-bedrock`, nested `@ai-sdk/amazon-bedrock/mantle` | Converse or nested Responses; regional endpoint | AWS static credentials for Converse; `AWS_BEARER_TOKEN_BEDROCK` bearer API key for Mantle Responses |
| Azure | `@ai-sdk/azure` | Chat or Responses, V1 route | `api-key` or bearer |
| Cohere | `@ai-sdk/cohere` | `CohereModel`; `https://api.cohere.com/v2/chat` | bearer |

Unknown npm values use `no_known_protocol_family`; an unknown nested npm affects
only that model.

## Catalog derivation

`tool_call`, `structured_output`, `temperature`, `reasoning`,
`reasoning_options`, `modalities.input`, `limit.context`, and `limit.output`
derive runtime capabilities and settings. Deprecated and non-text-output models
are omitted. `interleaved.field` selects compatible reasoning output handling.

Nested `provider { npm, api, shape }` overrides family, endpoint, and shape for
that model. `responses` selects Responses and `completions` selects Chat.
Authored `shape = "chat" | "responses"` is available at provider and model scope.

`${VAR}` endpoint placeholders generate required setup fields and are substituted
before endpoint validation. Fields containing `KEY`, `TOKEN`, or `SECRET` are
unsafe-to-project; other derived fields are public. Known aliases include
`AWS_REGION -> region`, `AZURE_RESOURCE_NAME -> resource_name`, and Vertex
project/location.

Azure nested Anthropic models follow their catalog npm classification:
`@ai-sdk/anthropic` sends API keys as `x-api-key`, which Microsoft Foundry
accepts. Entra access tokens use Authorization bearer instead. Bedrock Mantle
Responses uses the OpenAI-compatible endpoint with a Bedrock API key in
Authorization bearer form.

Provider store schema 3 persists family and adapter identity. Earlier stores are
rejected. Current Oven pins are core/OpenAI/Google/Vertex `0.5.0`, Anthropic
`0.6.0`, Bedrock/Azure `0.4.0`, and Cohere/Open Responses `0.3.0`.
