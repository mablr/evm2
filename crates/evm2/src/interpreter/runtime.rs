use super::{
    BytecodeRef, Gas, Host, InstrStop, Memory, Message, MessageKind, MessageResult, Pc, Result,
    StackBacking, StackMut, StackRef, Word,
};
use crate::{
    EvmTypesHost, ExecutionConfig, ExecutionError, HostError, InterpreterRunner, SpecId, Version,
    bytecode::Bytecode,
    env::TxEnv,
    evm::{NonStaticAny, inspector::Inspector},
    interpreter::dispatch::{self, InstrTable},
    trustme,
    version::{EvmFeatures, GasParams},
};
use alloc::{boxed::Box, vec::Vec};
use alloy_primitives::{Address, B256, Bytes};
use core::{any::TypeId, cmp::min, fmt, hint::cold_path, ops::Range, ptr::NonNull};
use derive_where::derive_where;

/// EVM interpreter.
#[derive_where(Debug)]
pub struct Interpreter<'frame, 'host, T: EvmTypesHost> {
    pub(in crate::interpreter) bytecode: Bytecode,
    // Borrows the immutable allocations owned by `bytecode`. Cleared before replacing it;
    // accessors must shorten the erased lifetime to the borrow of this interpreter.
    bytecode_ref: Option<BytecodeRef<'static>>,
    pub(in crate::interpreter) memory: Memory,
    pub(in crate::interpreter) return_data: Bytes,

    pub(in crate::interpreter) pc: *const u8,
    output: Range<u32>,
    #[derive_where(skip)]
    tx_env: Option<&'frame TxEnv<T>>,
    #[derive_where(skip)]
    message: Option<&'frame Message<T>>,
    host: Option<NonNull<T::Host<'host>>>,
    host_type_id: Option<TypeId>,
    host_bound: bool,
    inspector: Option<NonNull<dyn Inspector<T> + 'host>>,
    version: Option<&'frame Version>,
    pub(in crate::interpreter) stack_len: usize,
    #[derive_where(skip)]
    pub(in crate::interpreter) stack: Box<StackBacking>,

    pub(in crate::interpreter) gas: Gas,
    pub(in crate::interpreter) result: Result,
    error: Option<ExecutionError>,
    spec: SpecId,
    features: EvmFeatures,
    is_static: bool,
    pending_message: Option<PendingMessage<T>>,
}

// SAFETY: The interpreter's internal pointers are always valid. `pc` and `bytecode_ref` point into
// immutable owned bytecode and its jump table. Frame-local references are cleared before pooling,
// and host/inspector pointers are installed for
// execution and not used after the owning execution context is gone. The `Sync` bounds make the
// retained shared frame references safe to transfer between threads.
unsafe impl<T> Send for Interpreter<'_, '_, T>
where
    T: EvmTypesHost,
    T::MessageExt: Sync,
    T::TxEnvExt: Sync,
{
}

impl<'frame, 'host, T: EvmTypesHost> Interpreter<'frame, 'host, T> {
    /// Creates an interpreter from a transaction-global environment and a frame-local message,
    /// which carries the bytecode to run.
    pub fn new(tx_env: &'frame TxEnv<T>, message: &'frame Message<T>) -> Self {
        // SAFETY: Calling init right after.
        let mut interp = unsafe { Self::uninit() };
        interp.init(tx_env, message);
        interp
    }

    /// # Safety
    ///
    /// Must call `init` before use.
    unsafe fn uninit() -> Self {
        let bytecode = Bytecode::new();
        Self {
            pc: bytecode.original_byte_slice().as_ptr(),
            bytecode,
            bytecode_ref: None,
            stack_len: 0,
            gas: Gas::new(0),
            memory: Memory::new(),
            result: Ok(()),
            error: None,
            output: 0..0,
            tx_env: None,
            message: None,
            is_static: false,
            pending_message: None,
            return_data: Bytes::new(),
            host: None,
            host_type_id: None,
            host_bound: false,
            inspector: None,
            version: None,
            spec: SpecId::DEFAULT,
            features: EvmFeatures::empty(),
            // SAFETY: `MaybeUninit<Word>` does not need initialization.
            stack: unsafe { Box::new_uninit().assume_init() },
        }
    }

    /// Initializes this interpreter for a new frame, retaining reusable allocations.
    fn init(&mut self, tx_env: &'frame TxEnv<T>, message: &'frame Message<T>) {
        let bytecode = message.code.clone();
        let gas_limit = message.gas_limit;
        let is_static = message.caller_is_static || matches!(message.kind, MessageKind::StaticCall);
        self.pc = bytecode.original_byte_slice().as_ptr();
        self.bytecode_ref = None;
        self.bytecode = bytecode;
        self.stack_len = 0;
        self.gas = Gas::new_with_execution_gas_and_reservoir(gas_limit, message.reservoir);
        self.memory.clear();
        self.result = Ok(());
        self.error = None;
        self.output = 0..0;
        self.tx_env = Some(tx_env);
        self.message = Some(message);
        self.is_static = is_static;
        self.return_data = Bytes::new();
    }

