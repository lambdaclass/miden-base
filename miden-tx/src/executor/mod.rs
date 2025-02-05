use miden_lib::transaction::{ToTransactionKernelInputs, TransactionKernel};
use miden_objects::{
    accounts::{Account, AccountDelta, AccountStorage, AccountStorageDelta, AccountStub},
    assembly::ProgramAst,
    crypto::merkle::{merkle_tree_delta, MerkleStore},
    transaction::{TransactionInputs, TransactionScript},
    vm::{Program, StackOutputs},
    Felt, TransactionOutputError, Word,
};
use vm_processor::ExecutionOptions;

use super::{
    AccountCode, AccountId, Digest, ExecutedTransaction, NoteId, NoteScript, PreparedTransaction,
    RecAdviceProvider, ScriptTarget, TransactionCompiler, TransactionExecutorError,
    TransactionHost,
};

mod data;
pub use data::DataStore;

// TRANSACTION EXECUTOR
// ================================================================================================

/// The transaction executor is responsible for executing Miden rollup transactions.
///
/// Transaction execution consists of the following steps:
/// - Fetch the data required to execute a transaction from the [DataStore].
/// - Compile the transaction into a program using the [TransactionCompiler](crate::TransactionCompiler).
/// - Execute the transaction program and create an [ExecutedTransaction].
///
/// The transaction executor is generic over the [DataStore] which allows it to be used with
/// different data backend implementations.
///
/// The [TransactionExecutor::execute_transaction()] method is the main entry point for the
/// executor and produces an [ExecutedTransaction] for the transaction. The executed transaction
/// can then be used to by the prover to generate a proof transaction execution.
pub struct TransactionExecutor<D: DataStore> {
    data_store: D,
    compiler: TransactionCompiler,
    exec_options: ExecutionOptions,
}

impl<D: DataStore> TransactionExecutor<D> {
    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------
    /// Creates a new [TransactionExecutor] instance with the specified [DataStore].
    pub fn new(data_store: D) -> Self {
        Self {
            data_store,
            compiler: TransactionCompiler::new(),
            exec_options: ExecutionOptions::default(),
        }
    }

    // STATE MUTATORS
    // --------------------------------------------------------------------------------------------

    /// Fetches the account code from the [DataStore], compiles it, and loads the compiled code
    /// into the internal cache.
    ///
    /// This also returns the [AccountCode] object built from the loaded account code.
    ///
    /// # Errors:
    /// Returns an error if:
    /// - If the account code cannot be fetched from the [DataStore].
    /// - If the account code fails to be loaded into the compiler.
    pub fn load_account(
        &mut self,
        account_id: AccountId,
    ) -> Result<AccountCode, TransactionExecutorError> {
        let account_code = self
            .data_store
            .get_account_code(account_id)
            .map_err(TransactionExecutorError::FetchAccountCodeFailed)?;
        self.compiler
            .load_account(account_id, account_code)
            .map_err(TransactionExecutorError::LoadAccountFailed)
    }

    /// Loads the provided account interface (vector of procedure digests) into the compiler.
    ///
    /// Returns the old interface for the specified account ID if it previously existed.
    pub fn load_account_interface(
        &mut self,
        account_id: AccountId,
        procedures: Vec<Digest>,
    ) -> Option<Vec<Digest>> {
        self.compiler.load_account_interface(account_id, procedures)
    }

    /// Compiles the provided program into a [NoteScript] and checks (to the extent possible) if
    /// the specified note program could be executed against all accounts with the specified
    /// interfaces.
    pub fn compile_note_script(
        &mut self,
        note_script_ast: ProgramAst,
        target_account_procs: Vec<ScriptTarget>,
    ) -> Result<NoteScript, TransactionExecutorError> {
        self.compiler
            .compile_note_script(note_script_ast, target_account_procs)
            .map_err(TransactionExecutorError::CompileNoteScriptFailed)
    }

    /// Compiles the provided transaction script source and inputs into a [TransactionScript] and
    /// checks (to the extent possible) that the transaction script can be executed against all
    /// accounts with the specified interfaces.
    pub fn compile_tx_script<T>(
        &mut self,
        tx_script_ast: ProgramAst,
        inputs: T,
        target_account_procs: Vec<ScriptTarget>,
    ) -> Result<TransactionScript, TransactionExecutorError>
    where
        T: IntoIterator<Item = (Word, Vec<Felt>)>,
    {
        self.compiler
            .compile_tx_script(tx_script_ast, inputs, target_account_procs)
            .map_err(TransactionExecutorError::CompileTransactionScriptFailed)
    }

    // TRANSACTION EXECUTION
    // --------------------------------------------------------------------------------------------

