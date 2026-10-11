---
title: Harnesses
description: How harnesses use Exo's infrastructure, execute turns, and recover.
---

# Harnesses

The key insight behind Exo is that a harness's semantics can be separated
from the infrastructure it runs on. The harness decides how to call the
model, build context, compact history, plan, and use tools. The runtime
provides durable state, sandboxes, credentials, and network controls.

This lets you use Codex, Claude Code, Pi, or a custom harness behind the
same API, without rebuilding that infrastructure for each one.

![Exo's runtime, harnesses, and sandbox](/images/runtime.svg)

## The exoharness

The durable infrastructure layer. It owns identity (agents, threads,
turns), history (the append-only event log), artifacts, vaults, and sandbox
management. It stores and orders events, but doesn't decide which events
to put in the prompt or when to call the model.

Keeping those decisions out of the exoharness means you can change the
harness without changing the thread's durable record.

## The harness

The semantic layer. It assembles prompts, calls models, dispatches tools,
and decides how to manage context. Exo's bundled harnesses adapt the labs'
native runtimes to this contract; a custom harness implements it directly.

The durable thread state lives in the exoharness. A harness can be replaced
or upgraded while retaining that state. Recovery of an interrupted turn
still depends on the harness's implementation.

## Bundled harnesses

Select a harness in the agent definition, or with `--harness` when opening
or resuming a thread:

| Harness | What it is |
|:--------|:-----------|
| `codex` | OpenAI Codex backed by durable Exo threads |
| `claude-code` | Claude Code backed by durable Exo threads |
| `pi` | Pi backed by durable Exo threads |

Codex, Claude Code, and Pi use Exo events as the canonical conversation
state and run their native agent runtimes inside managed sandboxes. You
can continue a thread across sessions, or select a different harness while
retaining the conversation history.

## The execution loop

Exo durably queues submitted turns. Once a turn begins executing, the
harness can:

1. Read or derive prompt history from events.
2. Call the model.
3. Append messages and tool requests through the turn handle.
4. Execute tools and append results through the turn handle.
5. Finish the turn.

Which slice of history to send, which model to call, which tools to expose,
and when to compact all belong to the harness. The exoharness owns the
durable record and write ordering.

## Custom harnesses

A TypeScript harness module owns its turn loop. For example:

```markdown
---
harness: ./my-harness.ts
model:
  name: gpt-6.1-sol
  credential: openai
---

Help the user with their task.
```

The [Custom harness tutorial](../tutorials/write-your-own-agent) walks
through the interface, tool registration, and context building. Module
paths resolve relative to the agent file and must be available on the
runtime host.

## Recovery

If the service restarts during a turn, Codex first reconnects to a live
native turn. If the native process is gone, it resumes the saved Codex
thread or starts a new thread with the Exo conversation history. Recovery
fails when a tool call's outcome cannot be established, including a native
tool still in progress when Exo reconnects.

A custom TypeScript harness can implement `resumeTurn` and declare
`reconcileUnresolvedToolCalls: true` if it can establish the outcome of
unresolved calls. Claude Code and Pi currently end an interrupted turn as
failed; you can start a new turn on the same thread afterward.

## Tools

A **tool** is a function the model calls during an active turn. The harness
exposes its definition to the model and records the request and result in
the thread's durable history.

Tools are a harness concept. The exoharness supplies durable events,
artifacts, secrets, and sandbox processes; the harness decides which tools
the model sees and how to dispatch their calls.

A definition describes a tool's name, purpose, and input schema. Its
implementation can run in several places:

| Implementation | Where it runs |
|:---------------|:--------------|
| TypeScript tool module | The runtime host; its handler can execute commands in the thread's sandbox |
| Client tool | The connected application, e.g. a UI asking the user a question |
| MCP tool | The MCP server |

For declarations and configuration, see [Custom tools](../agent-definition#custom-tools)
and [Tool implementations](../agent-definition#tool-implementations).

### Trust model

Harness modules and TypeScript tool modules run as trusted code in the
harness process. Loading one is a trust decision. Commands that a handler
starts inside the thread's sandbox still use that sandbox's policy, but
there is not a capability sandbox around the module itself.

Sandbox network policies constrain sandbox processes. They do not constrain
arbitrary host-side code in a module or the client application's own
network access.

Credentials belong in vaults. Tool configuration can refer to a credential;
definitions, prompts, and results should not contain its raw value. See
[Vaults](vaults).