    pub(crate) fn clear_frame_refs(&mut self) {
        self.tx_env = None;
        self.message = None;
        self.version = None;
        self.host = None;
        self.host_type_id = None;
        self.host_bound = false;
        self.inspector = None;
        self.pending_message = None;
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (Box<StackBacking>, usize, Gas, Memory, Range<u32>) {
        (self.stack, self.stack_len, self.gas, self.memory, self.output)
    }

    /// Returns output produced by `RETURN` or `REVERT`.
    #[inline]
    pub fn output(&self) -> &[u8] {
        let start = self.output.start as usize;
        self.memory.slice(start, self.output.len())
    }

    /// Returns the current frame output memory range.
    #[inline]
    pub const fn output_range(&self) -> &Range<u32> {
        &self.output
    }

    /// Sets the current frame output memory range.
    #[inline]
    pub const fn set_output(&mut self, output: Range<u32>) {
        self.output = output;
    }

    /// Returns the current bytecode-relative program counter.
    #[inline]
    pub fn pc(&self) -> usize {
        // SAFETY: `pc` is always in bounds of `bytecode`.
        unsafe { self.pc.offset_from(self.bytecode.original_byte_slice().as_ptr()) as usize }
    }

    /// Sets the current bytecode-relative program counter.
    ///
    /// # Panics
    ///
    /// Panics if `pc` is out of bounds of the active bytecode.
    #[inline]
    pub fn set_pc(&mut self, pc: usize) {
        let bytecode = self.bytecode.bytes_slice();
        assert!(pc < bytecode.len());
        self.pc = unsafe { bytecode.as_ptr().add(pc) };
    }

    /// Returns the current opcode.
    #[inline]
    pub const fn opcode(&self) -> u8 {
        // SAFETY: `pc` is always in bounds of `bytecode`.
        unsafe { *self.pc }
    }

    /// Returns the active bytecode.
    #[inline]
    pub fn bytecode(&self) -> BytecodeRef<'_> {
        self.bytecode_ref.unwrap_or_else(|| BytecodeRef::new(&self.bytecode))
    }

    /// Returns the original active bytecode bytes.
    #[inline]
    pub fn original_bytecode(&self) -> Bytes {
        self.bytecode.original_bytes()
    }

    /// Calculates or returns the cached hash of the original active bytecode.
    #[inline]
    pub fn original_bytecode_hash(&self) -> B256 {
        self.bytecode.hash_slow()
    }

