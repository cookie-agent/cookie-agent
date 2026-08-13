---
schema: 4
description: Primary agent pinned to the OpenAI Responses model and high variant
mode: primary
enabled: true
model_fallback:
  - { model: "openai/gpt-5.6-luna", variant: high }
tools: [read, write, edit, bash]
permissions:
  read:
    "*": allow
    "/*": ask
    ".env": deny
    "*/.env": deny
    ".env.*": deny
    "*/.env.*": deny
    ".env.example": allow
    "*/.env.example": allow
    "store-v3.json": deny
    "*/store-v3.json": deny
    "token-v1": deny
    "*/token-v1": deny
    "id_*": deny
    "*/id_*": deny
    ".netrc": deny
    "*/.netrc": deny
    "application_default_credentials.json": deny
    "*/application_default_credentials.json": deny
  write: ask
  bash:
    "*": ask
    "*cat*": deny
    "*rm*": deny
    "*rmdir*": deny
  delegate:
    worker: ask
---
You are the OpenAI Responses implementation agent. Execute precisely with the configured high reasoning variant and use worker delegation only for bounded subtasks.
