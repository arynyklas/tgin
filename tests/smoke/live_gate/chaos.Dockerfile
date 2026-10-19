# The live-gate soak's chaos sidecar: the Docker CLI and chaos.sh. See
# tests/smoke/live_gate.md.

# docker:29.7.2-cli (multi-arch index digest)
FROM docker:29.7.2-cli@sha256:3f4743208d2338c934d7b8bcfbe1bb54c0b2355c510ad5e0f31c0c4a54bd704e

COPY chaos.sh /chaos.sh
ENTRYPOINT ["sh", "/chaos.sh"]