    /// Prepares and executes a transaction specified by the provided arguments and returns an
    /// [ExecutedTransaction].
    ///
    /// The method first fetches the data required to execute the transaction from the [DataStore]
    /// and compile the transaction into an executable program. Then, it executes the transaction
    /// program and creates an [ExecutedTransaction] object.
    ///
    /// # Errors:
    /// Returns an error if:
    /// - If required data can not be fetched from the [DataStore].
    /// - If the transaction program can not be compiled.
    /// - If the transaction program can not be executed.
    pub fn execute_transaction(
        &mut self,
        account_id: AccountId,
        block_ref: u32,
        notes: &[NoteId],
        tx_script: Option<TransactionScript>,
    ) -> Result<ExecutedTransaction, TransactionExecutorError> {
        let transaction = self.prepare_transaction(account_id, block_ref, notes, tx_script)?;

        let (stack_inputs, advice_inputs) = transaction.get_kernel_inputs();
        let advice_recorder: RecAdviceProvider = advice_inputs.into();
        let mut host = TransactionHost::new(transaction.account().into(), advice_recorder);

        let result = vm_processor::execute(
            transaction.program(),
            stack_inputs,
            &mut host,
            self.exec_options,
        )
        .map_err(TransactionExecutorError::ExecuteTransactionProgramFailed)?;

        let (tx_program, tx_script, tx_inputs) = transaction.into_parts();

        build_executed_transaction(
            tx_program,
            tx_script,
            tx_inputs,
            result.stack_outputs().clone(),
            host,
        )
    }

    // HELPER METHODS
    // --------------------------------------------------------------------------------------------

    /// Fetches the data required to execute the transaction from the [DataStore], compiles the
    /// transaction into an executable program using the [TransactionCompiler], and returns a
    /// [PreparedTransaction].
    ///
    /// # Errors:
    /// Returns an error if:
    /// - If required data can not be fetched from the [DataStore].
    /// - If the transaction can not be compiled.
    fn prepare_transaction(
        &mut self,
        account_id: AccountId,
        block_ref: u32,
        notes: &[NoteId],
        tx_script: Option<TransactionScript>,
    ) -> Result<PreparedTransaction, TransactionExecutorError> {
        let tx_inputs = self
            .data_store
            .get_transaction_inputs(account_id, block_ref, notes)
            .map_err(TransactionExecutorError::FetchTransactionInputsFailed)?;

        let tx_program = self
            .compiler
            .compile_transaction(
                account_id,
                tx_inputs.input_notes(),
                tx_script.as_ref().map(|x| x.code()),
            )
            .map_err(TransactionExecutorError::CompileTransactionFiled)?;

        Ok(PreparedTransaction::new(tx_program, tx_script, tx_inputs))
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Creates a new [ExecutedTransaction] from the provided data, advice provider and stack outputs.
fn build_executed_transaction(
    program: Program,
    tx_script: Option<TransactionScript>,
    tx_inputs: TransactionInputs,
    stack_outputs: StackOutputs,
    host: TransactionHost<RecAdviceProvider>,
) -> Result<ExecutedTransaction, TransactionExecutorError> {
    let (advice_recorder, vault_delta) = host.into_parts();

    // finalize the advice recorder
    let (advice_witness, _, map, store) = advice_recorder.finalize();

    // parse transaction results
    let tx_outputs = TransactionKernel::parse_transaction_outputs(&stack_outputs, &map.into())
        .map_err(TransactionExecutorError::InvalidTransactionOutput)?;
    let final_account = &tx_outputs.account;

    let initial_account = tx_inputs.account();

    if initial_account.id() != final_account.id() {
        return Err(TransactionExecutorError::InconsistentAccountId {
            input_id: initial_account.id(),
            output_id: final_account.id(),
        });
    }

    // build account delta

    // TODO: Fix delta extraction for new account creation
    // extract the account storage delta
    let storage_delta = extract_account_storage_delta(&store, initial_account, final_account)
        .map_err(TransactionExecutorError::InvalidTransactionOutput)?;

    // extract the nonce delta
    let nonce_delta = if initial_account.nonce() != final_account.nonce() {
        Some(final_account.nonce())
    } else {
        None
    };

    // construct the account delta
    let account_delta =
        AccountDelta::new(storage_delta, vault_delta, nonce_delta).expect("invalid account delta");

    Ok(ExecutedTransaction::new(
        program,
        tx_inputs,
        tx_outputs,
        account_delta,
        tx_script,
        advice_witness,
    ))
}

/// Extracts account storage delta between the `initial_account` and `final_account_stub` from the
/// provided `MerkleStore`
fn extract_account_storage_delta(
    store: &MerkleStore,
    initial_account: &Account,
    final_account_stub: &AccountStub,
) -> Result<AccountStorageDelta, TransactionOutputError> {
    // extract storage slots delta
    let tree_delta = merkle_tree_delta(
        initial_account.storage().root(),
        final_account_stub.storage_root(),
        AccountStorage::STORAGE_TREE_DEPTH,
        store,
    )
    .map_err(TransactionOutputError::ExtractAccountStorageSlotsDeltaFailed)?;

    // map tree delta to cleared/updated slots; we can cast indexes to u8 because the
    // the number of storage slots cannot be greater than 256
    let cleared_items = tree_delta.cleared_slots().iter().map(|idx| *idx as u8).collect();
    let updated_items = tree_delta
        .updated_slots()
        .iter()
        .map(|(idx, value)| (*idx as u8, *value))
        .collect();

    // construct storage delta
    let storage_delta = AccountStorageDelta { cleared_items, updated_items };

    Ok(storage_delta)
}
