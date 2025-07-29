# Copyright lowRISC Contributors.
# Licensed under the MIT License, see LICENSE for details.
# SPDX-License-Identifier: Apache-2.0
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    nixpkgs,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {inherit system;};
      in {
        devShells = {
          default = pkgs.mkShell {
            buildInputs = with pkgs; [udev];
            nativeBuildInputs = with pkgs; [
              rustup
              pkg-config
              bpf-linker

              # For llvm-objdump
              llvmPackages_20.bintools

              # To aid testing
              runc
            ];
          };
        };
        formatter = pkgs.alejandra;
      }
    );
}
