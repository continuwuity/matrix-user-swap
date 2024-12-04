{
  crane,
  cargo-insta,
  engage,
  lychee,
  matrix-user-swap,
  nixfmt-rfc-style,
  nodePackages,
  npins,
}:
crane.devShell {
  checks = {
    inherit matrix-user-swap;
  };

  packages = [
    cargo-insta
    engage
    lychee
    nixfmt-rfc-style
    nodePackages.markdownlint-cli
    npins
  ];
}
