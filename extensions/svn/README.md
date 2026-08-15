# Subversion extension

This in-tree extension provides the Subversion adapter for Zed's experimental
source-control provider API. It requires a system `svn` client at version 1.14
or newer in the 1.x series and a Dev or Nightly build of this branch.

Source developers with a host Rust toolchain can install this directory with
the `zed: install dev extension` action. Users of the container-built local
application should not do that: its launcher already selects an isolated
profile containing the precompiled extension, as described below. For remote
projects, `svn` must be available on the remote host because commands run next
to the working copy.

The MVP supports working-copy discovery and status, text and property diffs,
selected-path commits, add, schedule delete, revert, resolve, ignore, update,
switch, checkout, history, blame, base text, one-shot credential prompts, and
one-shot TLS certificate trust.

Normal status follows `svn status` and omits ignored paths. Use
`svn status --no-ignore` explicitly when inspecting ignore rules outside Zed.

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

The default `build` mode produces development binaries whose shared libraries
remain in the container. To create the same batteries-included application
layout as Zed's official Linux installer, build and unpack a local release:

```sh
./script/build-zed-container install
./target/container/local/zed
```

The stable launcher resolves through `target/container/local/current` to a
commit- and payload-specific application under `local/builds`. Rebuilding does
not overwrite an existing build. This installation is entirely beneath the
ignored `target/container` directory and does not read, replace, or otherwise
modify an existing `~/.local/zed.app` reference installation. The installer
packages the already compiled SVN WASM component into the application and
preinstalls it in an isolated profile at `target/container/local/user-data`.
The launcher selects that profile automatically, so normal use does not need a
host Rust toolchain and must not use Zed's source-compiling **Install Dev
Extension** action. To restage existing release artifacts without compiling,
run `./script/install-zed-local`. The release SVN WASM artifact also remains at
`target/container/wasm32-wasip2/release/zed_svn.wasm`.
Release/LTO compilation is memory-intensive, so bundle and install modes use
one Cargo job by default. Set `ZED_BUILD_JOBS` to a higher positive integer on
a host with sufficient memory. The archive-only `bundle` mode retains Zed's
canonical thin-LTO release profile and includes the separate static remote/SSH
server. The local `install` mode skips that unrelated second target, disables
LTO, uses 16 code-generation units, and omits debug information so it can build
on an 11 GiB development host; its desktop features and bundled-library layout
are unchanged.

The checked-in `.devcontainer/devcontainer.json` uses the same image and bind
mount. Open the repository as a dev container from Zed or another compatible
editor. The image preinstalls the `cargo-about` version required by Zed's
bundle license generator. Cargo registry and Git caches are kept in named
volumes while build outputs stay in the host workspace. On Linux,
`updateRemoteUserUID` maps the unprivileged `zed` user to a normal host user's
UID.

For the unusual case of a checkout owned by host UID 0, use
`./script/build-zed-container` or its `shell` mode. The script preserves root
ownership for host artifacts without changing the image's default non-root
user; the standard dev-container UID update intentionally does not remap a user
to UID 0.
