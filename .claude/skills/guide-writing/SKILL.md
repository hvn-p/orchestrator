---
name: guide-writing
description: Use before writing, updating or reviewing the orchestrator guide (`plugins/orchestrator/`); when a change to orchestrator's commands, options, output, messages, events, files, configuration or behaviour may leave the guide wrong; when the `guide` CI check asks for an update or a `Guide: unchanged` statement; or when asked to "update the guide", "write the guide", "mets à jour le guide", "relis le guide". It says who the guide is for and what goes in it.
---

# Writing the orchestrator guide

The guide, the `orchestrator` plugin's skill under `plugins/orchestrator/`, is
a user manual. Left alone it drifts toward a design document or a developer
notebook; these rules keep it a manual.

## Who reads it

The person who uses orchestrator, and the AI working for them in a Claude Code
session. Neither is developing orchestrator. They come with questions such as:

- "I expected orchestrator to do this; why doesn't it?"
- "What does this message, this event, this file mean?"
- "How does it work underneath? Simple commands hide it."
- "What can I change so it fits my machine and how I work?"

An AI also reads it before doing anything with orchestrator for the user.

Never write for a contributor: no module names, code layout, design history,
rejected options, or how to build and test orchestrator, unless the user needs
it to install or use the tool.

## What goes in

1. **What orchestrator does**: only what exists in the code the guide ships
   with. Never what is planned, designed or possible later, not even marked as
   such. The guide says that anything it does not describe, orchestrator does
   not do.
2. **How it works**: the mechanism behind each part, enough for a user to
   predict its behaviour and explain a surprise.
3. **Every command and option, file and directory, configuration field, event
   and message** a user can meet: what it does, what it means, its default.
4. **What the user can customise**, and the effect of each setting.
5. **Troubleshooting by symptom**, starting from what the user observes.

## What stays out

- Plans, open questions, design decisions, measurements kept for
  developers.
- Personal configuration practices: how one user organises their files is
  theirs. State the tool's behaviour as plain facts; never recommend a
  workflow.
- Anything not verified.

## How to write it

- Check every fact against the code, `--help` or a real run on the current
  branch. When unsure, verify; never write from memory.
- Keep the guide's `SKILL.md` short: what orchestrator does, how it works, how
  to answer, and links to `reference/` files, one file per kind of thing.
- The guide skill's `description` says first when to use it, then at most one
  sentence on what it is.
- English, plain sentences, no marketing, no em dash.

## On every pull request

Decide whether the change alters anything a user can see or rely on: a
command, option, output, message, event, file, configuration field, default,
limit or behaviour.

- **Yes**: update the guide in the same pull request.
- **No** (a refactor with no visible effect, tests, CI): write
  `Guide: unchanged, <reason>` in the pull request's description, the reason
  saying why nothing visible changed. The `guide` CI check requires one or the
  other.
