# Security Policy

## Supported versions

Only the latest tagged release and the current `main` branch receive
security fixes. The crate is published to crates.io as `onvif-device-rs`;
older tags are end-of-life — upgrade is a version bump.

| Version | Supported |
|---------|-----------|
| latest tag | ✅ |
| `main` | ✅ (fixes land here first, PR-only) |
| older tags | ❌ end-of-life |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

- Prefer a private [GitHub security advisory](https://github.com/mickeyzzc/onvif-rs/security/advisories/new).
- Alternatively email the maintainer (see the GitHub profile); include
  `onvif-rs security` in the subject.

Include reproduction details (SOAP request/response bodies, captures)
when possible. You will get an acknowledgement within 7 days. Urgent
fixes are released as patch versions out of band; otherwise they ship
with the next capability package (merge ≠ release — see
`CONTRIBUTING.md`).

## Scope

Security-relevant surfaces maintained by this library:

- SOAP/XML parsing of **untrusted** client requests, with bounded bodies,
  panic isolation per handler, and replay-guarded authentication.
- WS-UsernameToken verification (digest + plaintext, fail-closed since
  v0.3.0) and per-source auth lockout.
- WS-Discovery responder (multicast, untrusted by definition).

Out of scope: consumers' TLS termination, credential storage, HTTP server
hardening beyond the library's own limits (those belong to the embedding
application).

## Safe harbor

Fuzzing and penetration testing against your own deployments, and
submitting crashers found by the in-repo property tests, are welcome —
please still report anything that survives them privately first.
