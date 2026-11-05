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
          # `sessions::gzip_roundtrip` shells out to the host's gzip.
          nativeCheckInputs = [ pkgs.gzip ];

          # Axe shells out to sh, git, rg, fd and gzip. A minimal server has
          # none of them, so the wrapper pins them instead of the config.
          postInstall = ''
            wrapProgram $out/bin/axe --prefix PATH : ${
              pkgs.lib.makeBinPath [
                pkgs.bash
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
