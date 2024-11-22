let
  sources = import ./npins;
  pkgs = import sources.nixpkgs { };
  fenix = import sources.fenix { };

  stdenv' =
    if pkgs.stdenv.isLinux then
      pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv
    else
      pkgs.stdenv;

  rustToolchain = fenix.combine (
    with fenix.stable;
    [
      cargo
      clippy
      rust-src
      rustc

      # Nightly rustfmt because most options are unstable
      fenix.latest.rustfmt
    ]
  );
in
pkgs.mkShell.override { stdenv = stdenv'; } {
  env = {
    # Rust Analyzer needs to be able to find the path to default crate
    # sources, and it can read this environment variable to do so. The
    # `rust-src` component is required in order for this to work.
    RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
  };

  packages =
    [ rustToolchain ]
    ++ (with pkgs; [
      engage
      lychee
      nixfmt-rfc-style
      nodePackages.markdownlint-cli
      npins
    ]);
}
