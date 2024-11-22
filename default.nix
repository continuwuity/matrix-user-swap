{
  sources ? import ./npins,
  pkgs ? import sources.nixpkgs { },
  crane ? import ./nix/crane.nix { inherit sources pkgs; },
}:
pkgs.callPackage ./nix/package.nix { inherit crane; }
