import ./lib.nix ({ self, pkgs, lib, ... }:
let
  inherit (self.packages.x86_64-linux) kld-mgr;

  kexec-installer = self.inputs.nixos-images.packages.${pkgs.system}.kexec-installer-nixos-unstable;

  validator-system = self.nixosConfigurations.kld-00;

  dependencies = [
    validator-system.config.system.build.toplevel
    validator-system.config.system.build.disko
  ] ++ builtins.map (i: i.outPath) (builtins.attrValues self.inputs);

  closureInfo = pkgs.closureInfo { rootPaths = dependencies; };

  shared = {
    virtualisation.vlans = [ 1 ];
    systemd.network = {
      enable = true;

      networks."10-eth1" = {
        matchConfig.Name = "eth1";
        linkConfig.RequiredForOnline = "no";
      };
    };
    documentation.enable = false;

    # do not try to fetch stuff from the internet
    nix.settings = {
      substituters = lib.mkForce [ ];
      hashed-mirrors = null;
      connect-timeout = 1;
      flake-registry = pkgs.writeText "flake-registry" ''{"flakes":[],"version":2}'';
    };

    environment.etc."install-closure".source = "${closureInfo}/store-paths";
    system.extraDependencies = dependencies;
  };
  qemu-common = import (pkgs.path + "/nixos/lib/qemu-common.nix") {
    inherit lib pkgs;
  };
  interfacesNumbered = config: lib.zipLists config.virtualisation.vlans (lib.range 1 255);
  getNicFlags = config: lib.flip lib.concatMap
    (interfacesNumbered config)
    ({ fst, snd }: qemu-common.qemuNICFlags snd fst config.virtualisation.test.nodeNumber);
in
{
  name = "kld-mgr";
  nodes = {
    installer = { pkgs, ... }: {
      imports = [ shared ];
      systemd.network.networks."10-eth1".networkConfig.Address = "192.168.42.1/24";
      environment.systemPackages = [ pkgs.git ];

      system.activationScripts.rsa-key = ''
        ${pkgs.coreutils}/bin/install -D -m600 ${./ssh-keys/ssh} /root/.ssh/id_rsa
      '';
    };
    installed = {
      imports = [ shared ];
      systemd.network.networks."10-eth1".networkConfig.Address = "192.168.42.2/24";

      virtualisation.emptyDiskImages = [ 4096 4096 ];
      virtualisation.memorySize = 4096;
      networking.nameservers = [ "127.0.0.1" ];
      services.openssh.enable = true;
      services.openssh.settings.useDns = false;
      users.users.root.openssh.authorizedKeys.keyFiles = [ ./ssh-keys/ssh.pub ];
    };
  };
  testScript = { nodes, ... }:
    let
      tomlConfig = "${pkgs.runCommand "config" {} ''
        install -D ${./test-config.toml} $out/test-config.toml
      ''}/test-config.toml";
    in
    ''
      def create_test_machine(oldmachine=None, args={}): # taken from <nixpkgs/nixos/tests/installer.nix>
          machine = create_machine({
            "qemuFlags":
              '-cpu max -m 4024 -virtfs local,path=/nix/store,security_model=none,mount_tag=nix-store,'
              f' -drive file={oldmachine.state_dir}/empty0.qcow2,id=drive1,if=none,index=1,werror=report'
              ' -device virtio-blk-pci,drive=drive1'
              f' -drive file={oldmachine.state_dir}/empty1.qcow2,id=drive2,if=none,index=2,werror=report'
              ' -device virtio-blk-pci,drive=drive2'
              ' ${toString (getNicFlags nodes.installed)}'
          } | args)
          driver.machines.append(machine)
          return machine

      start_all()
      installed.wait_for_unit("sshd.service")
      installed.succeed("ip -c a >&2; ip -c r >&2")

      installer.wait_for_unit("network.target")
      installer.succeed("ping -c1 192.168.42.2")
      # our test config will read from here
      installer.succeed("cp -r ${self} /root/near-staking-knd")

      installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} generate-config /tmp/config")
      installer.succeed("nixos-rebuild dry-build --flake /tmp/config#kld-00 >&2")

      installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} --yes install --hosts kld-00 --debug --no-reboot --kexec-url ${kexec-installer}/nixos-kexec-installer-${pkgs.stdenv.hostPlatform.system}.tar.gz >&2")
      installer.succeed("ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no root@192.168.42.2 -- reboot >&2")
      installed.shutdown()

      new_machine = create_test_machine(oldmachine=installed, args={ "name": "after_install" })
      new_machine.start()
      hostname = new_machine.succeed("hostname").strip()
      assert "kld-00" == hostname, f"'kld-00' != '{hostname}'"

      installer.wait_until_succeeds("ssh -o StrictHostKeyChecking=no root@192.168.42.2 -- exit 0 >&2")

      new_machine.wait_for_unit("sshd.service")
      # TODO test actual service here

      installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} --yes dry-update --hosts kld-00 >&2")

      # requires proper setup of certificates...
      #installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} --yes update --hosts kld-00 >&2")
      #installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} --yes update --hosts kld-00 >&2")
      # XXX find out how we can make persist more than one profile in our test
      #installer.succeed("${lib.getExe kld-mgr} --config ${tomlConfig} --yes rollback --hosts kld-00 >&2")
    '';
})