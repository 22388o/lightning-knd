_:
{
  perSystem = { config, ... }: {
    checks.lightning-knd-clippy = config.packages.lightning-knd.clippy;
  };
}
