# Runtime provenance and writer-conflict retries

This change preserves Codex executable discovery, including commit `50a0272`'s
fallback from an incomplete managed Codex installation. It does not choose a
different Codex version, change Discord identifiers, migrate the queue schema,
or turn restart into an automatic source update.

## Build and identify an artifact

Use `scripts/Build-Runtime.ps1 -TargetDirectory <external-build-directory>` for a
source build. `install.ps1` and the source build in `update.ps1` use the same
provenance helper. The helper passes the exact working directory to Cargo and
captures Git HEAD, HEAD tree, and clean/dirty state on every invocation.
`CDR_BUILD_SOURCE_STATE` is set only in the Cargo child. A before/after content
fingerprint refuses verification if source changes during the build.

`cdr-runtime.exe --build-info` prints JSON without reading `.env`, opening a DB,
or connecting to Discord. It reports schema version, compiled package version,
source commit, HEAD tree, and source state. A direct Cargo invocation without
the explicit build input reports `unknown`; Cargo cannot retain a prior known
revision after that input disappears. A dirty build's HEAD tree does not
identify its uncommitted contents. SHA-256 identifies the exact executable.

The source-build helper compares the executable's embedded metadata with the
current source invocation. It refuses to label an unrelated or unknown artifact
with the current checkout's commit. External `update.ps1 -SkipBuild` candidates
retain their own embedded provenance; the updater verifies their exact hash and
does not relabel them. The Rust repository's Windows ZIP publisher is not part
of Discord Lover's source updater.

The updater loads these modules from its separate source checkout even when the
target installation has older support files. Its resume journal pins the new
provenance and restart modules alongside the existing upgrade controller.

## Status and restart

The existing atomic `.codex_discord_rust.runtime.lock` additionally records the
startup executable path, SHA-256, embedded build information and OS process
creation identity. This is computed once at startup. Status displays running
startup evidence, the current disk artifact, and checkout source separately.
A legacy or stale record leaves the running source unknown.

The restart entry captures the installed artifact hash before any deferred or
independent force-worker handoff. `-ExpectedBinaryHash <SHA256>` can require a
specific artifact. `-ExpectedSourceCommit <full-SHA>` requires matching embedded
source; dirty source also requires an explicit binary hash. Source-only restart
is allowed only for a known clean build.

Normal preparation writes the artifact and drain fence into the existing launch
journal's `preparing` phase before sealing intake. A crash in that phase resumes
preparation; it cannot authorize a launch without the exact sealed acknowledgement
and published restart marker. The original process may exit after that marker.
The existing unknown-launch and child-ownership protections still apply.

Artifact read handles prevent replacement during launch. A modern replacement
must publish the same artifact in its process-bound startup record and a fresh
heartbeat before completion. Completion receipts retain the artifact and are
rechecked on reentry. Force restart carries the same hash through its detached
worker and journal. Legacy deployment recovery retains its compatibility and
ownership rules; restored baseline receipts explicitly report source unknown.

No path automatically downloads, builds, selects or rolls back another runtime.
Use the existing verified deployment procedure to install a reviewed artifact,
then verify that artifact's actual running identity.

## Writer conflict retry

Only the definite serialized `-32600` rejection of `thread/resume` stating that
the thread already has an active writer receives delays of 5, 10 and 15 seconds,
capped at 15. Generic failures keep 30/60/120/240/480/900 seconds. Ambiguous starts,
execution holds and generation ownership rules are unchanged.

Both persisted and in-memory gates use that policy. Up to 128 disposable target
hints feed the existing bounded completion scheduler when due. They retain no
target lease or native execution slot while waiting. Execution revalidates the
original pending head, generation, attempt, update timestamp and error, then
uses the existing recovery gates. A cancelled, changed or held request cannot
authorize a different request through its old hint. A full cache falls back to
the unchanged 30-second paged Queue scan; 15 seconds is a retry delay, not an
unconditional completion guarantee under arbitrary load.

Focused tests exercise the production scheduler without new ingress, cold count-9
recovery, stale hints, generic backoff, PID reuse, changed artifacts, restart
interruption boundaries, force-worker pins, and same-cache build metadata changes.
