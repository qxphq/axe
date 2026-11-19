#!/bin/sh
# Install axe from the latest release: one per version, made by CI when a push
# to main carries a new Cargo.toml version.
#   curl -fsSL https://raw.githubusercontent.com/qxphq/axe/main/install.sh | sh
# AXE_INSTALL_DIR overrides the destination; the default is ~/.local/bin.
# NixOS installs the flake instead: nix profile add --no-write-lock-file github:qxphq/axe#default.
set -eu

repo=qxphq/axe
base="https://github.com/$repo/releases/latest/download"
dir=${AXE_INSTALL_DIR:-"${HOME:-}/.local/bin"}

die() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

case ${HOME:-} in
'') [ -n "${AXE_INSTALL_DIR:-}" ] || die "HOME is not set; set AXE_INSTALL_DIR" ;;
esac

# NixOS: the package manager is the install that belongs there. The flake
# builds from source and wraps the tools axe shells out to, which a NixOS
# PATH does not otherwise promise.
if [ -e /etc/NIXOS ]; then
	command -v nix >/dev/null 2>&1 ||
		die "NixOS without nix; install with: nix profile add --no-write-lock-file github:$repo#default"
	nix profile add --no-write-lock-file "github:$repo#default" ||
		die "nix profile add failed"
	printf 'installed axe via nix from github:%s#default\n' "$repo"
	exit 0
fi

os=$(uname -s)
arch=$(uname -m)

case $os in
Darwin) os=apple-darwin ;;
Linux) os=unknown-linux-musl ;;
*) die "unsupported system: $os (Linux and macOS only)" ;;
esac

case $arch in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) die "unsupported machine: $arch (x86_64 and aarch64 only)" ;;
esac

triple=$arch-$os
tar=axe-$triple.tar.gz

# Each tool named up front: a missing one would otherwise read as a failed
# download or, worse, a checksum mismatch.
command -v curl >/dev/null 2>&1 || die "curl is required"
if command -v sha256sum >/dev/null 2>&1; then
	sum="sha256sum -c"
elif command -v shasum >/dev/null 2>&1; then
	sum="shasum -a 256 -c"
else
	die "sha256sum or shasum is required to check the download"
fi

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM HUP

curl -fsSL -o "$tmp/$tar" "$base/$tar" || die "download failed: $base/$tar"
curl -fsSL -o "$tmp/$tar.sha256" "$base/$tar.sha256" ||
	die "download failed: $base/$tar.sha256"

(cd "$tmp" && $sum "$tar.sha256") >/dev/null 2>&1 || die "checksum mismatch for $tar"

tar -xzf "$tmp/$tar" -C "$tmp" axe || die "extract failed: $tar"

# The binary is the authority on whether it runs here, and it is asked before
# it replaces anything: a failure leaves an existing install as it was.
chmod 755 "$tmp/axe"
version=$("$tmp/axe" --version) ||
	die "the binary does not run on this system ($(uname -s)/$(uname -m)); build from source with
  cargo install --git https://github.com/$repo --locked"

mkdir -p "$dir" || die "cannot create $dir"
install -m 755 "$tmp/axe" "$dir/axe" || die "cannot install to $dir"
printf 'installed %s (%s)\n' "$dir/axe" "$version"

case ":$PATH:" in
*":$dir:"*) ;;
*) printf 'add %s to PATH to run it as `axe`\n' "$dir" >&2 ;;
esac
