junos-web — portable Linux build
================================

Contents
  junos-server        the LAN relay + web server (single static-ish binary)
  dist/               the compiled WebGPU/Leptos frontend
  junos-web.service   sample systemd unit (edit paths before use)
  junos-guider        the PHD2-compatible autoguider (optional)
  junos-guider.service  its sample systemd unit
  THIRD_PARTY.md      PHD2's notices (junos-guider is a port of it)
  README.txt          this file

Run (foreground, from this directory)
  ./junos-server --http-addr 0.0.0.0:8090 \
                 --https-addr 0.0.0.0:8443 \
                 --dist-dir ./dist

  - Browser UI:  https://<host>:8443   (self-signed cert; accept it — iOS
                 Safari needs TLS for WebGPU. A cert is generated into
                 ./.certs/ on first run.)
  - Point KStars' Ekos Live "offline server" at http://<host>:8090
    (8090, not the upstream default 8080).
  - Pass --no-https for a headless / HTTP-only run.

Install as a service (example)
  sudo install -Dm755 junos-server /usr/bin/junos-server
  sudo install -d /usr/share/junos-web && sudo cp -a dist /usr/share/junos-web/
  # Edit junos-web.service so --dist-dir points at /usr/share/junos-web/dist
  # and the User=/captures paths match your host, then:
  sudo install -Dm644 junos-web.service /etc/systemd/system/junos-web.service
  # Optional: the Profiles tab's Power off / Reboot buttons. Edit the user
  # name in the rule to match User= in the unit (the unit keeps AF_UNIX for
  # the D-Bus socket; drop it from RestrictAddressFamilies to forbid this).
  sudo install -Dm644 50-junos-web-power.rules /etc/polkit-1/rules.d/50-junos-web-power.rules
  sudo systemctl daemon-reload
  sudo systemctl enable --now junos-web

Guider (optional, instead of PHD2: both listen on :4400)
  ./junos-guider --indi-host localhost --indi-port 7624
  - Ekos' guider: PHD2 at <host>:4400. The relay finds it on 127.0.0.1:4410.
  - As a service: install it to /usr/bin/junos-guider and
    junos-guider.service to /etc/systemd/system/ (edit User=), then
    sudo systemctl enable --now junos-guider

Firewall (open the LAN ports if needed; 4400 for an Ekos on another host)
  sudo firewall-cmd --add-port=8090/tcp --add-port=8443/tcp --permanent
  sudo firewall-cmd --reload
