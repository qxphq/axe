# Release and packaging

Axe review prompt — one concern: **one version lives in one place, every
artifact the installer, flake and CI name actually exists, and a clean checkout
can be built and released on each supported platform.**

Read first: `AGENTS.md` invariants 1 and 16; README "Install", "Build", "Run";
`Cargo.toml`, `Cargo.lock`, `flake.nix`, `flake.lock`, `install.sh`, and
`.github/workflows/ci.yml` in full.

Verify with depth — follow a release from commit to installed binary:

- Version: only `Cargo.toml` holds it; `env!("CARGO_PKG_VERSION")` is the
  banner and `--version`; `flake.nix` reads the same file; no tag or second
  copy. Check `Cargo.lock`'s version field, the release notes claim, and
  whether `--version` works with no config, network or cwd.
- `install.sh`: POSIX sh, `set -eu`, `HOME` unset with `AXE_INSTALL_DIR`,
  OS/arch to triple mapping, Linux target only where CI builds it, musl
  detection, `mktemp` cleanup, curl checksum download, `sha256sum`/`shasum`,
  tar extraction with the exact member name, `install -m 755`, an existing
  binary, spaces in the destination, and the PATH hint. Check failure output
  names the failing step and does not leave a partial binary.
- Flake: locked inputs, version from `Cargo.toml`, `cargoLock.lockFile`,
  `doCheck = false` and its stated reason, `makeWrapper` PATH containing every
  external command axe shells out to (`sh`, `git`, `rg`, `fd`, `gzip`) and
  nothing irrelevant, platform claims versus the CI matrix (including the
  comment about a nixpkgs platform that errors), and `nix build` from a clean
  checkout. Check a missing `flake.lock`, a `nix flake update`-only change,
  and a dev shell or check that the README promises but the flake lacks.
- CI: `fmt --check`, clippy `-D warnings`, `test --locked`, release build on
  the matrix, the interactive harness job with tmux, and the coverage job with
  `--fail-under-lines 55`. Check the release job needs `test`, its permissions,
  the four target names, `--clobber`, the race to create the mutable `latest`
  release, asset names and `.sha256` generation, and whether a PR can publish.
  Check that the `interactive` job actually runs a turn and asserts something,
  and that the coverage job measures the tests it claims.
- Build: dependencies are strictly necessary and widely maintained
  (`serde_json`, `rustls`, `webpki-roots`, `ratatui`, `unicode-width`, `libc`
  on Linux); no crypto or compression crate; the rustls feature set and
  target-gated `libc` compile on macOS and Linux; release profile and binary
  size; no `unsafe` outside `sandbox.rs`; a `Cargo.lock` that matches the
  manifest. Check a clean `cargo build --locked` and `cargo install --git
  ... --locked`.
- Docs and release agreement: README install URL, repo name, `latest` tag
  claim, supported platforms and glibc note; the version in `about`/banner;
  a license or third-party notice the README claims (do not invent one if
  absent; report a claim without a file as a finding). Check `install.sh`'s
  documented override and the release asset names against what CI uploads.
- Reproducibility: a build from the same commit does not fetch a floating
  dependency, the Nix build does not read the network, and the release binary is
  stripped the same way on each target. Check the macOS targets cross-compiled
  from a runner and the Rust toolchain pin (`stable` versus an MSRV claim).

For each finding give the file line, the platform or command affected, and the
concrete outcome: an install that fails on a supported machine, a release that
publishes the wrong version, a binary that cannot find a shell tool, or a
consumer whose lock cannot resolve.

Out of scope: runtime behavior (each concern's brief), the README's product
claims beyond packaging (see `llm-ergonomics.md`), and dependency choice as
such unless it breaks a build or a platform.
