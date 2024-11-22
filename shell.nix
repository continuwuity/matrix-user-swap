{
  sources ? import ./npins,
  pkgs ? import sources.nixpkgs { },
  crane ? import ./nix/crane.nix {
    inherit sources pkgs;
    useNightlyRustfmt = true;
  },
}:
let
  matrix-user-swap = pkgs.callPackage ./nix/package.nix { inherit crane; };
in
pkgs.callPackage ./nix/devshell.nix { inherit crane matrix-user-swap; }
