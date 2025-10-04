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
      sha256 = "sha256-SJwZ8g0zF2WrKDVmHrVG3pD2RGoQeo24MEXnNx5FyuI=";
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
