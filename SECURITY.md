# Security Policy

## Supported versions

No released version is supported yet. tgin 1.0 will be the first supported
release; until then, security fixes land on the `main` branch. A support
policy will be announced with 1.0.

## Reporting a vulnerability

Please do not report vulnerabilities in public issues, discussions or pull
requests.

Report them privately through GitHub's private vulnerability reporting:
open the [Security tab](https://github.com/arynyklas/tgin/security) and choose
**Report a vulnerability**, or go straight to
<https://github.com/arynyklas/tgin/security/advisories/new>.

Please include:

- the affected version (`tgin --version`) and how tgin was installed
  (release archive, installer, container image or source build);
- the relevant part of your configuration, with bot tokens and secrets removed;
- steps to reproduce and the impact you observed or expect.

The discussion, the fix and the coordinated disclosure happen in the private
advisory. Once a fixed release is out, the advisory is published.

## Verifying releases

The per-platform release archives and the container image carry GitHub build
provenance attestations. To check that an artifact was built from this
repository by the workflow that publishes it, use the
[GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify tgin-x86_64-unknown-linux-gnu.tar.xz --repo arynyklas/tgin \
  --signer-workflow arynyklas/tgin/.github/workflows/release.yml
gh attestation verify oci://ghcr.io/arynyklas/tgin:<version> --repo arynyklas/tgin \
  --signer-workflow arynyklas/tgin/.github/workflows/image.yml
```
