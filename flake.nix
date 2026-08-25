{
  description = "nixos-package-search - live search-as-you-type popup for nixpkgs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        runtimeLibs = with pkgs; [
          libGL
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
        ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "nixos-package-search";
          version = "0.1.0";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.pkg-config pkgs.makeWrapper ];
          buildInputs = runtimeLibs;

          postFixup = ''
            wrapProgram $out/bin/nixos-package-search \
              --set LD_LIBRARY_PATH ${pkgs.lib.makeLibraryPath runtimeLibs} \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix pkgs.util-linux ]}
          '';

          meta = {
            description = "Search-as-you-type popup for nixpkgs packages, launch via nix shell";
            mainProgram = "nixos-package-search";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.pkg-config pkgs.rust-analyzer ] ++ runtimeLibs;
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
        };
      });
}
