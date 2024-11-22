{
  crane,
}:
crane.buildPackage {
  src = crane.cleanCargoSource ./..;
  strictDeps = true;
}
