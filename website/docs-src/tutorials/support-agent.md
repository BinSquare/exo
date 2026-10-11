---
title: Support agent tutorial
description: Build a support agent that reproduces tickets and works with your source code.
---

# Support agent tutorial

Let’s walk through how to build a support agent, step by step: first triage a
ticket, then give it source code so it can investigate and propose a fix.

## Before starting

Make sure Exo is installed. [Installation instructions](/#installation) are still being written.

We’ll use OpenAI models with Codex for this tutorial, so we need to make sure they are authorized. Exo stores secrets in vaults,
which allow an agent to use them without seeing their values. Every thread has access to the `global` vault, so let’s populate
an OpenAI API key there. With `OPENAI_API_KEY` set:

```bash
exo vault secret create global --preset openai
```

This captures the environment variable and stores an encrypted secret. On macOS, the encryption master key is kept in Keychain.
The preset is shorthand for `create openai --token-env OPENAI_API_KEY --allow-origin https://api.openai.com`.

## 1. Ticket triage

We’ll start with a ticket triage bot that needs no integrations. Define the agent in a file called `support-analyst.md`.

```markdown
---
harness: codex
model:
  name: gpt-6.1-sol
  credential: openai
---

For each support ticket, determine whether it describes a technical issue.
If it does:

* Try to reproduce the issue.
* Clearly explain whether you were able to reproduce it.
* If reproduced, provide a minimal test case.
* When possible, suggest a temporary workaround.

You do not have any GitHub credentials, so if you need to access GitHub stuff
use public APIs (e.g. https://api.github.com/repos/{owner}/{repo}/issues/{number}).
```

Exo supports Codex, Claude Code, Pi, and custom harnesses. By default this agent runs locally in a SmolVM sandbox with unrestricted
egress, using `ghcr.io/exoharness/codex-devbox`, which includes Codex, Python, and Node.js/TypeScript. The sandbox gets a placeholder
for the `openai` credential; Exo substitutes the real key on permitted requests outside the sandbox. You can customize these
settings with an [environment](../agent-definition#environments).

Now, run the agent:

```bash
exo run support-analyst.md
```

Paste in an example case, like:

```text
Check out https://github.com/braintrustdata/autoevals/issues/223
```

The agent will investigate the issue, try to reproduce it, and provide a diagnosis.

## 2. Adding git context

Support agents get even more powerful when they can access your source code. Add the repository to the definition so each thread
starts with its own checkout. `autoevals` is public, so this doesn't require a GitHub account or credential.

```markdown
---
harness: codex
model:
  name: gpt-6.1-sol
  credential: openai
resources:
  - name: autoevals
    type: git
    url: https://github.com/braintrustdata/autoevals
    mount_path: /workspace/autoevals
---

For each support ticket, determine whether it describes a technical issue.
If it does:

* Try to reproduce the issue.
* Clearly explain whether you were able to reproduce it.
* If reproduced, provide a minimal test case.
* When possible, suggest a temporary workaround.
* Attempt to fix the underlying issue, test the fix, and provide a patch.

Use GitHub's public API to read issues.
```

Run `exo run support-analyst.md` again, then try the same prompt:

```text
Check out https://github.com/braintrustdata/autoevals/issues/223
```

### Private repositories

For a private repository, create a credential with your `gh` login:

```bash
exo vault secret create global --preset github
```

Or import a personal access token with `GITHUB_PAT` set:

```bash
exo vault secret create global --preset github --token-env GITHUB_PAT
```

Then add `credential: github` to that Git resource. Credentials aren't selected implicitly from the repository URL. See
[Vaults](../architecture/vaults) for destination policies and per-user vaults.
