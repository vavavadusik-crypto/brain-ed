# Static Linux release artifacts

Supported targets: `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`. The ARM binary is also intended for Android/Termux;
this is a Linux ELF, not an Android APK. No Windows/macOS build is claimed.

Prerequisites: Rust **1.97.1** via rustup (pinned by `rust-toolchain.toml`),
Zig **0.15.1**, Bash, GNU coreutils, `file`, and binutils (`readelf`).
The release includes the default Cargo features, including optional semantic
search support, but no model weights. C++ compilation is necessary for esaxx-rs.
A pinned Zig can be supplied by `python3 -m pip install ziglang==0.15.1` in an
isolated environment, or by an existing Zig installation.

```sh
rustup target add --toolchain 1.97.1 x86_64-unknown-linux-musl aarch64-unknown-linux-musl
export ZIG=zig                         # or an absolute path to Zig 0.15.1
export CARGO_TARGET_DIR=/path/to/build # optional, defaults to checkout/target
export RELEASE_OUT=/path/to/dist      # optional, defaults to $CARGO_TARGET_DIR/dist
packaging/build-release.sh            # both targets, sequentially, two jobs each
# Recheck/package existing target/release/brain files without compiling:
packaging/build-release.sh --package-only
# One target, e.g. for a CI matrix job:
packaging/build-release.sh aarch64-unknown-linux-musl
(cd "$RELEASE_OUT" && sha256sum --check SHA256SUMS)
```

Artifacts are raw executables named `brain-<version>-<Rust target>`, alongside
`SHA256SUMS`, `.file` (ELF description) and `.bytes` (exact size). The script
uses `cargo build --locked`, repository vendored patches and explicit temporary
Zig C/C++/AR drivers. It overrides the machine-specific linker configuration
for these targets and remaps checkout/Cargo/Rust paths. Static linkage is
checked with readelf (no INTERP or NEEDED). Build output stays outside source
when `CARGO_TARGET_DIR` and `RELEASE_OUT` do. Use a fresh output directory per
version. This defines repeatable build inputs and commands; byte-for-byte
reproducibility between independent hosts has not been established.

`.github/workflows/release.yml` runs only on version tag pushes, checks the tag
against Cargo.toml, builds both matrix targets and uploads workflow artifacts.
It has read-only contents permission and publishes no release. No credentials
are needed. Each matrix download has its own `SHA256SUMS`; a future publisher
must combine the two manifests and publish both binaries and the combined
`SHA256SUMS` as assets under the matching `v<version>` release before `install.sh`
can download them. Existing release assets are not changed by this work.

`install.sh --version <version> --dry-run` prints the selected asset and target
without network access or files. The real installer downloads with HTTPS only,
checks exactly one digest matching the requested filename, and stages the
verified file alongside `~/.local/bin/brain` before an atomic rename. It does
not execute downloaded content. `BRAIN_RELEASE_BASE_URL` can select an HTTPS
mirror. The checksum and payload have the same trust origin: this checks
integrity, not an independent publisher signature. For source/desktop installs,
use the retained `packaging/install-source.sh`.

## macOS preview artifacts

`packaging/build-macos.sh` builds native ARM64 or Intel Mach-O assets and
SHA256SUMS on the matching runner, without Developer ID signing/notarization.
The release workflow only uploads build artifacts; it does not publish a release.
Combine all platform checksum manifests when publishing. See
[macOS installation and limitations](macos.md); native CI evidence is pending.
