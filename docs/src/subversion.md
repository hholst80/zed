---
title: Subversion
description: Use an SVN working copy from Zed's Source Control panel.
---

# Subversion {#subversion}

Zed's experimental Subversion extension connects the Source Control panel to a
system SVN client. It uses SVN concepts directly. Checkboxes select paths for
the next commit; they do not create a Git-style staging area or an SVN
changelist.

This feature is currently available from this source branch as a development
extension. It is not published in the Zed extension registry.

## Prerequisites {#subversion-prerequisites}

- Use a Dev or Nightly build from the Subversion feature branch.
- Install SVN 1.14 or newer in the 1.x series and make `svn` available on
  `PATH`.
- For a remote project, install `svn` on the remote host. Commands run where the
  working copy lives.

Install the extension:

1. Open the Extensions page.
2. Run {#action zed::InstallDevExtension}.
3. Select the `extensions/svn` directory in the Zed checkout.
4. Open an SVN working copy, or a descendant directory inside one.

Zed detects the applicable working-copy root with `svn info`. Use the
repository selector in the Source Control panel if the project also contains a
Git repository. Git remains selected by default when both providers claim the
same root.

## Working with changes {#subversion-changes}

The panel groups changes as Conflicts, Included Changes, Excluded Changes,
Unversioned, and Missing.

- Tracked changes and property-only changes are included in the next commit by
  default.
- Clear a checkbox to exclude a tracked path from the next commit.
- Add an unversioned path before selecting it for commit.
- Schedule a missing path for deletion before selecting it for commit.
- Mark a conflict resolved only after editing the working file to the desired
  result.
- Revert discards local text and property changes after a confirmation.
- Ignore adds a pattern to the containing directory's `svn:ignore` property.

Enter a commit message, select the paths to include, and choose **Commit**. Zed
passes an explicit target list to SVN. It does not commit a nested external as
part of its parent working copy.

Choose **Diff** on a row to inspect SVN's native text and property patch.
Property-only changes appear in this output. Binary files show the diagnostic
reported by SVN instead of a text patch.

## Repository operations {#subversion-repository-operations}

- {#action source_control::Refresh} reloads repository metadata and local
  status.
- {#action source_control::Update} runs an update with externals ignored and
  conflict acceptance set to `postpone`.
- {#action source_control::Switch} switches the working-copy root to a URL.
- {#action source_control::Checkout} checks out a URL into a selected directory
  and opens that directory as a worktree.

History and blame are available for changed versioned files. Historical text
uses numeric SVN revisions.

## Credentials and certificate trust {#subversion-authentication}

SVN first runs non-interactively. If a mutation needs credentials, Zed prompts
once and retries that operation once. The password is sent to SVN through
standard input with auth caching disabled. It is not placed in command
arguments or saved in settings.

If TLS certificate verification fails, Zed shows the realm, fingerprint, and
reported failures. **Trust Once** applies only to the retry and does not create
permanent trust.

## Current limitations {#subversion-limitations}

- The extension API is experimental and only enabled for Dev and Nightly builds
  in this branch.
- Editor gutter decorations still use Zed's Git diff pipeline. Use the row's
  **Diff** action for SVN text and property changes.
- A checkout currently starts from an active SVN repository in the Source
  Control panel.
- History and blame use credentials already available to SVN. The one-shot Zed
  credential prompt currently applies to mutations, not these online reads.
- Ignore edits the containing directory's local `svn:ignore` property. Zed does
  not yet identify inherited, runtime, or global ignore-rule provenance.
- A nested working copy or external appears independently only when its
  directory is also open as a Zed worktree. Parent commit and update operations
  still exclude externals.
- Long-running operations do not yet report incremental progress in the panel.
- Collaboration guests can inspect source-control data but cannot run
  mutations.
- SVN changelists, locks, branch enumeration, merge, relocate, cleanup,
  working-copy upgrade, and arbitrary property editing are not included.

## Troubleshooting {#subversion-troubleshooting}

If no repository appears, run these commands in the same environment as Zed:

```sh
svn --version --quiet
svn info --xml --non-interactive -- .
```

The first command must report SVN 1.14 or newer in the 1.x series. For a remote
project, run both commands on the remote host. Check Zed.log with
{#action zed::OpenLog} for an extension load or command error.
