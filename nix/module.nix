{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.rekos-web;
  portOf = addr: toInt (last (splitString ":" addr));
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

    httpAddr = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      example = "0.0.0.0:8080";
      description = ''
        HTTP listen address — used by KStars (Ekos Live offline server).
        Use 0.0.0.0 to accept connections from other hosts on the LAN.
      '';
    };

    httpsAddr = mkOption {
      type = types.str;
      default = "127.0.0.1:8443";
      example = "0.0.0.0:8443";
      description = ''
        HTTPS listen address — used by the browser UI. iOS Safari requires
        TLS to expose WebGPU. A self-signed cert is auto-generated into the
        service's StateDirectory (.certs/) on first run.
      '';
    };

    enableHttps = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Run the HTTPS listener. Disable for headless / CI deployments where
        only KStars needs to reach the server (HTTP only).
      '';
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Open the TCP ports (HTTP and, if enabled, HTTPS) in the NixOS firewall.
        Enable this when KStars or browsers run on a different host.
      '';
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "--captures-dir" "/srv/astro/captures" ];
      description = ''
        Additional command-line arguments passed verbatim to rekos-server.
        Useful for --captures-dir, --tls-cert, --tls-key.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.services.rekos-web = {
      description = "rekos-web KStars Ekos Live relay server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = {
        ExecStart = concatStringsSep " " (
          [ "${cfg.package}/bin/rekos-server"
            "--http-addr" cfg.httpAddr
          ]
          ++ optionals cfg.enableHttps [ "--https-addr" cfg.httpsAddr ]
          ++ optional (!cfg.enableHttps) "--no-https"
          ++ map escapeShellArg cfg.extraArgs
        );
        Restart = "on-failure";
        RestartSec = "5s";

        # The server auto-generates .certs/ in its working directory on
        # first run; StateDirectory gives DynamicUser a writable location
        # that survives restarts.
        StateDirectory = "rekos-web";
        WorkingDirectory = "/var/lib/rekos-web";

        DynamicUser = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        NoNewPrivileges = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
      };
    };

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall (
      [ (portOf cfg.httpAddr) ]
      ++ optional cfg.enableHttps (portOf cfg.httpsAddr)
    );
  };
}
