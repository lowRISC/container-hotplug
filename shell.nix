{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  buildInputs = with pkgs; [udev];
  nativeBuildInputs = with pkgs; [
    rustup
    pkg-config
    (pkgs.callPackage deps/bpf-linker.nix { useRustLlvm = true; })

    # For llvm-objdump
    llvmPackages.bintools
  ];
}
