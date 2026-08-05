---
schema: 1
description: Primary agent pinned to the compatible chat model base behavior
mode: primary
enabled: true
model_fallback:
  - { model: "quantumcookie.gateway/deepseek-v4-flash", variant: base }
tools: [read, grep, glob, write, edit, bash]
permissions:
  - { id: allow-workspace-read, action: read, resource: "*", effect: allow }
  - { id: allow-workspace-search, action: grep, resource: "*", effect: allow }
  - { id: allow-workspace-glob, action: glob, resource: "*", effect: allow }
  - { id: ask-write, action: write, resource: "*", effect: ask }
  - { id: ask-bash, action: bash, resource: "*", effect: ask }
  - { id: ask-delegate, action: delegate, resource: "*", effect: ask }
  - { id: ask-external-directory, action: external_directory, resource: "*", effect: ask }
  - { id: deny-read-root-dotenv, action: read, resource: ".env", effect: deny }
  - { id: deny-read-nested-dotenv, action: read, resource: "*/.env", effect: deny }
  - { id: deny-read-root-dotenv-variants, action: read, resource: ".env.*", effect: deny }
  - { id: deny-read-nested-dotenv-variants, action: read, resource: "*/.env.*", effect: deny }
  - { id: allow-read-root-dotenv-example, action: read, resource: ".env.example", effect: allow }
  - { id: allow-read-nested-dotenv-example, action: read, resource: "*/.env.example", effect: allow }
  - { id: deny-read-root-credential-store, action: read, resource: "store-v2.json", effect: deny }
  - { id: deny-read-nested-credential-store, action: read, resource: "*/store-v2.json", effect: deny }
  - { id: deny-read-root-daemon-token, action: read, resource: "token-v1", effect: deny }
  - { id: deny-read-nested-daemon-token, action: read, resource: "*/token-v1", effect: deny }
  - { id: deny-read-root-private-keys, action: read, resource: "id_*", effect: deny }
  - { id: deny-read-nested-private-keys, action: read, resource: "*/id_*", effect: deny }
  - { id: deny-read-root-netrc, action: read, resource: ".netrc", effect: deny }
  - { id: deny-read-nested-netrc, action: read, resource: "*/.netrc", effect: deny }
  - { id: deny-read-root-cloud-credentials, action: read, resource: "application_default_credentials.json", effect: deny }
  - { id: deny-read-nested-cloud-credentials, action: read, resource: "*/application_default_credentials.json", effect: deny }
  - { id: deny-workspace-search-enumeration, action: grep, resource: "*", effect: deny }
  - { id: deny-workspace-glob-enumeration, action: glob, resource: "*", effect: deny }
delegation:
  agents: [worker]
  max_depth: 3
---
You are the compatible-chat implementation agent. Use the exact base model behavior, make safe focused changes, and delegate only bounded supporting work.
