# Security Policy

## Supported versions

| Version | Supported |
| --- | --- |
| `main` (latest commit) | Yes |
| Latest tagged release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please do not report security vulnerabilities in public issues, discussions, or
pull requests.

Use GitHub private vulnerability reporting:

https://github.com/artur282/oxidezap/security/advisories/new

Include:

- A description of the issue and the affected version or commit.
- Reproduction steps or a proof of concept.
- The impact you believe it has.
- Any suggested fix or mitigation.

If private reporting is unavailable, open a public issue that only asks for a
private contact channel. Do not include exploit details in that issue.

## Scope

In scope:

- WhatsApp Web session and credential handling, including account isolation.
- The application lock and settings import/export paths.
- Download handling, URL argument handling, and the `whatsapp://` handler.
- The boundary between the GPUI shell and the WebKitGTK view.
- Proxy configuration and any place where user-controlled data reaches a
  privileged operation.

Out of scope:

- Vulnerabilities in WhatsApp Web, WebKitGTK, GPUI, or other dependencies that
  are already fixed upstream. Report those upstream; this project will update.
- Attacks that require a compromised local account, physical access, or an
  already-root shell, unless the application is supposed to cross that
  privilege boundary.
- Social engineering, spam, and denial-of-service against GitHub.

## Response

This is a personal open-source project maintained on a best-effort basis. We aim
to acknowledge reports within 7 days and to publish a fix or mitigation as soon
as practical. We will credit reporters in the advisory unless you ask us not to.

## Safe harbor

We will not pursue legal action for good-faith security research that follows
this policy, stays within the scope above, and avoids privacy violations, data
destruction, and service disruption.
