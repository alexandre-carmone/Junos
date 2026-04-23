{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.rekos-web;
  # Extract the numeric port from "host:port" for firewall rules
  port = toInt (last (splitString ":" cfg.bindAddr));
in
{
  options.services.rekos-web = {
    enable = mkEnableOption "rekos-web KStars Ekos Live LAN relay";

    package = mkOption {
      type = types.package;
      description = ''
        The rekos-web wrapper package (includes rekos-server with a
        baked-in --dist-dir pointing to the compiled WASM frontend).
        Defaults to the package from the same flake revision.
      '';
    };

    bindAddr = mkOption {
      type = types.str;
      default = "127.0.0.1:3000";
      example = "0.0.0.0:8080";
      description = ''
        Address and port the server listens on.
        Use 0.0.0.0 to accept connections from other hosts on the LAN
        (required for KStars to reach it from a separate machine).
      '';
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open the TCP port derived from bindAddr in the NixOS firewall.
        Enable this when KStars runs on a different host.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.services.rekos-web = {
      description = "rekos-web KStars Ekos Live relay server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/rekos-server --bind-addr ${cfg.bindAddr}";
        Restart = "on-failure";
        RestartSec = "5s";

        # Isolate the service — it only needs network and the Nix store (read-only)
        DynamicUser = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        NoNewPrivileges = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
      };
    };

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [ port ];
  };
}
