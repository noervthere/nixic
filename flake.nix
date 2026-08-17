{
  description = "nixic — a TUI music player for NixOS / Nix with YouTube Music support";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };

          # Bare nixic binary, built from the committed Cargo.lock.
          nixic = pkgs.rustPlatform.buildRustPackage {
            pname = "nixic";
            version = "0.1.0";
            src = nixpkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
          };

          # Wrapped binary: points nixic at bundled mpv / yt-dlp / cava so it
          # works out of the box without polluting the user's PATH.
          wrapped = pkgs.stdenv.mkDerivation {
            pname = "nixic";
            version = nixic.version;
            src = nixpkgs.lib.cleanSource ./.;
            dontUnpack = true;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            installPhase = ''
              runHook preInstall
              mkdir -p $out/bin
              makeWrapper ${nixic}/bin/nixic $out/bin/nixic \
                --set-default NIXIC_MPV_BIN ${pkgs.mpv}/bin/mpv \
                --set-default NIXIC_YTDLP_BIN ${pkgs.yt-dlp}/bin/yt-dlp \
                --prefix PATH : ${nixpkgs.lib.makeBinPath [ pkgs.mpv pkgs.yt-dlp pkgs.cava ]}
              runHook postInstall
            '';
          };
        in
        {
          default = wrapped;
          nixic = wrapped;
        });

      apps = forAllSystems (system:
        {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/nixic";
          };
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
              rust-analyzer
            ];
            buildInputs = with pkgs; [
              mpv
              yt-dlp
              cava
            ];
          };
        });
    };
}
