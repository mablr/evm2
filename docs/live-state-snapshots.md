# Live transaction snapshots

`State::snapshot` captures the in-memory state without its backing database.
`State::restore_snapshot` can restore it during an inspector callback or stateful
precompile, while engine CALL/CREATE frames remain suspended. The same snapshot
can also be restored into a later transaction or another `State` instance.

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

Snapshots include the accepted cache, account/storage originals and flags, access
warmth, transient storage, the prewarm set, journal, logs and active engine-frame
boundaries.
`SnapshotLogs::Retain` keeps current logs and actual frame-entry log boundaries;
`Restore` restores captured logs and adjusts the boundaries to the captured history.
Diagnostic inspector records are owned independently by the embedding application.

Snapshots preserve actual execution continuation: stack, memory, program counter,
depth and gas already consumed are not rewound. Capturing/restoring copies native
transaction state and frame metadata; ordinary frame entry/exit only maintains the
small active-frame registry. This is a complexity property, not a benchmark claim.

## Boundaries

- The backing database is not captured or replaced. The application must keep it
  compatible with the snapshot, including when switching forks. The accepted
  overlay is captured.
- Block/configuration/transaction environments, interpreter gas/refund counters and
  cheatcode companion state are not captured. The embedding application owns them.
- Only engine-managed CALL, CREATE and precompile scopes are automatically
  coordinated. Custom transaction handlers retaining raw checkpoints across a
  restoration must coordinate their own rollback boundaries.
- Full Foundry snapshot/fork/isolation behavior, Amsterdam state-gas accounting and
  tracing parity are not established by this API.

Native regressions cover ancestor/child rollback, divergent and in-bounds stale
history, reuse after a child has reverted, both log policies, restoration across
state instances and transactions, and CALL/CREATE with stateful-precompile success,
revert and halt.
