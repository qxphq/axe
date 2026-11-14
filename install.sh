#!/bin/sh
# Install axe from the rolling `latest` release: a build of `main` on every push.
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
# builds from source and wraps the tools axe shells out to, while the glibc
# release binary expects a loader layout this system does not keep.
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
Linux) os=unknown-linux-gnu ;;
*) die "unsupported system: $os (Linux and macOS only)" ;;
esac

case $arch in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) die "unsupported machine: $arch (x86_64 and aarch64 only)" ;;
esac

# The release binaries are glibc; a musl system would install a file that
# cannot start.
if [ "$os" = unknown-linux-gnu ] && ldd --version 2>&1 | grep -qi musl; then
	die "musl libc: no matching release binary; build from source with
  cargo install --git https://github.com/$repo --locked"
fi

triple=$arch-$os
tar=axe-$triple.tar.gz

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM HUP

curl -fsSL -o "$tmp/$tar" "$base/$tar" || die "download failed: $base/$tar"
curl -fsSL -o "$tmp/$tar.sha256" "$base/$tar.sha256" ||
	die "download failed: $base/$tar.sha256"

(
	cd "$tmp" || exit 1
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "$tar.sha256"
	else
		shasum -a 256 -c "$tar.sha256"
	fi
) >/dev/null 2>&1 || die "checksum mismatch for $tar"

tar -xzf "$tmp/$tar" -C "$tmp" axe || die "extract failed: $tar"

mkdir -p "$dir" || die "cannot create $dir"
install -m 755 "$tmp/axe" "$dir/axe" || die "cannot install to $dir"

# The musl guard reads `ldd`, which a host can also have and not answer, so the
# binary is the authority: one that does not start here is removed rather than
# left where the next `axe` would find it.
version=$("$dir/axe" --version) || {
	rm -f "$dir/axe"
	die "the binary does not run on this system ($(uname -s)/$(uname -m)); build from source with
  cargo install --git https://github.com/$repo --locked"
}
printf 'installed %s (%s)\n' "$dir/axe" "$version"

case ":$PATH:" in
*":$dir:"*) ;;
*) printf 'add %s to PATH to run it as `axe`\n' "$dir" >&2 ;;
esac
