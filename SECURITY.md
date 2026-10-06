# Security policy

structured-zstd decodes untrusted input, so a crash, hang, out-of-bounds access
or unbounded allocation on any input is a security issue.

## Reporting a vulnerability

Report it privately through
[GitHub private vulnerability reporting](https://github.com/structured-world/structured-zstd/security/advisories/new),
not in a public issue. Include the input that triggers it (or how to build it),
the version or commit, the features enabled, and what you observed.

You will get an answer within a few days. A confirmed issue is fixed in a new
release, and the advisory is published with credit to you unless you prefer
otherwise.

## Supported versions

Fixes go into the latest release only.
