# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Clawshake, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, use [GitHub's private vulnerability reporting](https://github.com/lidn12/clawshake/security/advisories/new) to submit your report. You will receive an acknowledgement within 48 hours.

### What to include

- Description of the vulnerability
- Steps to reproduce
- Affected versions
- Any potential impact

## Scope

Security issues we're especially interested in:

- **Identity & keys** — weaknesses in Ed25519 identity handling or key storage
- **Permissions** — bypasses of the peer permission system
- **P2P protocol** — attacks on the libp2p transport, relay, or DHT layer
- **Tool invocation** — unauthorized remote tool execution
- **Manifest injection** — crafted manifests that escape intended sandboxing

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
