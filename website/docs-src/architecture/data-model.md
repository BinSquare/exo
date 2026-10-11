---
title: Data model
description: Agents, threads, sessions, turns, events, and artifacts.
---

# Data model

## Agent

A configuration object containing instructions, tools, MCP servers,
resources, and defaults for the harness and model. Threads select their
environment and vaults, and can override the agent's defaults.

Creating an agent saves its definition and allocates an identity. Nothing
needs to be running yet. Updating an agent changes its defaults; saved
thread selections still take precedence. See
[Configuration overrides](../agent-definition#configuration-overrides)
for the planned general override schema.

## Thread, session, turn

- A **thread** contains an agent's conversation history, execution state,
  filesystem state, and configuration. It can stop and resume, and may
  accumulate millions of interactions over a very long time. The runtime
  exposes it through the `ThreadHandle` trait.
- A **session** represents a client's interaction with an agent within a
  thread. A thread, or even a turn, can continue across multiple sessions.
- A **turn** is one user input, which may map to many LLM calls (tool
  loops, subagents) before producing a final result.

```text
Agent (identity + default configuration)
 └── Thread (history + configuration + execution state)
      ├── Sessions (client interactions)
      │    └── Turns (submitted input → model/tool work → result)
      └── Sandbox (persistent working environment)
```

### Lifecycle

Creating a thread allocates its identity and durable state. Running a turn
materializes resources and prepares the sandbox as needed. Subsequent
sessions resume that thread's history and working environment.

Exo durably queues submitted turns and serializes execution within a
thread. When execution starts,
`begin_turn(...)` records the user's input and returns a **turn handle**
the harness uses to append events and finish the turn. Head
tracking and write ordering stay inside the exoharness, so the harness can
start the model call as quickly as possible once execution begins.

The turn remains associated with the session that started it; another
client can later follow its progress. Disconnecting a client does not
delete the thread. Events remain available after the client disconnects
or the service restarts. Whether an interrupted turn can resume after a
service restart depends on the harness's [recovery implementation](harnesses#recovery).

### Forking history

A fork creates a new thread with a copy of the source thread's history.
The original thread is unchanged, and each branch can continue independently.
You can fork at the latest event or select an earlier event.

Selecting an earlier event changes the history copied into the new thread.
It does not undo commands the agent already ran, roll back an external
service, or restore the sandbox's filesystem to that moment. See
[Sandbox snapshots](sandboxes#snapshots) for filesystem restoration.

### What persists

| Operation | Conversation history | Sandbox state |
|:----------|:---------------------|:--------------|
| Disconnect a client | Preserved | Not deleted by the disconnect |
| Resume a thread | Continues the same history | Reuses its working environment |
| Fork a thread | Creates a new branch | Not automatically restored to the fork event |
| Restore a snapshot | Preserved | Restored from the snapshot |

For creating, resuming, and forking threads, see
[Agent definition](../agent-definition#threads-sessions-and-turns).
See [Sandboxes](sandboxes) for sandbox lifecycle, backends, and snapshots.

## Event

An event is an append-only record of a change to conversation state:
harness-emitted LLM inputs and outputs (formatted as
[Lingua](https://github.com/braintrustdata/lingua) messages), system
updates like session openings, tool requests and results, and
harness-defined custom types.

The exoharness stores and orders events durably; harnesses *interpret* them
to construct message history and higher-level behavior. Events use UUIDv7
ids, so ordered comparison and pagination are efficient, and they're
exposed through structured APIs — `get_events`, `get_event`,
`watch_events` — as typed cursor scans, not raw SQL or payload
archaeology.

Custom event types are allowed and should be namespaced. This is how
concepts like compaction stay out of the substrate: a harness writes a
custom event pointing at a derived context view or summary. Compaction
never needs to be a first-class exoharness concept.

Note that the full event log is not the same as the prompt: the harness
decides which events (or which summary of them) to send to the model on a
given round. The log is the durable record; the prompt is a view the
harness builds from it.

### Event types

This list is generated from the runtime's `EventData::kind` mapping when
the docs build. The [typed event definitions](https://github.com/exoharness/exo/blob/main/crates/exoharness/src/types.rs)
describe each variant's payload.

<!-- EXO_EVENT_KINDS -->

Harnesses can also emit namespaced custom events, such as
`agent_runtime.approval_requested`. HTTP clients should preserve event IDs
and reconnect to `/event/watch?after=EVENT_ID` to continue from their last
received event.

## Artifact

An artifact is an opaque, immutable, versioned set of bytes the agent can
set and retrieve. Updates flow through the event log (e.g.
`write_artifact` returns a version), and you can fetch the
latest or any specific version.
