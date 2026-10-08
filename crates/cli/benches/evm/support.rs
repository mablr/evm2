use crate::fixture::Suites;
use criterion::{BatchSize, BenchmarkGroup, black_box, measurement::WallTime};
use evm2::{
    BaseEvmTypes, Evm, Inspector, Precompiles, SpecId,
    env::BlockEnv,
    ethereum::{RecoveredTxEnvelope, ethereum_tx_registry},
    evm::InMemoryDB,
    interpreter::{Interpreter, Message, MessageResult, OpcodeSet},
};
use evm2_cli::evm_bench::BenchCase;
use std::{borrow::Cow, env};

type BenchEvm = Evm<'static, BaseEvmTypes>;

#[derive(Clone, Debug)]
pub(crate) struct PreparedBench {
    name: Cow<'static, str>,
    spec: SpecId,
    block: BlockEnv,
    db: InMemoryDB,
    tx: RecoveredTxEnvelope,
}

impl PreparedBench {
    pub(crate) fn load(bench: &BenchCase, suites: &Suites) -> Self {
        let spec = bench.transaction_spec().expect("transaction benchmark must have a spec");
        let suite = suites.get(bench.fixture_path);
        let case = suite.case(&bench.name, spec);
        Self {
            name: bench.name.clone(),
            spec,
            block: case.block(),
            db: case.state(),
            tx: case.tx(spec),
        }
    }

    pub(crate) fn sanity_check(&self) {
        let mut runner = Runner::new(self);
        let _ = runner.run().unwrap_or_else(|err| {
            panic!("{} benchmark transaction must execute: {err:?}", self.name)
        });
    }

    pub(crate) fn bench(&self, group: &mut BenchmarkGroup<'_, WallTime>) {
        group.bench_function(self.name.as_ref(), |b| {
            b.iter_batched(
                || Runner::new(self),
                |mut runner| {
                    black_box(runner.run().unwrap_or_else(|err| {
                        panic!("{} benchmark transaction must execute: {err:?}", self.name)
                    }))
                },
                BatchSize::SmallInput,
            );
        });
    }
}

struct Runner {
    evm: BenchEvm,
    tx: RecoveredTxEnvelope,
}

impl Runner {
    fn new(prepared: &PreparedBench) -> Self {
        let mut evm = new_evm(prepared.spec, prepared.block, prepared.db.clone());
        match env::var("EVM2_BENCH_INSPECTOR").as_deref() {
            Ok("full") => {
                evm.set_inspector(CallOnlyInspector { opcodes: OpcodeSet::ALL, calls: 0 })
            }
            Ok("empty") => {
                evm.set_inspector(CallOnlyInspector { opcodes: OpcodeSet::EMPTY, calls: 0 })
            }
            Ok("none") | Err(_) => {}
            Ok(value) => {
                panic!("unknown EVM2_BENCH_INSPECTOR: {value}; expected none, full, or empty")
            }
        }
        Self { evm, tx: prepared.tx.clone() }
    }

    fn run(&mut self) -> evm2::registry::HandlerResult<evm2::TxResult> {
        self.evm.transact(&self.tx).map(evm2::ExecutedTx::commit)
    }
}

fn new_evm(spec: SpecId, block: BlockEnv, db: InMemoryDB) -> BenchEvm {
    Evm::new(spec, block, ethereum_tx_registry(spec), db, Precompiles::base(spec))
}

// Opt in with EVM2_BENCH_INSPECTOR=full or empty to measure a call-only inspector.
struct CallOnlyInspector {
    opcodes: OpcodeSet,
    calls: usize,
}

impl Inspector<BaseEvmTypes> for CallOnlyInspector {
    fn call(
        &mut self,
        _interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        _message: &mut Message<BaseEvmTypes>,
    ) -> Option<MessageResult<BaseEvmTypes>> {
        self.calls += 1;
        None
    }

    fn inspected_opcodes(&self) -> OpcodeSet {
        self.opcodes
    }
}
