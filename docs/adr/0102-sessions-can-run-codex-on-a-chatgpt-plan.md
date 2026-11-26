# 0102: Sessions can run Codex on a ChatGPT plan

2026-11-26 · accepted

## Context

[ADR 0002](0002-subscription-only.md) keeps model API billing out of fctl.
People with a ChatGPT plan can authorize eligible inference through
[Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source).

## Decision

- An instance someone runs enables Codex with `FCTL_CODEX_ENABLED=1`. fctl
  uses the open-source OAuth flow. A paid or hosted service needs its own
  authorization from OpenAI.
- A session keeps its agent for its lifetime. A manual run takes the
  project's default or an explicit choice. A schedule, review or autopilot
  takes the operator's choice and uses their connection. An absent choice
  means Claude. Hosts and agents advertise the `codex` feature before a
  Codex session reaches them.
- The control plane holds encrypted OAuth credentials. It refreshes one
  set of rotating tokens at a time. An assigned runner gets an access
  token, and the supervisor uses it for a fixed Responses API endpoint.
  Codex App Server gets a local relay and no billing key. Snapshots carry
  only the thread history and work.
- OpenAI enforces plan and app limits. fctl pauses on its usage limit and
  keeps the connection from starting more work until the reset or a
  person's retry. People or their workspace admins disable additional
  credits in ChatGPT for plan-only usage.

## Consequences

- Claude and Codex use fctl's transcript, messages, pull requests, memory
  and retrospectives. Claude Remote Control stays specific to Claude.
- Codex needs a current host, fctl-agent and a sandbox image with Codex CLI.
