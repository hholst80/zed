---
title: Source Control Provider Extensions
description: Implement an experimental source-control provider for Zed.
---

# Source Control Provider Extensions {#source-control-provider-extensions}

The experimental source-control extension API lets an extension provide
repository discovery, status, revisions, diffs, history, blame, and mutations
without implementing Git-specific behavior. It is available in extension API
0.8 on Dev and Nightly builds from the Subversion feature branch.

The host owns the provider-neutral model and Source Control UI. An extension
adapts that model to a concrete tool such as SVN and reports its supported
capabilities.

## Manifest {#source-control-provider-manifest}

Declare each provider by a stable lowercase identifier:

```toml
[source_control_providers.subversion]
display_name = "Subversion"

[[capabilities]]
kind = "process:exec"
command = "svn"
args = ["**"]
```

A source-control provider is eligible for remote loading. Its executable must
be available on the machine that hosts the worktree.

## Provider contract {#source-control-provider-contract}

Implement the `source_control_*` methods on `zed_extension_api::Extension`:

- `source_control_discover`
- `source_control_status`
- `source_control_load_text`
- `source_control_diff`
- `source_control_history`
- `source_control_blame`
- `source_control_execute`

Discovery returns opaque repository IDs, working-copy paths, display metadata,
and a capability record. All paths in status and operation records are
normalized, slash-separated, and relative to the working-copy root. Unsupported
operations return `UnsupportedOperation`; they must not behave as no-ops.

The host can preserve overlapping repositories from multiple providers. Git is
selected by default when Git and an extension provider claim the same root.

## Running commands {#source-control-provider-commands}

Use `Worktree::run_command` with `ScopedCommand` for provider processes. The
host checks the extension's process capability, constrains the working
directory, writes optional standard input, terminates the child when the request
is cancelled, and rejects oversized input and output.

Pass arguments as a vector. Do not invoke a shell. Mark secret-bearing standard
input as redacted. Keep paths normalized and use the concrete tool's end-of-
options delimiter where available.

Return `AuthenticationRequired` or `TrustRequired` when host interaction is
needed. A retry receives `SourceControlExecutionOptions`. Do not include
passwords in provider errors, diagnostics, command arguments, or persisted
state.

## Remote and collaboration behavior {#source-control-provider-remote}

For remote development, provider requests run through the project protocol and
execute beside the remote worktree. The UI receives structured snapshots and
sanitized errors. Collaboration guests are read-only; mutation requests are
rejected by both the guest and host sides.

This API is not stable or published yet. Discuss the provider model and security
boundary with Zed maintainers before depending on it outside this branch.
