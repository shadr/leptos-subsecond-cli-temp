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
            wayland

            cargo-leptos

            tailwindcss_4
            mold
            clang
            tailwindcss-language-server
            llvmPackages_latest.lld
            leptosfmt
            dart-sass
            trunk

            pkgs.egl-wayland
            pkgs.libGL
            pkgs.pipewire
            pkgs.pkg-config
            pkgs.wayland
            pkgs.xorg.libxcb
            pkgs.xorg.libXrandr
            pkgs.libgbm
            pkgs.dbus

            pkgs.llvmPackages_latest.libclang
            pkgs.llvmPackages_latest.libclang.lib
            glib

            (rust-bin.nightly.latest.default.override {
              extensions = [
                "rust-src"
                "rustc-codegen-cranelift-preview"
                "llvm-tools-preview"
              ];
              targets = [ "wasm32-unknown-unknown" ];
            })


            (pkgs.callPackage ./dioxus070.nix {})
            # (dioxus-cli.overrideAttrs {
            #   version = "0.7.0-alpha.3";
            #   src =  fetchCrate {
            #     pname = "dioxus-cli";
            #     version = "0.7.0-alpha.3";
            #     hash = "sha256-ibEniOqI0IW9ME+k/rCYUgOpYS16wpzPXFxgn0XAzQo=";
            #     # owner = "DioxusLabs";
            #     # repo = "dioxus";
            #     # tag = "v0.7.0-alpha.3";
            #     # sha256 = "sha256-fQBElpBAwxT8K+DTpocKvvBgXbrxyM25KqOvARQvcGQ=";
            #   };
            #   cargoPatches = [];
            #   cargoHash = "sha256-t5umDmhU8IC5Rau5ssyW0bZnnBI7JxL8A5qlW4WEDOg=";
            # })
          ];

          # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "clang";
          # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-Clink-arg=-fuse-ld=${pkgs.mold}/bin/mold";

          nativeBuildInputs = [
            pkgs.wayland
            pkgs.pipewire
            llvmPackages_latest.llvm
            pkgs.libclang
            pkgs.libGL
            rustPlatform.bindgenHook
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages_latest.libclang.lib}/lib";
          # LD_LIBRARY_PATH = "${}/lib";
        };
      }
    );
}
