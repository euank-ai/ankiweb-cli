{
  description = "CLI tool for interacting with AnkiWeb";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          ankiweb-cli = pkgs.rustPlatform.buildRustPackage {
            pname = "ankiweb-cli";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            meta = with pkgs.lib; {
              description = "CLI tool for interacting with AnkiWeb";
              license = licenses.mit;
              mainProgram = "ankiweb-cli";
            };
          };
          default = self.packages.${system}.ankiweb-cli;
        });

      overlays.default = final: prev: {
        ankiweb-cli = self.packages.${prev.system}.ankiweb-cli;
      };
    };
}