    /// Returns the current operand stack.
    #[inline]
    pub const fn stack(&self) -> StackRef<'_> {
        StackRef::new(&self.stack, self.stack_len)
    }

    /// Returns the current mutable operand stack.
    #[inline]
    pub const fn stack_mut(&mut self) -> StackMut<'_> {
        StackMut { stack: &mut self.stack, len: &mut self.stack_len }
    }

    /// Returns the current gas state.
    #[inline]
    pub const fn gas(&self) -> Gas {
        self.gas
    }

    /// Returns a reference to the current gas state.
    #[inline]
    pub const fn gas_mut(&mut self) -> &mut Gas {
        &mut self.gas
    }

    /// Sets the current interpreter gas state.
    #[inline]
    pub const fn set_gas(&mut self, gas: Gas) {
        self.gas = gas;
    }

    /// Returns the current linear memory.
    #[inline]
    pub const fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Returns the current mutable linear memory.
    #[inline]
    pub const fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    /// Returns the current interpreter result.
    #[inline]
    pub const fn result(&self) -> Result {
        self.result
    }

    /// Sets the interpreter result.
    #[inline]
    pub const fn set_result(&mut self, result: Result) {
        self.result = result;
    }

    /// Sets the current instruction result to `stop`.
    #[inline]
    pub const fn set_stop(&mut self, stop: InstrStop) {
        self.set_result(Err(stop));
    }

    /// Returns the active frame-local call/create message.
    #[inline]
    pub const fn message(&self) -> &'frame Message<T> {
        // SAFETY: `message` is initialized before inspected execution starts.
        unsafe { self.message.unwrap_unchecked() }
    }

    #[inline]
    #[doc(hidden)]
    pub const fn message_field_offset_for_jit() -> usize {
        core::mem::offset_of!(Self, message)
    }

    /// Returns the cached transaction-global environment.
    #[inline]
    pub const fn tx_env(&self) -> &'frame TxEnv<T> {
        // SAFETY: `tx_env` is initialized before execution starts.
        unsafe { self.tx_env.unwrap_unchecked() }
    }

    /// Returns return data from the last call-like operation.
    #[inline]
    pub const fn return_data(&self) -> &Bytes {
        &self.return_data
    }

    /// Returns a mutable reference to return data from the last call-like operation.
    #[inline]
    pub const fn return_data_mut(&mut self) -> &mut Bytes {
        &mut self.return_data
    }

    /// Borrows the operand stack, linear memory, and host independently for inspection.
    #[inline]
    pub const fn stack_memory_host(&mut self) -> (StackRef<'_>, &[u8], &mut T::Host<'host>) {
        // SAFETY: As in `host`, the host pointer is initialized during execution and points
        // outside the interpreter's owned stack and memory.
        let host = unsafe { self.host_ptr().as_mut() };
        (self.stack(), self.memory.as_slice(), host)
    }

    /// Returns the host implementation.
    ///
    /// Suspended message callbacks must bind host access with [`Self::with_host`].
    /// Panics when host access is not bound.
    #[inline]
    pub const fn host(&mut self) -> &mut T::Host<'host> {
        // SAFETY: `host` is initialized at the beginning of inspected execution.
        unsafe { self.host_ptr().as_mut() }
    }

    /// Returns the active base specification ID.
    #[inline]
    pub const fn spec(&self) -> SpecId {
        self.spec
    }

    /// Returns the active runtime version data.
    #[inline]
    pub const fn version(&self) -> &Version {
        // SAFETY: `version` is initialized before execution starts.
        unsafe { self.version.unwrap_unchecked() }
    }

    /// Returns whether the active frame forbids state-changing operations.
    #[inline]
    pub const fn is_static(&self) -> bool {
        self.is_static
    }

    /// Sets the static-call flag for external execution adapters.
    #[inline]
    pub const fn set_static(&mut self, is_static: bool) {
        self.is_static = is_static;
    }

    /// Converts a host failure into an instruction stop, retaining external errors only in this
    /// frame.
    #[inline]
    pub fn fail(&mut self, error: impl Into<HostError>) -> InstrStop {
        match error.into() {
            HostError::Halt(stop) => stop,
            HostError::Execution(error) => {
                self.error = Some(error);
                InstrStop::FatalExternalError
            }
        }
    }

    /// Finishes a backend run, returning its owned error and clearing execution references.
    pub(crate) fn finish_run(&mut self, stop: InstrStop) -> Result<InstrStop, ExecutionError> {
        self.host = None;
        self.host_type_id = None;
        self.host_bound = false;
        self.inspector = None;
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        if stop.is_fatal() {
            return Err(ExecutionError::Fatal(
                "interpreter returned a fatal stop without an error".into(),
            ));
        }
        Ok(stop)
    }

    /// Runs the interpreter until it stops without opcode stepping.
    ///
    /// Retains any frame inspector installed by the EVM for external execution.
    #[inline]
    pub fn run(
        &mut self,
        config: &ExecutionConfig<T>,
        host: &mut T::Host<'host>,
    ) -> Result<InstrStop, ExecutionError> {
        self.run_inner(
            config.base_spec_id(),
            config.version(),
            host,
            self.inspector,
            config.instructions,
            false,
        )
    }

    /// Runs the interpreter until it stops with an execution inspector.
    #[inline]
    pub fn run_inspect(
        &mut self,
        config: &ExecutionConfig<T>,
        host: &mut T::Host<'host>,
        inspector: &mut (dyn Inspector<T> + 'host),
    ) -> Result<InstrStop, ExecutionError> {
        let step_opcodes = !inspector.inspected_opcodes().is_empty();
        self.run_with_inspector(config, host, inspector, step_opcodes, None)
    }

    /// Runs with retained inspector hooks and optional per-opcode callbacks.
    pub(crate) fn run_with_inspector(
        &mut self,
        config: &ExecutionConfig<T>,
        host: &mut T::Host<'host>,
        inspector: &mut (dyn Inspector<T> + 'host),
        step_opcodes: bool,
        runner: Option<&dyn InterpreterRunner<T>>,
    ) -> Result<InstrStop, ExecutionError> {
        let inspector = Some(NonNull::from(inspector));
        if !step_opcodes && let Some(runner) = runner {
            self.prepare_run(config.base_spec_id(), config.version(), host);
            self.inspector = inspector;
            if let Some(stop) = runner.run(config, self, host) {
                return self.finish_run(stop);
            }
        }
        self.run_inner(
            config.base_spec_id(),
            config.version(),
            host,
            inspector,
            if step_opcodes { config.inspect_instructions } else { config.instructions },
            step_opcodes,
        )
    }

    /// Prepares this interpreter for external execution, retaining its frame inspector.
    #[inline]
    #[doc(hidden)]
    pub fn prepare_run(&mut self, spec: SpecId, version: &Version, host: &mut T::Host<'host>) {
        if self.bytecode_ref.is_none() {
            // SAFETY: The view borrows immutable allocations retained by our owned `Bytecode`,
            // so moving the interpreter does not invalidate it. `init` clears the view before
            // replacing the owner, and accessors tie the returned view to `&self`.
            let bytecode = unsafe { trustme::decouple_lt(&self.bytecode) };
            self.bytecode_ref = Some(BytecodeRef::new(bytecode));
        }
        self.memory.set_memory_limit(version.memory_limit);
        // SAFETY: `version` remains alive for the duration of this interpreter run.
        let version = unsafe { trustme::decouple_lt(version) };
        self.host_type_id = Some(NonStaticAny::type_id(host));
        self.host = Some(NonNull::from(host));
        self.host_bound = true;
        self.version = Some(version);
        self.spec = spec;
        self.features = version.features;
    }

    #[inline(never)]
    fn run_inner(
        &mut self,
        spec: SpecId,
        version: &Version,
        host: &mut T::Host<'host>,
        inspector: Option<NonNull<dyn Inspector<T> + 'host>>,
        instructions: &InstrTable<T>,
        step_opcodes: bool,
    ) -> Result<InstrStop, ExecutionError> {
        self.prepare_run(spec, version, host);
        self.inspector = inspector;

        let stop = loop {
            if self.error.is_some() {
                break InstrStop::FatalExternalError;
            }
            if let Err(stop) = self.result {
                break stop;
            }
            if let Some(stop) = dispatch::run(self, instructions, step_opcodes) {
                break stop;
            }

            // Dispatch has returned: no instruction gas or stack references remain alive.
            let PendingMessage { mut message, completion } =
                self.pending_message.take().expect("dispatch yielded a message");
            let mut pc = Pc::new(self.pc);
            let op = pc.op();
            let tx_env = self.tx_env();
            let host_type_id = self.host_type_id;
            let result = {
                self.suspend_host();
                let guard = SuspendedHost { interpreter: self };
                host.execute_message(tx_env, &mut message, Some(&mut *guard.interpreter))
            };
            // Host execution creates fresh reborrows; refresh the pointer before later step hooks.
            self.host_type_id = host_type_id;
            self.host = Some(NonNull::from(&mut *host));
            self.host_bound = true;
            let result = match result {
                Ok(result) => complete_message::<T>(
                    StackMut { stack: &mut self.stack, len: &mut self.stack_len },
                    &mut self.gas,
                    &mut self.memory,
                    &mut self.return_data,
                    completion,
                    result,
                ),
                Err(error) => Err(self.fail(error)),
            };
            if self.result.is_ok() {
                self.result = result;
            }
            if result.is_ok() {
                dispatch::inc_pc(&mut pc, op);
            }
            self.pc = pc.as_ptr();
            if step_opcodes {
                let stack_len = self.stack_len;
                InterpreterState::wrap_mut(self).inspect_step_end(pc, stack_len);
            }
        };
        self.finish_run(stop)
    }

    /// Takes an owned error recorded by an instruction or inspector hook.
    pub(crate) const fn take_error(&mut self) -> Option<ExecutionError> {
        self.error.take()
    }

    /// Runs a suspended parent's callback with access to its host.
    ///
    /// Custom [`Host::execute_message`] implementations must use this scope before callbacks
    /// access [`Self::host`] or [`Self::stack_memory_host`]. Direct host access and child execution
    /// happen outside the scope; bind a fresh scope for each subsequent callback. Access is
    /// revoked when the callback returns or unwinds.
    ///
    /// # Panics
    ///
    /// Panics unless host access is suspended and `host` is the host used to prepare this frame.
    pub fn with_host<'bound, R>(
        &mut self,
        host: &mut T::Host<'bound>,
        callback: impl FnOnce(&mut Self) -> R,
    ) -> R {
        assert!(!self.host_bound, "host access is already bound");
        assert_eq!(
            self.host_type_id,
            Some(NonStaticAny::type_id(host)),
            "parent belongs to a different host type"
        );
        let host = NonNull::from(host);
        assert_eq!(
            self.host.map(NonNull::cast::<()>),
            Some(host.cast::<()>()),
            "parent belongs to a different host"
        );
        // SAFETY: The same original host object and concrete lifetime-erased type were checked.
        // T's host family differs only by stored-object lifetimes, so the pointer layout matches.
        // Copying preserves the fresh reborrow's provenance and metadata (vtable addresses need
        // not be identical for two coercions of the same trait-object host). HostBinding revokes
        // access before the reborrow ends, including when the callback unwinds.
        self.host = Some(unsafe { core::mem::transmute_copy(&host) });
        self.host_bound = true;
        let guard = HostBinding { interpreter: self };
        callback(&mut *guard.interpreter)
    }

    pub(crate) const fn suspend_host(&mut self) {
        self.host_bound = false;
    }

    const fn host_ptr(&self) -> NonNull<T::Host<'host>> {
        assert!(self.host_bound, "suspended host access requires Interpreter::with_host");
        self.host.expect("host is initialized while bound")
    }
}

