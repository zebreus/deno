{
  description = "High-performance computing projects";

  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:nixos/nixpkgs?ref=refs/heads/master";
    nixpkgs-asciidoc.url = "github:zebreus/nixpkgs?ref=f1a3be7a1160cc4810c0274ab76f0fac813eb4d6";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      nixpkgs-asciidoc,
      fenix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      with nixpkgs.legacyPackages.${system};
      let
        pkgs-with-asciidoc = import nixpkgs-asciidoc { inherit system; };
        pkgs-fenix = fenix.packages.${system};
      in
      rec {

        name = "high-performance-computing";
        packages.default = llvmPackages_16.stdenv.mkDerivation {
          name = name;
          src = ./.;

          buildInputs = [
            llvmPackages_16.openmp
            gcc12
            clang_16

            openmpi
            gnumake
            zlib
            sshfs

            clang-tools_16
            lldb
            nil

            (pkgs-fenix.complete.withComponents [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
            ])
            pkgs-fenix.rust-analyzer
            python3
            gnumake
            cmake
            protobuf
          ];
        };
      }
    );
}
