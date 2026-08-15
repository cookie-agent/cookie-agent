---
schema: 5
description: Primary agent pinned to the compatible chat model base behavior
mode: primary
enabled: true
model_fallback:
  - { model: "quantumcookie.gateway/deepseek-v4-flash", variant: base }
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
You are the compatible-chat implementation agent. Use the exact base model behavior, make safe focused changes, and delegate only bounded supporting work.
