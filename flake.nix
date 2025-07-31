{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
      in
      with pkgs;
      {
        devShells.default = mkShell {
          buildInputs = [
            openssl
            pkg-config
            cargo-insta
            llvmPackages_latest.llvm
            llvmPackages_latest.bintools
            zlib.out

            (rust-bin.nightly.latest.default.override {
              extensions = [
                "rust-src"
                "llvm-tools-preview"
              ];
              targets = [ "wasm32-unknown-unknown" ];
            })

            (pkgs.callPackage ./dioxus070.nix {})
          ];

          # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "clang";
          # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-Clink-arg=-fuse-ld=${pkgs.mold}/bin/mold";
        };
      }
    );
}
