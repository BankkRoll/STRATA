# Security policy

Strata is provided as-is and not actively maintained; security reports are handled on a
best-effort basis, with no response-time commitment.

## Reporting a vulnerability

Report privately through GitHub:
[**Security → Report a vulnerability**](https://github.com/BankkRoll/STRATA/security/advisories/new).
Please don't open a public issue for a vulnerability.

Include the affected component, steps to reproduce, and the impact you expect. A minimal proof
of concept helps.

## Scope

The parts where a bug matters most:

- **The elevated helper** (`strata-helper`): anything that lets an unelevated or untrusted
  process make it read, write or delete what the user couldn't.
- **The named pipe** (`strata-ipc`): bypassing the DACL, integrity label, peer signature check,
  handshake or rate limit; pipe-name squatting; malformed frames that crash or confuse either
  end.
- **Deletion safety** (`strata-clean`): any path, spelling, link, junction, mount point or race
  that gets past the never-delete list or pre-flight re-verification, or deletes something other
  than what the user confirmed.
- **Parsers** (`strata-ntfs`, rule packs, cache files): panics, hangs or memory blowups on
  malformed input.

Out of scope: issues that need an attacker who already has administrator rights, and unsigned
local builds you made yourself.
