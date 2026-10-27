{
  description = "Telegram bot that watches an LDK Server node";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "ldk-telegram";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "ldk-server-client-0.1.0" = "sha256-OzmKTOX3wROk1DXxwsj/RmFiohLYHA0nFv6bKtYCx8g=";
            };
          };
          meta = {
            description = "Telegram bot that watches an LDK Server node";
            license = with pkgs.lib.licenses; [ mit asl20 ];
            mainProgram = "ldk-telegram";
          };
        };
      });

      nixosModules.default = import ./nix/module.nix self;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [ clippy rustfmt rust-analyzer ];
        };
      });
    };
}
