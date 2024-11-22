{
  crane,
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
    engage
    lychee
    nixfmt-rfc-style
    nodePackages.markdownlint-cli
    npins
  ];
}
