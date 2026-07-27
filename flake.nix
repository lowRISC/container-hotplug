# Copyright lowRISC Contributors.
# Licensed under the MIT License, see LICENSE for details.
# SPDX-License-Identifier: Apache-2.0
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    # 25.11 has no LLVM 22, which the pinned nightly needs (see below).
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    nixpkgs-unstable,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [rust-overlay.overlays.default];
        };
        unstable = import nixpkgs-unstable {inherit system;};

        # The eBPF crate needs nightly (`build-std`), so use one nightly
        # toolchain for the whole tree, as CI does. Which nightly is pinned by
        # the rust-overlay input in flake.lock; bump it with
        # `nix flake update rust-overlay`, keeping the LLVM below in step.
        rustToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
            toolchain.default.override {
              extensions = ["rust-src"];
            }
        );

        # bpf-linker must be built against the same LLVM major as the
        # toolchain above, or it cannot read the bitcode rustc emits.
        llvmPackages = unstable.llvmPackages_22;
        bpf-linker = unstable.bpf-linker.override {
          rustc = {
            inherit llvmPackages;
            inherit (llvmPackages) llvm;
          };
        };
      in {
        devShells = {
          default = pkgs.mkShell {
            buildInputs = with pkgs; [udev];
            nativeBuildInputs = [
              rustToolchain
              pkgs.pkg-config
              bpf-linker

              # For llvm-objdump
              llvmPackages.bintools

              # To aid testing
              pkgs.runc
            ];
          };
        };
        formatter = pkgs.alejandra;
      }
    );
}
