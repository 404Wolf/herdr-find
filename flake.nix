{
  description = "MCP server that finds a running Herdr agent session by what it's about and focuses its pane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Toolchain fallback for consumers whose (followed) nixpkgs ships a rustc
    # older than the crate's rust-version. Only used in that case.
    nixpkgs-rust.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-rust,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      # The consumer's rustPlatform when its rustc is new enough, else ours.
      rustPlatformFor =
        pkgs:
        if nixpkgs.lib.versionAtLeast pkgs.rustc.version cargoToml.package.rust-version then
          pkgs.rustPlatform
        else
          nixpkgs-rust.legacyPackages.${pkgs.stdenv.hostPlatform.system}.rustPlatform;
    in
    {
      packages = forAllSystems (pkgs: {
        herdr-find = (rustPlatformFor pkgs).buildRustPackage {
          pname = cargoToml.package.name;
          inherit (cargoToml.package) version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            inherit (cargoToml.package) description;
            homepage = "https://github.com/404wolf/herdr-find";
            license = pkgs.lib.licenses.mit;
            mainProgram = "herdr-find";
            platforms = systems;
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.herdr-find;
      });

      overlays.default = final: _prev: {
        herdr-find = self.packages.${final.stdenv.hostPlatform.system}.herdr-find;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.herdr-find ];
          packages = with pkgs; [
            cargo
            clippy
            rust-analyzer
            rustc
            rustfmt
            nixfmt
          ];
        };
      });

      formatter = forAllSystems (
        pkgs:
        pkgs.writeShellApplication {
          name = "herdr-find-fmt";
          runtimeInputs = with pkgs; [
            cargo
            rustfmt
            nixfmt
            findutils
          ];
          text = ''
            find . -name '*.nix' -not -path './target/*' -exec nixfmt {} +
            cargo fmt --all
          '';
        }
      );

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) herdr-find;
      });
    };
}
