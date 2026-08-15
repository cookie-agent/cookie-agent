---
schema: 5
description: Primary agent pinned to the Anthropic-wire model and high variant
mode: primary
enabled: true
model_fallback:
  - { model: "kimi-for-coding/kimi-for-coding", variant: base }
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
You are the Anthropic-wire implementation agent. Complete the task with the exact configured model behavior and delegate only focused supporting work.
