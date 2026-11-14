{
  description = "Mount iCloud Drive on Linux as a local-first FUSE filesystem";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        icloud-for-linux = pkgs.callPackage ./nix/package.nix { };
      in
      {
        packages = {
          inherit icloud-for-linux;
          default = icloud-for-linux;
        };

        apps.default = {
          type = "app";
          program = "${icloud-for-linux}/bin/icloudctl";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            fuse3
            pkg-config
            sqlite
            just
            gnumake
          ];
          inputsFrom = [ icloud-for-linux ];
          shellHook = ''
            echo "icloud-for-linux dev shell — cargo run -- --help"
          '';
        };
      });
}
