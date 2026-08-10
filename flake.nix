{
  description = "ccorral — terminal control panel for Claude Code sessions in tmux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forEach = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forEach (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "ccorral";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
        };
      });

      devShells = forEach (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.gcc pkgs.rust-analyzer pkgs.clippy ];
        };
      });
    };
}
