{
  description = "yt-dlp-feed";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "yt-dlp-feed";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "lofty-0.23.3" = "sha256-/IksX73xY06jNBYrZsfQdqX7a3sDkA0hkxtOXbWFaqs=";
              "yt-dlp-2.7.2" = "sha256-ObDbbZOSnumqR8mL85OIp4BeBQ0v/egFS3GF+WtS7Xw=";
            };
          };
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [
            openssl
          ];
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            rustc
            rustfmt
            pkg-config
            openssl
          ];
        };
      });
    };
}
