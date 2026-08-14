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

## Containerized build

Build Zed, its CLI, and the SVN extension without installing Zed's development
packages on the host. Docker and Podman are both supported and auto-detected:

```sh
./script/build-zed-container
```

The repository is bind-mounted into the container. Build artifacts therefore
remain available on the host under `target/container`:

- `target/container/debug/zed`
- `target/container/debug/cli`
- `target/container/wasm32-wasip2/debug/zed_svn.wasm`

Use `--release` for release artifacts, `test` for the native source-control
suites, `bundle` for Zed's Linux release archive, `shell` for an interactive
development environment, or `image` to build only the image. Pass
`--no-image-build` to reuse an image without checking for Dockerfile changes.

The checked-in `.devcontainer/devcontainer.json` uses the same image and bind
mount. Open the repository as a dev container from Zed or another compatible
editor; Cargo registry and Git caches are kept in named volumes while build
outputs stay in the host workspace. On Linux, `updateRemoteUserUID` maps the
unprivileged `zed` user to a normal host user's UID.

For the unusual case of a checkout owned by host UID 0, use
`./script/build-zed-container` or its `shell` mode. The script preserves root
ownership for host artifacts without changing the image's default non-root
user; the standard dev-container UID update intentionally does not remap a user
to UID 0.
