# Explicit archive behavior

User intent: an explicit archive request takes precedence over all bot-side
pending work and old processing records. Native writer ownership remains the
app-server's decision.

- A1: held/unconfirmed ingress, queue/intake work, response receipts, active
  turns and old execution holds do not prevent the native archive request.
- A2: archive does not resume a thread or require an idle read first.
- A3: native active-writer rejection remains an error; no retry, fork or new turn.
- A4: retain pending inputs as evidence, fenced from automatic execution after
  archive. Do not delete conversation data or synthesize delivery confirmation.
- A5: retain exact user/target binding, bounded transport deadlines, persisted
  result verification and truthful handling of unknown results.

Work mode: focused regression tests first, then implementation. Tests use the
existing native RPC fixture and temporary databases, never the live bot store.
