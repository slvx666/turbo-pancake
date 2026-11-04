# ADR 0014 — TLS for an IP-only server: a private CA trusted by the clients

* Status: **accepted** (2026-11-04)
* Spec: §2 (access by IP, certificates must be validated, an explicit trust model, never
  disable verification, the installer needs a reliable initial address/key), §8 (TLS),
  §12 (TLS with trust and certificate rotation), §14 spike 5. Completes ADR 0013 §12.

## Context

The server is a home machine reached by its public IPv4 without a domain. Public CAs
issue IP-address certificates only in limited ways; the owner may not want to depend on
that. The client must still verify every connection.

## Decision

1. **rustls everywhere** (aws-lc-rs provider, already used by the media stack): the
   app-server serves HTTPS + WSS on its listen port (`--tls-cert`, `--tls-key`, PEM);
   handshakes run in their own tasks with a 10 s timeout and at most 256 at once, so a
   slow client cannot stall accepting. UDP media is unchanged (DTLS-SRTP).
2. **Trust model:** a **private CA** made by `turbo-pancake-app-server gen-cert --name
   <ip> [--name <host>]`: `ca.pem` goes to the clients (`--server-ca`, later bundled by
   the installer, S7), `ca.key` is kept offline, the server certificate (IP / DNS SANs,
   825 days by default) is re-issued from the same CA with `renew-cert` — rotation without
   touching clients. With `--server-ca` the client trusts **only** that CA; without it,
   the public web PKI (Mozilla roots), for a server with a publicly trusted certificate.
3. **No way to disable verification.** A wrong or missing CA, an expired certificate or
   an address that is not in the certificate fails the connection with a clear reason
   ("server certificate is not trusted"); the client keeps the error visible and never
   falls back to plain HTTP.
4. A server bound off loopback with accounts refuses to start without TLS unless
   `--insecure-allow-plaintext` (LAN trials) is given.

## Alternatives

* Pinning the server's public key (SPKI hash): equally strong, but rotation then needs a
  client update or a second pin; the CA keeps rotation on the server side.
* Let's Encrypt IP certificates: short-lived, needs the ACME flow on the home server; can
  be used later without client changes (web PKI path).
* Self-signed server certificate as the trust anchor: rotation means re-distributing it.

## Consequences

* `docs/DEPLOY-TLS.md` describes generating, installing and rotating certificates.
* S7 (installer) bundles `ca.pem` and the server address; S8 verifies on the target server.
