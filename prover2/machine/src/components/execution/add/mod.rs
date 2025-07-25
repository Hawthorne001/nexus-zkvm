use std::marker::PhantomData;

use num_traits::One;
use stwo_prover::{
    constraint_framework::EvalAtRow,
    core::{
        backend::simd::{m31::LOG_N_LANES, SimdBackend},
        fields::{m31::BaseField, qm31::SecureField, FieldExpOps},
        poly::{circle::CircleEvaluation, BitReversedOrder},
        ColumnVec,
    },
};

use nexus_vm::{riscv::BuiltinOpcode, WORD_SIZE};
use nexus_vm_prover_air_column::AirColumn;
use nexus_vm_prover_trace::{
    builder::{FinalizedTrace, TraceBuilder},
    component::ComponentTrace,
    eval::TraceEval,
    program::{ProgramStep, Word},
    trace_eval,
    utils::zero_array,
};

use crate::{
    components::{
        execution::{common::ExecutionComponent, decoding::InstructionDecoding},
        utils::{
            add_16bit_with_carry, add_with_carries,
            constraints::{ClkIncrement, PcIncrement},
            u32_to_16bit_parts_le,
        },
    },
    framework::BuiltInComponent,
    lookups::{
        AllLookupElements, ComponentLookupElements, InstToProgMemoryLookupElements,
        InstToRegisterMemoryLookupElements, LogupTraceBuilder, ProgramExecutionLookupElements,
    },
    side_note::{program::ProgramTraceRef, SideNote},
};

mod add;
mod addi;
mod columns;

use columns::{Column, PreprocessedColumn};

pub const ADD: Add<add::Add> = Add::new();
pub const ADDI: Add<addi::Addi> = Add::new();

pub trait AddOp:
    InstructionDecoding<PreprocessedColumn = PreprocessedColumn, MainColumn = Column>
{
}

pub struct Add<A> {
    _phantom: PhantomData<A>,
}

impl<A: AddOp> ExecutionComponent for Add<A> {
    const OPCODE: BuiltinOpcode = <A as InstructionDecoding>::OPCODE;

    const REG1_ACCESSED: bool = true;
    const REG2_ACCESSED: bool = <A as InstructionDecoding>::REG2_ACCESSED;
    const REG3_ACCESSED: bool = true;
    const REG3_WRITE: bool = true;

    type Column = Column;
}

struct ExecutionResult {
    carry_bits: [bool; 2], // carry bits for 16-bit boundaries
    sum_bytes: Word,
}

impl<A: AddOp> Add<A> {
    const fn new() -> Self {
        assert!(matches!(
            A::OPCODE,
            BuiltinOpcode::ADD | BuiltinOpcode::ADDI
        ));
        Self {
            _phantom: PhantomData,
        }
    }

    fn execute_step(value_b: Word, value_c: Word) -> ExecutionResult {
        // Recompute 32-bit result from 8-bit limbs.
        let (sum_bytes, carry_bits) = add_with_carries(value_b, value_c);
        let carry_bits = [carry_bits[1], carry_bits[3]];

        ExecutionResult {
            carry_bits,
            sum_bytes,
        }
    }

    fn generate_trace_row(
        &self,
        trace: &mut TraceBuilder<Column>,
        row_idx: usize,
        program_step: ProgramStep,
    ) {
        let step = &program_step.step;

        let pc = step.pc;
        let pc_parts = u32_to_16bit_parts_le(pc);
        let (pc_next, pc_carry) = add_16bit_with_carry(pc_parts, WORD_SIZE as u16);

        let clk = step.timestamp;
        let clk_parts = u32_to_16bit_parts_le(clk);
        let (clk_next, clk_carry) = add_16bit_with_carry(clk_parts, 1u16);

        let value_b = program_step.get_value_b();
        let (value_c, _) = program_step.get_value_c();
        let ExecutionResult {
            carry_bits,
            sum_bytes,
        } = Self::execute_step(value_b, value_c);

        trace.fill_columns(row_idx, pc_parts, Column::Pc);
        trace.fill_columns(row_idx, pc_next, Column::PcNext);
        trace.fill_columns(row_idx, pc_carry, Column::PcCarry);

        trace.fill_columns(row_idx, clk_parts, Column::Clk);
        trace.fill_columns(row_idx, clk_next, Column::ClkNext);
        trace.fill_columns(row_idx, clk_carry, Column::ClkCarry);

        trace.fill_columns_bytes(row_idx, &value_b, Column::BVal);
        trace.fill_columns_bytes(row_idx, &value_c, Column::CVal);
        trace.fill_columns_bytes(row_idx, &sum_bytes, Column::AVal);
        trace.fill_columns(row_idx, carry_bits, Column::HCarry);
    }
}

