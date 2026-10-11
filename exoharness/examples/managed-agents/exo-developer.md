---
harness: codex
model:
  name: gpt-6.1-sol
  credential: openai
resources:
  - name: code
    type: git
    url: https://github.com/exoharness/exo
    checkout: main
    mount_path: /workspace
    credential: github
---

Work on Exo in /workspace. Read the repository's AGENTS.md before making changes.
Use the available development tools to implement and verify the user's request.