/// Interpreter state exposed to instruction implementations.
#[repr(transparent)]
pub struct InterpreterState<'frame, 'host, T: EvmTypesHost>(
    pub(crate) Interpreter<'frame, 'host, T>,
);

impl<T: EvmTypesHost> fmt::Debug for InterpreterState<'_, '_, T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'frame, 'host, T: EvmTypesHost> InterpreterState<'frame, 'host, T> {
    /// Converts a host failure into an instruction stop in this frame.
    #[inline]
    pub fn fail(&mut self, error: impl Into<HostError>) -> InstrStop {
        self.0.fail(error)
    }

    #[inline]
    pub(crate) const fn wrap_mut<'a>(
        interp: &'a mut Interpreter<'frame, 'host, T>,
    ) -> &'a mut Self {
        // SAFETY: `InterpreterState` is a transparent wrapper over `Interpreter`.
        unsafe { core::mem::transmute::<&mut Interpreter<'frame, 'host, T>, &mut Self>(interp) }
    }

    #[inline]
    pub(in crate::interpreter) const unsafe fn gas_from_state_ptr(state: *mut Self) -> *mut Gas {
        // SAFETY: The caller upholds that `state` points to a valid interpreter state.
        unsafe { &raw mut (*state).0.gas }
    }

    #[inline]
    pub(crate) const fn gas_mut(&mut self) -> &mut Gas {
        &mut self.0.gas
    }

    #[inline(always)]
    pub(crate) const fn set_result(&mut self, result: Result) {
        self.0.result = result;
    }

    #[inline]
    pub(crate) const fn result(&self) -> Result {
        self.0.result
    }

    #[inline]
    pub(crate) const fn set_pc_stack_len(&mut self, pc: *const u8, stack_len: usize) {
        self.0.pc = pc;
        self.0.stack_len = stack_len;
    }

    /// Returns the cached transaction-global environment.
    #[inline]
    pub const fn tx(&self) -> &'frame TxEnv<T> {
        // SAFETY: `tx_env` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.tx_env.unwrap_unchecked() }
    }

    /// Returns the active base specification ID.
    #[inline]
    pub const fn spec(&self) -> SpecId {
        self.0.spec
    }

    /// Returns the active bytecode.
    #[inline]
    pub const fn bytecode(&self) -> BytecodeRef<'_> {
        // SAFETY: `prepare_run` initializes the view before instruction execution. It remains
        // valid across call/resume cycles and is only cleared when initializing a new frame.
        unsafe { self.0.bytecode_ref.unwrap_unchecked() }
    }

    /// Returns the host implementation.
    #[inline]
    pub const fn host(&mut self) -> &mut T::Host<'host> {
        // SAFETY: `host` is initialized at the beginning of `run` and cleared before the
        // method returns. Instruction execution is synchronous, so the pointer cannot outlive the
        // `run` host borrow.
        self.0.host()
    }

    /// Returns the active runtime version data.
    #[inline]
    pub const fn version(&self) -> &'frame Version {
        // SAFETY: `version` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.version.unwrap_unchecked() }
    }

    /// Returns the active frame-local call/create message.
    #[inline]
    pub const fn message(&self) -> &'frame Message<T> {
        // SAFETY: `message` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.message.unwrap_unchecked() }
    }

    /// Returns whether the active frame forbids state-changing operations.
    #[inline]
    pub const fn is_static(&self) -> bool {
        self.0.is_static
    }

    /// Returns `true` if the active feature set contains `feature`.
    #[inline]
    pub const fn feature(&self, feature: EvmFeatures) -> bool {
        self.0.features.contains(feature)
    }

    /// Returns the active dynamic gas parameters.
    #[inline]
    pub const fn gas_params(&self) -> &'frame GasParams {
        &self.version().gas_params
    }

    /// Returns linear memory.
    #[inline]
    pub const fn memory(&mut self) -> &mut Memory {
        &mut self.0.memory
    }

    /// Resizes linear memory using the active runtime gas parameters.
    #[inline]
    pub fn resize_memory(&mut self, gas: &mut Gas, offset: usize, len: usize) -> Result {
        self.0.memory.resize_evm(gas, self.gas_params(), offset, len)
    }

    /// Returns return data from the last call-like operation.
    #[inline]
    pub const fn return_data(&self) -> &Bytes {
        &self.0.return_data
    }

    /// Returns a mutable reference to return data from the last call-like operation.
    #[inline]
    pub const fn return_data_mut(&mut self) -> &mut Bytes {
        &mut self.0.return_data
    }

    /// Clears return data from the last call-like operation.
    #[inline]
    pub fn clear_return_data(&mut self) {
        self.0.return_data.clear();
    }

    /// Returns the current frame output memory range.
    #[inline]
    pub const fn output_range(&self) -> &Range<u32> {
        &self.0.output
    }

    /// Sets the current frame output memory range.
    #[inline]
    pub const fn set_output(&mut self, output: Range<u32>) {
        self.0.output = output;
    }

    #[inline]
    pub(crate) fn inspect_step(&mut self, pc: Pc, stack_len: usize) {
        self.0.pc = pc.as_ptr();
        self.0.stack_len = stack_len;
        unsafe {
            let mut inspector = self.0.inspector.unwrap_unchecked();
            inspector.as_mut().step(&mut self.0);
        }
    }

    #[inline]
    pub(crate) fn inspect_step_end(&mut self, pc: Pc, stack_len: usize) {
        self.0.pc = pc.as_ptr();
        self.0.stack_len = stack_len;
        if self.0.result.is_err_and(InstrStop::is_out_of_gas) {
            cold_path();
            // Failed charges may leave a wrapped counter until frame settlement.
            self.0.gas.set_remaining(0);
        }
        unsafe {
            let mut inspector = self.0.inspector.unwrap_unchecked();
            inspector.as_mut().step_end(&mut self.0);
        }
    }

    #[inline]
    pub(crate) fn inspect_selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &Word,
    ) {
        if let Some(mut inspector) = self.0.inspector {
            unsafe {
                let mut host = self.0.host_ptr();
                inspector.as_mut().selfdestruct(contract, target, value, host.as_mut());
            }
        }
    }

    #[inline]
    pub(crate) const fn has_inspector(&self) -> bool {
        self.0.inspector.is_some()
    }

    #[inline]
    pub(crate) const fn has_pending_message(&self) -> bool {
        self.0.pending_message.is_some()
    }

    pub(crate) fn discard_pending_message(&mut self) {
        self.0.pending_message = None;
    }

    pub(crate) fn suspend_message(&mut self, message: Message<T>, completion: MessageCompletion) {
        self.0.pending_message = Some(PendingMessage { message, completion });
    }

    pub(crate) fn complete_message(
        &mut self,
        stack: StackMut<'_>,
        completion: MessageCompletion,
        result: MessageResult<T>,
    ) -> Result {
        complete_message::<T>(
            stack,
            &mut self.0.gas,
            &mut self.0.memory,
            &mut self.0.return_data,
            completion,
            result,
        )
    }
}

