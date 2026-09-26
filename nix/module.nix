{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.junos-web;
  portOf = addr: toInt (last (splitString ":" addr));

  # The system unit keeps its state in /var/lib; the graphical-session user
  # unit (apps.enable) in the user's $XDG_STATE_HOME, which systemd spells %S.
  stateDir = if cfg.apps.enable then "%S/junos-web" else "/var/lib/junos-web";
  certDir  = "${stateDir}/certs";
  certPath = "${certDir}/cert.pem";
  keyPath  = "${certDir}/key.pem";

  # subjectAltName list (comma-joined) fed to `openssl req -addext`.
  sanList = concatStringsSep "," cfg.tls.subjectAltNames;

  # Generates a self-signed cert into the directory passed as $1 if missing.
  # Idempotent — subsequent starts skip the openssl call. Delete the dir to
  # force renewal. The dir is an argument rather than baked in so the user
  # unit's %S is expanded by systemd.
  generateCertScript = pkgs.writeShellScript "junos-web-gen-cert" ''
    set -eu
    dir="$1"
    install -d -m 0700 "$dir"
    if [ ! -s "$dir/cert.pem" ] || [ ! -s "$dir/key.pem" ]; then
      echo "junos-web: generating self-signed TLS cert at $dir"
      ${pkgs.openssl}/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=junos-web" \
        -addext "subjectAltName=${sanList}" \
        -keyout "$dir/key.pem" \
        -out    "$dir/cert.pem"
      chmod 0600 "$dir/key.pem" "$dir/cert.pem"
    fi
  '';

  effectiveCert = if cfg.tls.cert != null then cfg.tls.cert else certPath;
  effectiveKey  = if cfg.tls.key  != null then cfg.tls.key  else keyPath;

  tlsArgs = optionals cfg.enableHttps [
    "--tls-cert" effectiveCert
    "--tls-key"  effectiveKey
  ];

  capturesPath = if cfg.capturesDir != null then toString cfg.capturesDir else null;
  capturesArgs = optionals (capturesPath != null) [ "--captures-dir" capturesPath ];

  dsoTilePath = if cfg.dsoTileDir != null then toString cfg.dsoTileDir else null;
  dsoTileArgs = optionals (dsoTilePath != null) [ "--dso-tile-dir" dsoTilePath ];

  execStart = concatStringsSep " " (
    [ "${cfg.package}/bin/junos-server"
      "--http-addr" cfg.httpAddr
    ]
    ++ optionals cfg.enableHttps [ "--https-addr" cfg.httpsAddr ]
    ++ optional (!cfg.enableHttps) "--no-https"
    ++ tlsArgs
    ++ capturesArgs
    ++ dsoTileArgs
    ++ map escapeShellArg cfg.extraArgs
  );

  generateCert = cfg.enableHttps && cfg.tls.autoGenerate && cfg.tls.cert == null;
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
          Generate a self-signed TLS cert + key on first start (and reuse
          them afterwards) — into /var/lib/junos-web/certs, or
          ~/.local/state/junos-web/certs when apps.enable is set. Set to
          false if you want to supply your own cert via tls.cert / tls.key.
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

    user = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "alexandre";
      description = ''
        System user to run junos-server as. When null (the default) the unit
        runs under a DynamicUser, which is the right choice when capturesDir
        is a dedicated folder created for the service.

        Set this when capturesDir lives on a disk owned by a real user: a
        DynamicUser gets a random uid that cannot traverse a 0700 home or
        removable-media mount point, and cannot write files owned by someone
        else. `/api/files/*` then fails with an opaque 500 on every request.
        Pointing the unit at the owning user fixes both the traversal and the
        write side (thumbnail cache, rename, delete).

        ProtectHome/ProtectSystem still apply, so this does not hand the
        service the user's home — only the paths listed in capturesDir and
        dsoTileDir are reachable.

        Required by apps.enable, where it names the desktop user whose
        graphical session runs the service.
      '';
    };

    apps = {
      enable = mkEnableOption ''
        launching KStars and PHD2 from the Profiles tab. The hardened system
        unit cannot do this: its PATH does not contain the apps, and it has no
        display, no AF_UNIX (X11/Wayland/D-Bus sockets), a private /tmp (X11
        socket, Xauthority) and no home (KStars config). With this enabled the
        server instead runs as a systemd user service of `user`, started with
        their graphical session so launched apps open on that desktop.
        It stops at logout, and runs without the system unit's sandboxing'';

      packages = mkOption {
        type = types.listOf types.package;
        default = [ pkgs.kstars pkgs.phd2 ];
        defaultText = literalExpression "[ pkgs.kstars pkgs.phd2 ]";
        description = ''
          Packages put on the service's PATH. The Launch buttons exec
          `kstars` and `phd2` by bare name. INDI drivers do not need to be
          listed: KStars resolves indiserver and the drivers from its own
          settings.
        '';
      };
    };

    group = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "users";
      description = ''
        Primary group for the service. Only meaningful together with
        services.junos-web.user; defaults to that user's own group.
      '';
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

    dsoTileDir = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/srv/astro/dso_tiles";
      description = ''
        Directory holding the offline DSO survey tiles served at
        `/api/dso_tiles/*`, as written by `scripts/prefetch_dso_tiles.py`.
        When null the server falls back to `.cache/dso_tiles` under its
        WorkingDirectory (${"/var/lib/junos-web"}, the StateDirectory) — set
        this to point at a pre-populated cache stored elsewhere. The path is
        added to ReadOnlyPaths (or bind-mounted when under /home) so the
        hardened unit can reach it. The cache is optional — without it the
        Framing Assistant simply always uses the live hips2fits proxy.
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

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = [
        {
          assertion = cfg.tls.autoGenerate || (cfg.tls.cert != null && cfg.tls.key != null);
          message = "services.junos-web: either tls.autoGenerate must be true, or both tls.cert and tls.key must be set.";
        }
        {
          assertion = cfg.apps.enable -> cfg.user != null;
          message = "services.junos-web: apps.enable runs the server in a user's graphical session, so services.junos-web.user must be set.";
        }
      ];

      networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall (
        [ (portOf cfg.httpAddr) ]
        ++ optional cfg.enableHttps (portOf cfg.httpsAddr)
      );
    }

    (mkIf (!cfg.apps.enable) {
      systemd.services.junos-web = {
        description = "junos-web KStars Ekos Live relay server";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];

        serviceConfig =
          let
            # When an external path lives under /home, ProtectHome=true would
            # mask it. Switch ProtectHome to "tmpfs" (still hides every other
            # home) and bind-mount those paths so the service can reach them.
            # Non-home paths are already reachable read-only under
            # ProtectSystem=strict, so only captures (which needs write) has to
            # be listed there — the DSO cache is read-only.
            underHome = p: p != null && hasPrefix "/home/" p;
            homePaths = filter underHome [ capturesPath dsoTilePath ];
            anyUnderHome = homePaths != [ ];
          in
          {
            ExecStart = execStart;

            ReadWritePaths = optional (capturesPath != null && !(underHome capturesPath)) capturesPath;
            BindPaths      = homePaths;

            ExecStartPre = mkIf generateCert [ "${generateCertScript} ${certDir}" ];

            Restart = "on-failure";
            RestartSec = "5s";

            StateDirectory = "junos-web";
            StateDirectoryMode = "0750";
            WorkingDirectory = stateDir;

            DynamicUser = cfg.user == null;
            PrivateTmp = true;
            ProtectSystem = "strict";
            ProtectHome = if anyUnderHome then "tmpfs" else true;
            NoNewPrivileges = true;
            RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
          }
          // optionalAttrs (cfg.user  != null) { User  = cfg.user;  }
          // optionalAttrs (cfg.group != null) { Group = cfg.group; };
      };
    })

    # Graphical-session user unit: inherits DISPLAY/XAUTHORITY/D-Bus from the
    # environment the desktop imports into the user manager, so KStars and
    # PHD2 spawned by /api/apps/launch open on that desktop.
    (mkIf cfg.apps.enable {
      systemd.user.services.junos-web = {
        description = "junos-web KStars Ekos Live relay server";
        wantedBy = [ "graphical-session.target" ];
        partOf   = [ "graphical-session.target" ];
        after    = [ "graphical-session.target" ];

        # User units are installed for every account; only run for `user`.
        unitConfig.ConditionUser = cfg.user;

        path = cfg.apps.packages;

        serviceConfig = {
          ExecStart = execStart;
          ExecStartPre = mkIf generateCert [ "${generateCertScript} ${certDir}" ];

          # Launched apps are children of junos-server, so they share its
          # cgroup. The default control-group KillMode would take a running
          # KStars down with every server restart; kill only the server and
          # let AppManager::scan_existing() pick the survivors back up.
          KillMode = "process";

          Restart = "on-failure";
          RestartSec = "5s";

          StateDirectory = "junos-web";
          WorkingDirectory = stateDir;
        };
      };
    })
  ]);
}