impl<A: AddOp> BuiltInComponent for Add<A> {
    type PreprocessedColumn = PreprocessedColumn;

    type MainColumn = Column;

    type LookupElements = (
        InstToProgMemoryLookupElements,
        ProgramExecutionLookupElements,
        InstToRegisterMemoryLookupElements,
    );

    fn generate_preprocessed_trace(
        &self,
        _log_size: u32,
        _program: &ProgramTraceRef,
    ) -> FinalizedTrace {
        FinalizedTrace::empty()
    }

    fn generate_main_trace(&self, side_note: &mut SideNote) -> FinalizedTrace {
        let num_add_steps = <Self as ExecutionComponent>::iter_program_steps(side_note).count();
        let log_size = num_add_steps.next_power_of_two().ilog2().max(LOG_N_LANES);

        let mut common_trace = TraceBuilder::new(log_size);
        let mut local_trace = TraceBuilder::new(log_size);

        for (row_idx, program_step) in
            <Self as ExecutionComponent>::iter_program_steps(side_note).enumerate()
        {
            self.generate_trace_row(&mut common_trace, row_idx, program_step);
            A::generate_trace_row(row_idx, &mut local_trace, program_step);
        }
        // fill padding
        for row_idx in num_add_steps..1 << log_size {
            common_trace.fill_columns(row_idx, true, Column::IsLocalPad);
        }

        common_trace.finalize().concat(local_trace.finalize())
    }

    fn generate_interaction_trace(
        &self,
        component_trace: ComponentTrace,
        side_note: &SideNote,
        lookup_elements: &AllLookupElements,
    ) -> (
        ColumnVec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>>,
        SecureField,
    ) {
        assert_eq!(
            component_trace.original_trace.len(),
            Column::COLUMNS_NUM + A::DecodingColumn::COLUMNS_NUM
        );
        let lookup_elements = Self::LookupElements::get(lookup_elements);
        let mut logup_trace_builder = LogupTraceBuilder::new(component_trace.log_size());

        <Self as ExecutionComponent>::generate_interaction_trace(
            &mut logup_trace_builder,
            &component_trace,
            side_note,
            &lookup_elements,
        );

        logup_trace_builder.finalize()
    }

