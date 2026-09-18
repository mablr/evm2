# Live transaction snapshots

`State::transaction_snapshot` captures a reusable native transaction-layer snapshot.
`State::restore_transaction_snapshot` can restore it during an inspector callback or
stateful precompile, while engine CALL/CREATE frames remain suspended.

A saved journal cursor alone is insufficient: restoration can replace the history
that an active frame expects to undo. The engine therefore registers each active
frame with an identity and rollback boundaries. On restoration, captured ancestors
keep their saved boundaries; active frames absent from the snapshot receive the
restored boundary. Returned frames are not recreated. Rollback of a frame absent
from the snapshot undoes its writes made after restoration without undoing
restoration itself; a captured ancestor can still unwind the restored history.

For example, A writes 11 and captures, then writes 22 and calls B. B restores the
snapshot and reverts. A observes 11, not 22; if A subsequently reverts, its captured
ancestor boundary still restores the pre-A state. Checkpoints are matched by frame
identity, including when an old numeric cursor happens to fit the replaced history.

Snapshots include account/storage originals and flags, access warmth, transient
storage, the prewarm set, journal, logs and active engine-frame boundaries.
`SnapshotLogs::Retain` keeps current logs and actual frame-entry log boundaries;
`Restore` restores captured logs and adjusts the boundaries to the captured history.
Diagnostic inspector records are owned independently by the embedding application.

Snapshots preserve actual execution continuation: stack, memory, program counter,
depth and gas already consumed are not rewound. Capturing/restoring copies native
transaction state and frame metadata; ordinary frame entry/exit only maintains the
small active-frame registry. This is a complexity property, not a benchmark claim.

## Boundaries

- Snapshots belong to one `State` instance and one transaction lifecycle. Restoring
  after `clear_transaction_state`, or into another state, returns
  `SnapshotEpochMismatch` without modifying state. Setup/test snapshots across
  transactions require an additional integration design.
- Backing database and accepted-overlay contents are not captured. The application
  must keep those sources compatible. This API does not implement fork switching.
- Block/configuration/transaction environments, interpreter gas/refund counters and
  cheatcode companion state are not captured. The embedding application owns them.
- Public raw `StateCheckpoint` values are invalidated by snapshot restoration and
  transaction clearing, and `rollback` rejects them. Only engine-managed CALL,
  CREATE and precompile scopes are automatically coordinated. Custom transaction
  handlers retaining raw checkpoints must not restore snapshots in those scopes;
  a public coordinated scope API remains separate work.
- Full Foundry snapshot/fork/isolation behavior, Amsterdam state-gas accounting and
  tracing parity are not established by this API.

Native regressions cover ancestor/child rollback, divergent and in-bounds stale
history, reuse after a child has reverted, both log policies, identity/lifecycle
rejection, and CALL/CREATE with stateful-precompile success, revert and halt.

`StateCheckpoint::new` is no longer public: callers obtain generation-bound raw
checkpoints from `State::checkpoint`. A manually constructed numeric cursor cannot
identify which replacement history it belongs to. This deliberately changes the
unstable API rather than retaining an unchecked constructor.
