# Code signing policy

Free code signing provided by [SignPath.io](https://signpath.io), certificate by [SignPath Foundation](https://signpath.org).

## What is signed

Only the Windows `tpfs.exe` (x64 and ARM64) from release builds of the public repository
[kral/tpfs](https://github.com/kral/tpfs). The binaries are built by GitHub Actions
(`.github/workflows/release.yml`) from the tagged source, uploaded as a build artifact and submitted
to SignPath from that same workflow. Nothing built on a developer machine is signed.

## Roles

- Owner, committer and approver: Jiří Král ([@kral](https://github.com/kral)). Every signing request is approved by the owner.

Multi-factor authentication is required for the GitHub account and for the SignPath account used
for this project.

## Privacy

tpfs works only on your local files. It sends no data anywhere: no telemetry, no update checks, no
network access.

## Attribution

Free code signing provided by SignPath.io, certificate by SignPath Foundation