    fn add_constraints<E: EvalAtRow>(
        &self,
        eval: &mut E,
        trace_eval: TraceEval<Self::PreprocessedColumn, Self::MainColumn, E>,
        lookup_elements: &Self::LookupElements,
    ) {
        let [is_local_pad] = trace_eval!(trace_eval, Column::IsLocalPad);
        let [h_carry_1, h_carry_2] = trace_eval!(trace_eval, Column::HCarry);

        let a_val = trace_eval!(trace_eval, Column::AVal);
        let b_val = trace_eval!(trace_eval, Column::BVal);
        let c_val = trace_eval!(trace_eval, Column::CVal);

        ClkIncrement {
            is_local_pad: Column::IsLocalPad,
            clk: Column::Clk,
            clk_next: Column::ClkNext,
            clk_carry: Column::ClkCarry,
        }
        .constrain(eval, &trace_eval);
        PcIncrement {
            is_local_pad: Column::IsLocalPad,
            pc: Column::Pc,
            pc_next: Column::PcNext,
            pc_carry: Column::PcCarry,
        }
        .constrain(eval, &trace_eval);

        let modulus = E::F::from(256u32.into());

        // add two bytes at a time
        //
        // (1 − is-local-pad) · (a-val(1) + h-carry(1) · 2^8 − b-val(1) − c-val(1) ) = 0
        // (1 − is-local-pad) · (a-val(2) + h-carry(2) · 2^8 − b-val(2) − c-val(2) − h-carry(1)) = 0
        eval.add_constraint(
            (E::F::one() - is_local_pad.clone())
                * (a_val[0].clone()
                    + a_val[1].clone() * modulus.clone()
                    + h_carry_1.clone() * modulus.clone().pow(2)
                    - (b_val[0].clone()
                        + b_val[1].clone() * modulus.clone()
                        + c_val[0].clone()
                        + c_val[1].clone() * modulus.clone())),
        );
        // (1 − is-local-pad) · (a-val(3) + h-carry(3) · 2^8 − b-val(3) − c-val(3) − h-carry(2)) = 0
        // (1 − is-local-pad) · (a-val(4) + h-carry(4) · 2^8 − b-val(4) − c-val(4) − h-carry(3)) = 0
        eval.add_constraint(
            (E::F::one() - is_local_pad.clone())
                * (a_val[2].clone()
                    + a_val[3].clone() * modulus.clone()
                    + h_carry_2.clone() * modulus.clone().pow(2)
                    - (b_val[2].clone()
                        + b_val[3].clone() * modulus.clone()
                        + c_val[2].clone()
                        + c_val[3].clone() * modulus.clone()
                        + h_carry_1.clone())),
        );

        let local_trace_eval = TraceEval::new(eval);
        A::constrain_decoding(eval, &trace_eval, &local_trace_eval);

        // Logup Interactions
        let (rel_inst_to_prog_memory, rel_cont_prog_exec, rel_inst_to_reg_memory) = lookup_elements;

        let instr_val = A::combine_instr_val(&local_trace_eval);
        let reg_addrs = A::combine_reg_addresses(&local_trace_eval);

        let c_val = if Self::REG2_ACCESSED {
            c_val
        } else {
            zero_array::<WORD_SIZE, E>()
        };

        <Self as ExecutionComponent>::constrain_logups(
            eval,
            &trace_eval,
            (
                rel_inst_to_prog_memory,
                rel_cont_prog_exec,
                rel_inst_to_reg_memory,
            ),
            reg_addrs,
            [a_val, b_val, c_val],
            instr_val,
        );

        eval.finalize_logup_in_pairs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        components::{
            Cpu, CpuBoundary, ProgramMemory, ProgramMemoryBoundary, RegisterMemory,
            RegisterMemoryBoundary,
        },
        framework::test_utils::{assert_component, components_claimed_sum, AssertContext},
    };
    use nexus_vm::{
        riscv::{BasicBlock, BuiltinOpcode, Instruction, Opcode},
        trace::k_trace_direct,
    };
    use num_traits::Zero;

    #[test]
    fn assert_add_constraints() {
        let basic_block = vec![BasicBlock::new(vec![
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 1, 0, 127),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 2, 1, 0),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 3, 2, 1),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 4, 3, 2),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 5, 4, 3),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADD), 6, 5, 4),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 2, 1, 1230),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 3, 2, 1231),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 4, 3, 1232),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 5, 4, 1233),
            Instruction::new_ir(Opcode::from(BuiltinOpcode::ADDI), 6, 5, 1234),
        ])];
        let (view, program_trace) =
            k_trace_direct(&basic_block, 1).expect("error generating trace");

        let assert_ctx = &mut AssertContext::new(&program_trace, &view);
        let mut claimed_sum = SecureField::zero();

        claimed_sum += assert_component(ADD, assert_ctx);
        claimed_sum += assert_component(ADDI, assert_ctx);

        claimed_sum += components_claimed_sum(
            &[
                &Cpu,
                &CpuBoundary,
                &RegisterMemory,
                &RegisterMemoryBoundary,
                &ProgramMemory,
                &ProgramMemoryBoundary,
            ],
            assert_ctx,
        );

        assert!(claimed_sum.is_zero());
    }
}
