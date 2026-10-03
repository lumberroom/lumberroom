# Security policy

## Supported versions

`main` only. Migrations are forward-only, so there is no older line of the schema a fix
could land on; upgrading past a vulnerable commit is the only supported remediation.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting rather than a public issue:
[github.com/lumberroom/lumberroom/security/advisories/new](https://github.com/lumberroom/lumberroom/security/advisories/new).

Include:

- the affected commit or tag
- the component (server, CLI, authorization server, a specific adapter)
- steps to reproduce, or a proof of concept
- what an attacker gains: read, write, or something else

## What to expect

- Acknowledgement within 5 business days.
- A fix lands on `main`, not a backport.
- The fix gets a line in the changelog once merged. Credit is given if you want it and
  withheld if you don't.

No PGP key and no bounty program. Report over the channel above; email and other
back-channels won't get a faster response.
