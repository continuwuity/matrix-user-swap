{
  sources,
  pkgs,
  useNightlyRustfmt ? false,
}:
let
  crane = import sources.crane { inherit pkgs; };
in
crane.overrideToolchain (
  p:
  let
    fenix = import sources.fenix { pkgs = p; };
    stable = fenix.fromToolchainName {
      name = (pkgs.lib.importTOML ../rust-toolchain.toml).toolchain.channel;
      sha256 = "sha256-yMuSb5eQPO/bHv+Bcf/US8LVMbf/G/0MSfiPwBhiPpk=";
    };
    nightly = fenix.latest;
  in
  fenix.combine [
    stable.rustc
    stable.cargo
    stable.clippy
    (if useNightlyRustfmt then nightly.rustfmt else stable.rustfmt)
  ]
)
