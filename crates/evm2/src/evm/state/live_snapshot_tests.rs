//! Native live-replacement settlement regressions.

use super::{AccountInfo, SnapshotEpochMismatch, SnapshotLogs, State};
use crate::{SpecId, Version, evm::CacheDB, interpreter::Word};
use alloy_primitives::{Address, Log};

const ACCOUNT: Address = Address::with_last_byte(0x40);

fn state() -> State<'static> {
    let mut db = CacheDB::default();
    db.insert_account_info(&ACCOUNT, AccountInfo::default());
    db.insert_account_storage(&ACCOUNT, &Word::ZERO, &Word::from(5));
    State::new(db)
}

fn write(state: &mut State<'_>, value: u64) {
    state.storage_slot(&ACCOUNT, Word::ZERO, false).unwrap().write(Word::from(value));
}

fn value(state: &mut State<'_>) -> Word {
    state.storage_slot(&ACCOUNT, Word::ZERO, false).unwrap().current()
}

#[test]
fn newer_frame_revert_retains_restoration_and_ancestor_undo() {
    let mut state = state();
    let features = Version::base(SpecId::CANCUN).features;
    let parent = state.enter_frame();
    write(&mut state, 11);
    let snapshot = state.transaction_snapshot();
    write(&mut state, 22);
    let child = state.enter_frame();
    write(&mut state, 33);
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    // The obsolete child cursor is longer than the restored journal. Its new
    // boundary follows identity, not the accidental numeric length of history.
    state.rollback_frame(child, features);
    assert_eq!(value(&mut state), Word::from(11));
    state.rollback_frame(parent, features);
    assert_eq!(value(&mut state), Word::from(5));
}

#[test]
fn in_bounds_obsolete_cursor_does_not_revert_into_unrelated_history() {
    let mut state = state();
    let features = Version::base(SpecId::CANCUN).features;
    let parent = state.enter_frame();
    write(&mut state, 11);
    let snapshot = state.transaction_snapshot();
    write(&mut state, 22);
    let child = state.enter_frame();
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    // Grow past the old cursor so mere bounds validation would incorrectly pass.
    for value in 30..40 {
        write(&mut state, value);
    }
    state.rollback_frame(child, features);
    assert_eq!(value(&mut state), Word::from(11));
    state.rollback_frame(parent, features);
    assert_eq!(value(&mut state), Word::from(5));
}

#[test]
fn snapshot_from_reverted_child_retains_only_still_active_ancestor_boundaries() {
    let mut state = state();
    let features = Version::base(SpecId::CANCUN).features;
    let parent = state.enter_frame();
    write(&mut state, 11);
    let child = state.enter_frame();
    write(&mut state, 22);
    let snapshot = state.transaction_snapshot();
    state.rollback_frame(child, features);
    assert_eq!(value(&mut state), Word::from(11));
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    assert_eq!(value(&mut state), Word::from(22));
    let new_child = state.enter_frame();
    write(&mut state, 33);
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    write(&mut state, 44);
    state.rollback_frame(new_child, features);
    assert_eq!(value(&mut state), Word::from(22));
    assert_eq!(state.frames.len(), 1);
    state.rollback_frame(parent, features);
    assert_eq!(value(&mut state), Word::from(5));
}

#[test]
fn retained_logs_obey_actual_frame_entry_boundaries() {
    let mut state = state();
    let features = Version::base(SpecId::CANCUN).features;
    let parent = state.enter_frame();
    state.logs.push(Log::default());
    let snapshot = state.transaction_snapshot();
    state.logs.push(Log::default());
    let child = state.enter_frame();
    state.logs.push(Log::default());
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    assert_eq!(state.logs.len(), 3);
    state.logs.push(Log::default());
    state.rollback_frame(child, features);
    assert_eq!(state.logs.len(), 2);
    state.rollback_frame(parent, features);
    assert!(state.logs.is_empty());
}

#[test]
fn restoring_logs_rebases_newer_frame_log_boundary() {
    let mut state = state();
    let features = Version::base(SpecId::CANCUN).features;
    let parent = state.enter_frame();
    state.logs.push(Log::default());
    let snapshot = state.transaction_snapshot();
    state.logs.push(Log::default());
    let child = state.enter_frame();
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Restore).unwrap();
    state.logs.push(Log::default());
    state.rollback_frame(child, features);
    assert_eq!(state.logs.len(), 1);
    state.rollback_frame(parent, features);
    assert!(state.logs.is_empty());
}

#[test]
fn snapshots_reject_other_state_or_cleared_transaction_without_partial_mutation() {
    let mut source = state();
    write(&mut source, 11);
    let snapshot = source.transaction_snapshot();
    let mut other = state();
    write(&mut other, 22);
    assert_eq!(
        other.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain),
        Err(SnapshotEpochMismatch)
    );
    assert_eq!(value(&mut other), Word::from(22));
    source.clear_transaction_state();
    write(&mut source, 33);
    assert_eq!(
        source.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain),
        Err(SnapshotEpochMismatch)
    );
    assert_eq!(value(&mut source), Word::from(33));
}

#[test]
#[should_panic(expected = "raw checkpoint invalidated by state replacement")]
fn raw_checkpoint_rejected_after_restore_even_when_cursor_is_in_bounds() {
    let mut state = state();
    let raw = state.checkpoint();
    let snapshot = state.transaction_snapshot();
    state.restore_transaction_snapshot(&snapshot, SnapshotLogs::Retain).unwrap();
    state.rollback(raw, Version::base(SpecId::CANCUN).features);
}

#[test]
#[should_panic(expected = "raw checkpoint invalidated by state replacement")]
fn raw_checkpoint_rejected_after_transaction_clear() {
    let mut state = state();
    let raw = state.checkpoint();
    state.clear_transaction_state();
    state.rollback(raw, Version::base(SpecId::CANCUN).features);
}
