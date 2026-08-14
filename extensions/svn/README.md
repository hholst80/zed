# Subversion extension

This in-tree extension provides the Subversion adapter for Zed's experimental
source-control provider API. It requires a system `svn` client at version 1.14
or newer in the 1.x series and a Dev or Nightly build of this branch.

Install it as a development extension by running the
`zed: install dev extension` action and selecting this directory. For remote
projects, `svn` must be available on the remote host because commands run next
to the working copy.

The MVP supports working-copy discovery and status, text and property diffs,
selected-path commits, add, schedule delete, revert, resolve, ignore, update,
switch, checkout, history, blame, base text, one-shot credential prompts, and
one-shot TLS certificate trust.

Run its native and WebAssembly checks from the Zed repository root:

```sh
cargo test -p zed_svn
cargo build -p zed_svn --target wasm32-wasip2
```

The native tests create disposable repositories with `svnadmin`. They also
exercise descendant project roots and an authenticated `svnserve` fixture when
the required executables are installed.
