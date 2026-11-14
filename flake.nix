{
  description = "axe: minimal coding agent harness";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      each = f: nixpkgs.lib.genAttrs
        # nixpkgs 26.11 errors out on x86_64-darwin, so it is not claimed here.
        [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ]
        (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = each (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "axe";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # No test run in the sandbox: `cargo test` is the gate CI and the
          # developer run, and a build that repeats it rebuilds the test
          # profile on every machine and fails on whatever the sandbox lacks
          # (an unwritable home, the host's gzip, /bin/sh).
          doCheck = false;

          # What axe runs itself: `sh` (every command), `git` (its exec
          # directory, for the remotes the allowlist hands over), `gzip` (old
          # session bodies), and coreutils' `kill` (a timeout or an interrupt
          # takes the process group) and `sha256sum` (the hash a project MCP
          # file is approved by). What the model's confined `bash` needs on top:
          # `rg` and `fd`. A minimal server has none of them, so the wrapper
          # pins them instead of the config.
          postInstall = ''
            wrapProgram $out/bin/axe --prefix PATH : ${
              pkgs.lib.makeBinPath [
                pkgs.bash
                pkgs.coreutils
                pkgs.git
                pkgs.ripgrep
                pkgs.fd
                pkgs.gzip
              ]
            }
          '';

          meta = {
            description = "Minimal coding agent harness";
            mainProgram = "axe";
            platforms = pkgs.lib.platforms.unix;
          };
        };
      });
    };
}