#[derive(Default)]
pub(crate) struct InterpreterPool<T: EvmTypesHost> {
    frames: Vec<Box<Interpreter<'static, 'static, T>>>,
}

impl<T: EvmTypesHost> InterpreterPool<T> {
    pub(crate) const fn new() -> Self {
        Self { frames: Vec::new() }
    }

    pub(crate) fn pop<'frame, 'host>(
        &mut self,
        tx_env: &'frame TxEnv<T>,
        message: &'frame Message<T>,
    ) -> Box<Interpreter<'frame, 'host, T>> {
        let frame = match self.frames.pop() {
            Some(mut frame) => {
                frame.init(tx_env, message);
                frame
            }
            None => Box::new(Interpreter::new(tx_env, message)),
        };
        // SAFETY: Frames stored in the pool have their frame-local references cleared before they
        // are erased to `'static`. Rebinding the lifetime is only used to initialize the next
        // frame.
        unsafe { trustme::decouple_lt_box(frame) }
    }

    pub(crate) fn push<'pool, 'frame, 'host>(
        &'pool mut self,
        mut frame: Box<Interpreter<'frame, 'host, T>>,
    ) -> &'pool mut Interpreter<'frame, 'host, T> {
        frame.clear_frame_refs();
        // SAFETY: `clear_frame_refs` removes every reference carrying `'frame`, so the boxed
        // interpreter can be stored in the pool with the erased `'static` lifetime.
        let frame = unsafe { trustme::decouple_lt_box(frame) };
        let frame = self.frames.push_mut(frame);
        // SAFETY: The returned borrow is tied to `&mut self`; the erased frame references are
        // empty.
        unsafe { trustme::decouple_interpreter_lt_mut(frame) }
    }

    pub(crate) fn last_mut<'frame, 'host>(&mut self) -> Option<&mut Interpreter<'frame, 'host, T>> {
        let frame = self.frames.last_mut()?.as_mut();
        // SAFETY: Frames stored in the pool have had their frame-local references cleared by
        // `push`, and this borrow is tied to the pool borrow.
        Some(unsafe { trustme::decouple_interpreter_lt_mut(frame) })
    }
}

