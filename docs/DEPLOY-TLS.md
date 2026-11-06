# TLS for the server (ADR 0014)

The server is usually reached by its IP address. Clients always verify the certificate;
the simplest safe setup is a private CA that only your clients trust.

## 1. Create the CA and the server certificate (once)

On the server (or any trusted machine), with the address clients will dial:

```bash
turbo-pancake-app-server gen-cert --name <public IP> [--name <LAN IP>] [--name <host name>] --out tls/
```

It writes:

| File | Where it goes |
|---|---|
| `ca.pem` | to every client (`--server-ca`, later bundled by the installer). Not secret. |
| `ca.key` | **offline** (USB stick / password manager). Whoever has it can issue certificates your clients trust. |
| `server.pem`, `server.key` | on the server, readable only by the service account (`chmod 600` / `icacls`). |

`gen-cert` refuses to overwrite existing keys.

## 2. Run the server with TLS

```bash
turbo-pancake-app-server --bind 0.0.0.0:7878 --tls-cert tls/server.pem --tls-key tls/server.key
# environment instead of flags: TURBO_PANCAKE_TLS_CERT, TURBO_PANCAKE_TLS_KEY
```

HTTPS and WSS are served on the same port. Without TLS, a server bound to a
non-loopback address refuses to serve accounts (unless `--insecure-allow-plaintext`
for a LAN trial).

## 3. Clients

```bash
turbo-pancake-desktop --server wss://<IP>:7878/v1/signal --server-ca ca.pem
```

Without `--server-ca` the client trusts the public web PKI only (a server with a
certificate from a public CA). A wrong CA, an expired certificate or an address that is
not in the certificate shows "Сертификат сервера не доверенный" — there is no option to
skip the check.

## 4. Rotation

Before `server.pem` expires (825 days by default) or when the IP changes:

```bash
turbo-pancake-app-server renew-cert --dir tls/ --name <IP> [--name ...]
```

(`ca.key` must be in `tls/` for this step; put it back offline afterwards.) Restart the
server. Clients keep their `ca.pem`. If `ca.key` leaks: run `gen-cert` into a new
directory and give clients the new `ca.pem`.

## 5. A release for one server

Build the client with the server address and CA baked in, so users need no flags
(nothing of it is committed; set the variables only for the build):

```powershell
$env:TURBO_PANCAKE_DEFAULT_SERVER = 'wss://<IP>:7878/v1/signal'
$env:TURBO_PANCAKE_DEFAULT_SERVER_CA = '<path>\ca.pem'
cargo build --release --locked -p turbo-pancake-launcher -p turbo-pancake-update-tool -p turbo-pancake-desktop
```

`--server` / `--server-ca` on the command line still win. Then pack the launcher and
client (`turbo-pancake-update pack`) and build the installer with
`TURBO_PANCAKE_SETUP_PAYLOAD` (ADR 0017).

Server on Debian/Ubuntu: copy `server.pem` and `server.key` to `/root/turbo-pancake-tls/`
and run `deploy/linux/install-server.sh <public IP> [branch]` as root. It installs
PostgreSQL and Rust, builds the server, creates the `turbo-pancake` systemd service
(TCP and UDP 7878, firewall rules) and can be re-run to update.
