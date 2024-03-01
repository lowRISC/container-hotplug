{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  buildInputs = with pkgs; [udev];
  nativeBuildInputs = with pkgs; [rustup pkg-config];
}
