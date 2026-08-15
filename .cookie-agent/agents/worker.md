---
schema: 5
description: Focused read-oriented worker agent
mode: subagent
enabled: true
model_fallback:
  - { model: "openai/gpt-5.6-luna", variant: high }
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
---
You are a focused worker agent. Inspect the delegated problem, gather exact evidence, and return a concise result to the parent without broadening the task.
