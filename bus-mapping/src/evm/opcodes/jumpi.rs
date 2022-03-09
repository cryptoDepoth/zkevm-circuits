use super::Opcode;
use crate::circuit_input_builder::{CircuitInputStateRef, ExecStep};
use crate::{operation::RW, Error};
use eth_types::GethExecStep;

/// Placeholder structure used to implement [`Opcode`] trait over it
/// corresponding to the [`OpcodeId::JUMPI`](crate::evm::OpcodeId::JUMPI)
/// `OpcodeId`.
#[derive(Debug, Copy, Clone)]
pub(crate) struct Jumpi;

impl Opcode for Jumpi {
    fn gen_associated_ops(
        state: &mut CircuitInputStateRef,
        exec_step: &mut ExecStep,
        steps: &[GethExecStep],
    ) -> Result<(), Error> {
        let step = &steps[0];
        // `JUMPI` needs two read operation
        state.push_stack_op(
            exec_step,
            RW::READ,
            step.stack.nth_last_filled(0),
            step.stack.nth_last(0)?,
        );
        state.push_stack_op(
            exec_step,
            RW::READ,
            step.stack.nth_last_filled(1),
            step.stack.nth_last(1)?,
        );

        Ok(())
    }
}

#[cfg(test)]
mod jumpi_tests {
    use super::*;
    use crate::circuit_input_builder::{ExecStep, TransactionContext};
    use eth_types::evm_types::StackAddress;
    use eth_types::{bytecode, Word};
    use pretty_assertions::assert_eq;

    #[test]
    fn jumpi_opcode_impl() -> Result<(), Error> {
        let condition = 1;
        let destination = 69;

        let mut code = bytecode! {
            PUSH32(condition)
            PUSH32(destination)
            #[start]
            JUMPI
            STOP
        };
        for _ in 0..(destination - 68) {
            code.write(0);
        }
        code.append(&bytecode! {
            JUMPDEST
            STOP
        });

        // Get the execution steps from the external tracer
        let block = crate::mock::BlockData::new_from_geth_data(
            mock::new_single_tx_trace_code_at_start(&code).unwrap(),
        );

        let mut builder = block.new_circuit_input_builder();
        builder
            .handle_tx(&block.eth_tx, &block.geth_trace, Default::default())
            .unwrap();

        let mut test_builder = block.new_circuit_input_builder();
        let mut tx = test_builder
            .new_tx(&block.eth_tx, Default::default(), !block.geth_trace.failed)
            .unwrap();
        let mut tx_ctx = TransactionContext::new(&block.eth_tx, &block.geth_trace).unwrap();

        // Generate step corresponding to JUMP
        let mut step = ExecStep::new(
            &block.geth_trace.struct_logs[0],
            0,
            test_builder.block_ctx.rwc,
            0,
        );
        let mut state_ref = test_builder.state_ref(&mut tx, &mut tx_ctx);

        // Add the last 2 Stack reads
        state_ref.push_stack_op(
            &mut step,
            RW::READ,
            StackAddress::from(1022),
            Word::from(destination),
        );
        state_ref.push_stack_op(
            &mut step,
            RW::READ,
            StackAddress::from(1023),
            Word::from(condition),
        );

        tx.steps_mut().push(step);
        test_builder.block.txs_mut().push(tx);

        // Compare first step bus mapping instance
        assert_eq!(
            builder.block.txs()[0].steps()[0].bus_mapping_instance,
            test_builder.block.txs()[0].steps()[0].bus_mapping_instance,
        );

        // Compare containers
        assert_eq!(builder.block.container, test_builder.block.container);

        Ok(())
    }
}
