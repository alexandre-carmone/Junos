junos-web — portable Linux build
================================

Contents
  junos-server        the LAN relay + web server (single static-ish binary)
  dist/               the compiled WebGPU/Leptos frontend
  junos-web.service   sample systemd unit (edit paths before use)
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
  sudo systemctl daemon-reload
  sudo systemctl enable --now junos-web

Firewall (open the LAN ports if needed)
  sudo firewall-cmd --add-port=8090/tcp --add-port=8443/tcp --permanent
  sudo firewall-cmd --reload
