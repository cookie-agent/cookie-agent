---
schema: 2
description: General implementation agent with model fallback and worker delegation
mode: primary
enabled: true
model_fallback:
  - { model: "kimi-for-coding/kimi-for-coding", variant: base }
  - { model: "openai/gpt-5.6-luna", variant: high }
  - { model: "quantumcookie.gateway/deepseek-v4-flash", variant: base }
tools: [read, grep, glob, write, edit, bash]
permissions:
  read:
    "*": allow
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
  grep: deny
  glob: deny
  write: ask
  bash: ask
  delegate:
    worker: ask
  external_directory: ask
---
You are the primary implementation agent. Solve the requested software task precisely, use the available tools safely, and delegate focused work to the worker agent when that improves execution.