#[cfg(test)]
mod owned_error_tests {
    use super::*;
    use crate::{
        BaseEvmConfig, DatabaseError, EvmConfig, OpcodeConfig,
        env::TxEnvExt,
        interpreter::{
            GasTracker, MessageExt, MessageResultExt, OpcodeSet, instructions, op, private,
        },
        test_utils::{TestHost, TestTypes},
    };

    #[test]
    fn owned_error_is_taken_on_exit() {
        let tx = TxEnvExt::default();
        let message = MessageExt::default();
        let mut interpreter = Interpreter::<TestTypes>::new(&tx, &message);
        let error = DatabaseError::new(core::fmt::Error, false);
        let stop = interpreter.fail(error.clone());
        let result = interpreter.finish_run(stop);
        assert_eq!(result, Err(ExecutionError::Database(error)));
        assert!(interpreter.error.is_none());
        assert_eq!(interpreter.finish_run(InstrStop::Stop), Ok(InstrStop::Stop));
    }

    #[test]
    fn custom_host_binds_each_parent_callback() {
        fn inspect(host: &mut TestHost, parent: &mut Interpreter<'_, '_, TestTypes>) {
            // execute_message already mutated the host by recording its call.
            assert_eq!(host.calls.len(), 1);
            parent.with_host(host, |parent| {
                parent.host().tstore(&Address::ZERO, &Word::ZERO, &Word::from(123));
                assert_eq!(parent.host().tload(&Address::ZERO, &Word::ZERO), Word::from(123));
                parent.stack_mut().push(Word::from(99)).unwrap();
            });
            assert!(!parent.host_bound);
            host.tstore(&Address::ZERO, &Word::ZERO, &Word::from(456));
            parent.with_host(host, |parent| {
                assert_eq!(parent.host().tload(&Address::ZERO, &Word::ZERO), Word::from(456));
            });
            assert!(!parent.host_bound);
        }

        struct Hooks(OpcodeSet);
        impl Inspector<TestTypes> for Hooks {
            fn inspected_opcodes(&self) -> OpcodeSet {
                self.0
            }
        }
        for opcodes in [OpcodeSet::EMPTY, OpcodeSet::ALL] {
            let tx = TxEnvExt::default();
            let message = parent_call_message();
            let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
            let config = ExecutionConfig::for_config::<BaseEvmConfig<{ SpecId::OSAKA as u32 }>>();
            let mut host = TestHost {
                message_hook: Some(inspect),
                execute_result: MessageResultExt {
                    gas: GasTracker::new(1000),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                parent.run_inspect(&config, &mut host, &mut Hooks(opcodes)),
                Ok(InstrStop::Stop)
            );
            assert_eq!(parent.stack().as_slice(), &[Word::from(99), Word::from(1)]);
            assert_eq!(parent.gas().remaining(), 99_879);
        }
    }

    #[test]
    #[should_panic(expected = "suspended host access requires Interpreter::with_host")]
    fn custom_host_cannot_access_stale_parent_host() {
        fn inspect(host: &mut TestHost, parent: &mut Interpreter<'_, '_, TestTypes>) {
            host.tstore(&Address::ZERO, &Word::ZERO, &Word::from(456));
            parent.host().tload(&Address::ZERO, &Word::ZERO);
        }
        let tx = TxEnvExt::default();
        let message = parent_call_message();
        let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
        let config = ExecutionConfig::for_config::<BaseEvmConfig<{ SpecId::OSAKA as u32 }>>();
        let mut host = TestHost { message_hook: Some(inspect), ..Default::default() };
        parent.run_inspect(&config, &mut host, &mut crate::NoopInspector::default()).unwrap();
    }

    #[test]
    #[cfg(feature = "std")]
    fn host_binding_is_revoked_on_unwind() {
        let tx = TxEnvExt::default();
        let message = MessageExt::default();
        let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
        let mut host = TestHost::default();
        parent.prepare_run(SpecId::OSAKA, Version::base(SpecId::OSAKA), &mut host);
        parent.suspend_host();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parent.with_host(&mut host, |parent| {
                parent.host().tstore(&Address::ZERO, &Word::ZERO, &Word::from(123));
                panic!("callback failed");
            });
        }));
        assert!(result.is_err());
        assert!(!parent.host_bound);
        host.tstore(&Address::ZERO, &Word::ZERO, &Word::from(456));
        parent.with_host(&mut host, |parent| {
            assert_eq!(parent.host().tload(&Address::ZERO, &Word::ZERO), Word::from(456));
        });
        assert!(!parent.host_bound);
    }

    #[test]
    #[cfg(feature = "std")]
    fn message_unwind_clears_parent_host_identity() {
        fn inspect(_host: &mut TestHost, _parent: &mut Interpreter<'_, '_, TestTypes>) {
            panic!("message failed");
        }
        let tx = TxEnvExt::default();
        let message = parent_call_message();
        let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
        let config = ExecutionConfig::for_config::<BaseEvmConfig<{ SpecId::OSAKA as u32 }>>();
        let mut host = TestHost { message_hook: Some(inspect), ..Default::default() };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parent.run_inspect(&config, &mut host, &mut crate::NoopInspector::default()).unwrap();
        }));
        assert!(result.is_err());
        assert!(!parent.host_bound);
        assert!(parent.host.is_none());
        assert!(parent.host_type_id.is_none());
    }

    fn parent_call_message() -> Message<TestTypes> {
        let mut code = [op::PUSH1, 0].repeat(5);
        code.extend([op::PUSH1, 0x22, op::PUSH2, 3, 0xe8, op::CALL, op::STOP]);
        MessageExt {
            gas_limit: 100_000,
            code: Bytecode::new_legacy(code.into()),
            ..Default::default()
        }
    }

    #[test]
    fn call_wrapper_error_cancels_suspension_and_reaches_step_end() {
        struct RejectCall;

        impl private::Instruction<TestTypes> for RejectCall {
            fn execute(
                pc: &mut Pc,
                stack: StackMut<'_>,
                state: &mut InterpreterState<'_, '_, TestTypes>,
            ) -> Result {
                <instructions::call<TestTypes> as private::Instruction<TestTypes>>::execute(
                    pc, stack, state,
                )?;
                Err(InstrStop::OutOfGas)
            }
        }

        struct Config;

        impl EvmConfig<TestTypes> for Config {
            const BASE_SPEC_ID: SpecId = SpecId::OSAKA;
            const OPCODE_CONFIG: &'static OpcodeConfig<TestTypes> = &{
                let mut config = OpcodeConfig::base::<Self>();
                config.set_instruction::<RejectCall>(op::CALL, 100);
                config
            };
        }

        struct Rejections(usize, OpcodeSet);

        impl Inspector<TestTypes> for Rejections {
            fn step_end(&mut self, interp: &mut Interpreter<'_, '_, TestTypes>) {
                if interp.opcode() == op::CALL && interp.result().is_err() {
                    assert_eq!(interp.result(), Err(InstrStop::OutOfGas));
                    self.0 += 1;
                }
            }

            fn inspected_opcodes(&self) -> OpcodeSet {
                self.1
            }
        }

        for opcodes in [None, Some(OpcodeSet::EMPTY), Some(OpcodeSet::ALL)] {
            let tx = TxEnvExt::default();
            let message = parent_call_message();
            let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
            let config = ExecutionConfig::for_config::<Config>();
            let mut host = TestHost::default();
            let mut inspector = Rejections(0, opcodes.unwrap_or(OpcodeSet::ALL));
            let result = if opcodes.is_some() {
                parent.run_inspect(&config, &mut host, &mut inspector)
            } else {
                parent.run(&config, &mut host)
            };
            assert_eq!(result, Ok(InstrStop::OutOfGas));
            assert!(parent.pending_message.is_none());
            assert_eq!(host.calls.len(), usize::from(opcodes.is_none()));
            assert_eq!(inspector.0, usize::from(opcodes == Some(OpcodeSet::ALL)));
        }
    }

    #[test]
    fn call_wrapper_observes_precompletion_state_when_inspected() {
        struct ObserveCall;

        impl private::Instruction<TestTypes> for ObserveCall {
            fn execute(
                pc: &mut Pc,
                stack: StackMut<'_>,
                state: &mut InterpreterState<'_, '_, TestTypes>,
            ) -> Result {
                <instructions::call<TestTypes> as private::Instruction<TestTypes>>::execute(
                    pc, stack, state,
                )?;
                let len = Word::from(state.return_data().len());
                state.host().tstore(&Address::ZERO, &Word::ZERO, &len);
                Ok(())
            }
        }

        struct Config;

        impl EvmConfig<TestTypes> for Config {
            const BASE_SPEC_ID: SpecId = SpecId::OSAKA;
            const OPCODE_CONFIG: &'static OpcodeConfig<TestTypes> = &{
                let mut config = OpcodeConfig::base::<Self>();
                config.set_instruction::<ObserveCall>(op::CALL, 100);
                config
            };
        }

        struct Completion(usize, OpcodeSet);

        impl Inspector<TestTypes> for Completion {
            fn step_end(&mut self, interp: &mut Interpreter<'_, '_, TestTypes>) {
                if interp.opcode() == op::STOP && interp.result().is_ok() {
                    self.0 = interp.return_data().len();
                    assert_eq!(interp.stack().as_slice(), &[Word::from(1)]);
                }
            }

            fn inspected_opcodes(&self) -> OpcodeSet {
                self.1
            }
        }

        for opcodes in [None, Some(OpcodeSet::EMPTY), Some(OpcodeSet::ALL)] {
            let tx = TxEnvExt::default();
            let message = parent_call_message();
            let mut parent = Interpreter::<TestTypes>::new(&tx, &message);
            let config = ExecutionConfig::for_config::<Config>();
            let mut host = TestHost {
                execute_result: MessageResultExt {
                    gas: GasTracker::new(1000),
                    output: Bytes::from_static(b"child"),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut inspector = Completion(0, opcodes.unwrap_or(OpcodeSet::ALL));
            let result = if opcodes.is_some() {
                parent.run_inspect(&config, &mut host, &mut inspector)
            } else {
                parent.run(&config, &mut host)
            };
            assert_eq!(result, Ok(InstrStop::Stop));
            assert_eq!(parent.return_data(), &Bytes::from_static(b"child"));
            assert_eq!(
                host.tload(&Address::ZERO, &Word::ZERO),
                Word::from(if opcodes.is_some() { 0 } else { 5 })
            );
            assert_eq!(inspector.0, if opcodes == Some(OpcodeSet::ALL) { 5 } else { 0 });
        }
    }
}

