{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  buildInputs = with pkgs; [udev];
  nativeBuildInputs = with pkgs; [
    rustup
    pkg-config
    bpf-linker

    # For llvm-objdump
    llvmPackages.bintools

    # To aid testing
    runc
  ];
}
