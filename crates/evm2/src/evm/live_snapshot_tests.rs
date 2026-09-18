//! Real CALL, CREATE and stateful-precompile settlement after live replacement.

use super::{
    AccountInfo, InMemoryDB, SnapshotLogs, TransactionSnapshot,
    precompile::{PrecompileOutput, PrecompileProvider},
};
use crate::{
    BaseEvmTypes, Evm, Inspector, PrecompileError, SpecId,
    bytecode::Bytecode,
    env::{BlockEnvExt, TxEnvExt},
    interpreter::{
        GasTracker, Host, InstrStop, Interpreter, Message, MessageExt, MessageKind, Word, op,
    },
    precompiles::PrecompileHalt,
    registry::TxRegistry,
    test_utils::{legacy_bytecode, push_all},
    utils::address_to_word,
};
use alloc::vec;
use alloy_primitives::{Address, Bytes};
use std::sync::{Arc, Mutex};

const TARGET: Address = Address::with_last_byte(0x40);
const PRECOMPILE: Address = Address::with_last_byte(0x41);
const SLOT_ACCOUNT: Address = Address::with_last_byte(0x42);

type SharedSnapshot = Arc<Mutex<Option<TransactionSnapshot>>>;

struct Capture(SharedSnapshot);

impl Inspector<BaseEvmTypes> for Capture {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        let mut snapshot = self.0.lock().unwrap();
        if snapshot.is_none() {
            *snapshot = Some(interp.host().state().transaction_snapshot());
        }
    }
}

struct RestorePrecompile {
    snapshot: SharedSnapshot,
    outcome: u8,
}

impl PrecompileProvider<BaseEvmTypes> for RestorePrecompile {
    fn contains(&self, address: &Address) -> bool {
        *address == PRECOMPILE
    }

    fn execute(
        &mut self,
        evm: &mut Evm<'_, BaseEvmTypes>,
        _message: &Message,
        _gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        let snapshot = self.snapshot.lock().unwrap();
        evm.state_mut()
            .restore_transaction_snapshot(snapshot.as_ref().unwrap(), SnapshotLogs::Retain)
            .unwrap();
        evm.state_mut()
            .storage_slot(&SLOT_ACCOUNT, Word::ZERO, false)
            .unwrap()
            .write(Word::from(33));
        Some(match self.outcome {
            0 => Ok(PrecompileOutput::default()),
            1 => Err(PrecompileError::Revert(Bytes::from_static(b"restored"))),
            _ => Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)),
        })
    }
}

#[test]
fn call_create_and_precompile_settlement_after_replacement() {
    for kind in [MessageKind::Call, MessageKind::Create] {
        for precompile_outcome in 0..3 {
            for parent_outcome in 0..3 {
                let snapshot = SharedSnapshot::default();
                let mut database = InMemoryDB::default();
                database.insert_account_info(&SLOT_ACCOUNT, AccountInfo::default());
                database.insert_account_storage(&SLOT_ACCOUNT, &Word::ZERO, &Word::from(5));
                let mut evm = Evm::new(
                    SpecId::CANCUN,
                    BlockEnvExt::default(),
                    TxRegistry::new(),
                    database,
                    RestorePrecompile { snapshot: snapshot.clone(), outcome: precompile_outcome },
                );
                evm.set_inspector(Capture(snapshot));
                let mut code = vec![op::PUSH1, 11, op::PUSH0, op::SSTORE];
                push_all(
                    &mut code,
                    [
                        Word::ZERO,
                        Word::ZERO,
                        Word::ZERO,
                        Word::ZERO,
                        Word::ZERO,
                        address_to_word(&PRECOMPILE),
                        Word::from(50_000),
                    ],
                );
                code.extend([op::CALL, op::POP]);
                code.extend(match parent_outcome {
                    0 => vec![op::STOP],
                    1 => vec![op::PUSH0, op::PUSH0, op::REVERT],
                    _ => vec![0xfe],
                });
                let mut message = MessageExt {
                    kind,
                    destination: TARGET,
                    code_address: TARGET,
                    gas_limit: 200_000,
                    code: legacy_bytecode(code),
                    ..MessageExt::default()
                };
                let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
                let expected_stop = match parent_outcome {
                    0 => InstrStop::Stop,
                    1 => InstrStop::Revert,
                    _ => InstrStop::InvalidOpcode,
                };
                assert_eq!(
                    result.stop, expected_stop,
                    "{kind:?}/{precompile_outcome}/{parent_outcome}"
                );
                let expected = if parent_outcome == 0 && precompile_outcome == 0 { 33 } else { 5 };
                assert_eq!(
                    evm.state_mut()
                        .storage_slot(&SLOT_ACCOUNT, Word::ZERO, false)
                        .unwrap()
                        .current(),
                    Word::from(expected)
                );
                evm.state_mut().clear_transaction_state();
                // Clearing asserts that every frame retired before another transaction.
                let mut followup = MessageExt {
                    destination: SLOT_ACCOUNT,
                    code_address: SLOT_ACCOUNT,
                    gas_limit: 20_000,
                    code: Bytecode::new_legacy(Bytes::from_static(&[op::STOP])),
                    ..MessageExt::default()
                };
                assert_eq!(
                    Host::execute_message(&mut evm, &TxEnvExt::default(), &mut followup).stop,
                    InstrStop::Stop
                );
                evm.state_mut().clear_transaction_state();
            }
        }
    }
}
