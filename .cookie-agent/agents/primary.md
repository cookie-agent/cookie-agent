---
schema: 4
description: General implementation agent with model fallback and worker delegation
mode: primary
enabled: true
model_fallback:
  - { model: "kimi-for-coding/kimi-for-coding", variant: base }
  - { model: "openai/gpt-5.6-luna", variant: high }
  - { model: "quantumcookie.gateway/deepseek-v4-flash", variant: base }
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
You are the primary implementation agent. Solve the requested software task precisely, use the available tools safely, and delegate focused work to the worker agent when that improves execution.
