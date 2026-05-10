{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.junos-web;
  portOf = addr: toInt (last (splitString ":" addr));

  certDir  = "/var/lib/junos-web/certs";
  certPath = "${certDir}/cert.pem";
  keyPath  = "${certDir}/key.pem";

  # subjectAltName list (comma-joined) fed to `openssl req -addext`.
  sanList = concatStringsSep "," cfg.tls.subjectAltNames;

  # Generates a self-signed cert under StateDirectory if missing. Idempotent —
  # subsequent starts skip the openssl call. Delete the dir to force renewal.
  generateCertScript = pkgs.writeShellScript "junos-web-gen-cert" ''
    set -eu
    install -d -m 0700 ${certDir}
    if [ ! -s ${certPath} ] || [ ! -s ${keyPath} ]; then
      echo "junos-web: generating self-signed TLS cert at ${certDir}"
      ${pkgs.openssl}/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=junos-web" \
        -addext "subjectAltName=${sanList}" \
        -keyout ${keyPath} \
        -out    ${certPath}
      chmod 0600 ${keyPath} ${certPath}
    fi
  '';
in
{
  options.services.junos-web = {
    enable = mkEnableOption "junos-web KStars Ekos Live LAN relay";

    package = mkOption {
      type = types.package;
      description = ''
        The junos-web wrapper package (includes junos-server with a
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
        TLS to expose WebGPU.
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

    tls = {
      autoGenerate = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Generate a self-signed TLS cert + key into ${certDir} on first
          start (and reuse them afterwards). Set to false if you want to
          supply your own cert via tls.cert / tls.key.
        '';
      };

      subjectAltNames = mkOption {
        type = types.listOf types.str;
        default = [ "DNS:localhost" "IP:127.0.0.1" ];
        example = [ "DNS:localhost" "IP:127.0.0.1" "IP:192.168.1.10" "DNS:nas.lan" ];
        description = ''
          subjectAltName entries baked into the auto-generated cert.
          Add an `IP:` entry for every address a browser will hit
          (e.g. the host's LAN IP) so iOS Safari accepts the cert.
        '';
      };

      cert = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = ''
          Path to a PEM-encoded TLS certificate. When set together with
          tls.key, overrides the auto-generated cert. Must be readable by
          the service user (DynamicUser-friendly: world-readable, or
          deployed via systemd LoadCredential).
        '';
      };

      key = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = ''
          Path to the PEM-encoded TLS private key matching tls.cert.
        '';
      };
    };

    capturesDir = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/srv/astro/captures";
      description = ''
        Root directory exposed by the Files tab (`/api/files/*`). Browser
        requests are sandboxed inside this folder. When null the server
        falls back to $HOME/Pictures and finally cwd — but DynamicUser +
        ProtectHome means $HOME doesn't exist for the service, so set this
        explicitly. The path is added to ReadWritePaths so the hardened
        unit can reach it.
      '';
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "--http-addr" "0.0.0.0:8080" ];
      description = ''
        Additional command-line arguments passed verbatim to junos-server.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.tls.autoGenerate || (cfg.tls.cert != null && cfg.tls.key != null);
        message = "services.junos-web: either tls.autoGenerate must be true, or both tls.cert and tls.key must be set.";
      }
    ];

    systemd.services.junos-web = {
      description = "junos-web KStars Ekos Live relay server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig =
        let
          effectiveCert = if cfg.tls.cert != null then cfg.tls.cert else certPath;
          effectiveKey  = if cfg.tls.key  != null then cfg.tls.key  else keyPath;

          tlsArgs = optionals cfg.enableHttps [
            "--tls-cert" effectiveCert
            "--tls-key"  effectiveKey
          ];

          capturesPath = if cfg.capturesDir != null then toString cfg.capturesDir else null;
          capturesArgs = optionals (capturesPath != null) [ "--captures-dir" capturesPath ];

          # When capturesDir lives under /home, ProtectHome=true would mask
          # it. Switch ProtectHome to "tmpfs" (still hides every other home)
          # and bind-mount the captures path so the service can reach it.
          capturesUnderHome = capturesPath != null && hasPrefix "/home/" capturesPath;
        in
        {
          ExecStart = concatStringsSep " " (
            [ "${cfg.package}/bin/junos-server"
              "--http-addr" cfg.httpAddr
            ]
            ++ optionals cfg.enableHttps [ "--https-addr" cfg.httpsAddr ]
            ++ optional (!cfg.enableHttps) "--no-https"
            ++ tlsArgs
            ++ capturesArgs
            ++ map escapeShellArg cfg.extraArgs
          );

          ReadWritePaths = optional (capturesPath != null && !capturesUnderHome) capturesPath;
          BindPaths      = optional capturesUnderHome capturesPath;

          ExecStartPre = mkIf (cfg.enableHttps && cfg.tls.autoGenerate && cfg.tls.cert == null)
            [ "${generateCertScript}" ];

          Restart = "on-failure";
          RestartSec = "5s";

          StateDirectory = "junos-web";
          StateDirectoryMode = "0750";
          WorkingDirectory = "/var/lib/junos-web";

          DynamicUser = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectHome = if capturesUnderHome then "tmpfs" else true;
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