#[derive_where(Debug)]
struct PendingMessage<T: EvmTypesHost> {
    message: Message<T>,
    completion: MessageCompletion,
}

#[derive(Debug)]
pub(crate) enum MessageCompletion {
    Call { output: Range<usize>, new_account_state_gas: u64 },
    Create { new_account_state_gas: u64 },
}

fn complete_message<T: EvmTypesHost>(
    mut stack: StackMut<'_>,
    gas: &mut Gas,
    memory: &mut Memory,
    return_data: &mut Bytes,
    completion: MessageCompletion,
    mut result: MessageResult<T>,
) -> Result {
    if result.stop.is_fatal() {
        return Err(result.stop);
    }
    gas.merge_child_gas(result.gas, result.stop);
    match completion {
        MessageCompletion::Call { output, new_account_state_gas } => {
            // Refund the upfront account-creation charge if the value-bearing CALL failed.
            if new_account_state_gas != 0 && !result.stop.is_success() {
                gas.refill_reservoir(new_account_state_gas);
            }
            let copy_len = min(output.len(), result.output.len());
            if copy_len != 0 {
                // Hooks can change memory, so reacquire and resize it before copying the output.
                memory.resize(output.start, copy_len)?;
                unsafe {
                    memory.set_unchecked(output.start, result.output.get_unchecked(..copy_len));
                }
            }
            core::mem::swap(return_data, &mut result.output);
            stack.push(Word::from(result.stop.is_success()))
        }
        MessageCompletion::Create { new_account_state_gas } => {
            // CREATE failures leave no new account and refund its conditional state charge.
            if new_account_state_gas != 0
                && (result.created_address.is_none() || !result.stop.is_success())
            {
                gas.refill_reservoir(new_account_state_gas);
            }
            // EIP-211 exposes only CREATE revert data.
            if result.stop == InstrStop::Revert {
                core::mem::swap(return_data, &mut result.output);
            } else {
                return_data.clear();
            }
            stack.push(result.created_address_for_parent())
        }
    }
}

struct HostBinding<'a, 'frame, 'host, T: EvmTypesHost> {
    interpreter: &'a mut Interpreter<'frame, 'host, T>,
}

impl<T: EvmTypesHost> Drop for HostBinding<'_, '_, '_, T> {
    fn drop(&mut self) {
        self.interpreter.suspend_host();
    }
}

struct SuspendedHost<'a, 'frame, 'host, T: EvmTypesHost> {
    interpreter: &'a mut Interpreter<'frame, 'host, T>,
}

impl<T: EvmTypesHost> Drop for SuspendedHost<'_, '_, '_, T> {
    fn drop(&mut self) {
        self.interpreter.host = None;
        self.interpreter.host_type_id = None;
        self.interpreter.suspend_host();
    }
}
