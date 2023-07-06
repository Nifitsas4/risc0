// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The execution phase is implemented by this module.
//!
//! The result of the execution phase is a [Session]. Each [Session] contains
//! one or more [Segment]s, each of which contains an execution trace of the
//! specified program.

mod env;
pub(crate) mod io;
mod monitor;
#[cfg(feature = "profiler")]
pub(crate) mod profiler;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, fmt::Debug, io::Write, mem::take, rc::Rc};

use anyhow::{anyhow, bail, Context, Result};
use crypto_bigint::{CheckedMul, Encoding, NonZero, U256, U512};
use risc0_zkp::{
    core::{
        digest::{DIGEST_BYTES, DIGEST_WORDS},
        hash::sha::{BLOCK_BYTES, BLOCK_WORDS},
        log2_ceil,
    },
    ZK_CYCLES,
};
use risc0_zkvm_platform::{
    fileno,
    memory::MEM_SIZE,
    syscall::{
        bigint, ecall, halt,
        reg_abi::{REG_A0, REG_A1, REG_A2, REG_A3, REG_A4, REG_T0},
    },
    PAGE_SIZE, WORD_SIZE,
};
use rrs_lib::{instruction_executor::InstructionExecutor, HartState};
use serde::{Deserialize, Serialize};

pub use self::env::{ExecutorEnv, ExecutorEnvBuilder};
use self::monitor::MemoryMonitor;
use crate::{
    align_up,
    opcode::{MajorType, OpCode},
    receipt::ExitCode,
    Loader, MemoryImage, Program, Segment, SegmentRef, Session, SimpleSegmentRef,
};

/// The number of cycles required to compress a SHA-256 block.
const SHA_CYCLES: usize = 72;

/// Number of cycles required to complete a BigInt operation.
const BIGINT_CYCLES: usize = 9;

/// The Executor provides an implementation for the execution phase.
///
/// The proving phase uses an execution trace generated by the Executor.
pub struct Executor<'a> {
    env: ExecutorEnv<'a>,
    pre_image: MemoryImage,
    monitor: MemoryMonitor,
    pc: u32,
    init_cycles: usize,
    body_cycles: usize,
    segment_cycle: usize,
    segments: Vec<Box<dyn SegmentRef>>,
    insn_counter: u32,
    split_insn: Option<u32>,
    const_cycles: usize,
    pending_syscall: Option<SyscallRecord>,
    syscalls: Vec<SyscallRecord>,
    exit_code: Option<ExitCode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyscallRecord {
    pub to_guest: Vec<u32>,
    pub regs: (u32, u32),
}

#[derive(Clone)]
struct OpCodeResult {
    pc: u32,
    exit_code: Option<ExitCode>,
    extra_cycles: usize,
}

impl OpCodeResult {
    fn new(pc: u32, exit_code: Option<ExitCode>, extra_cycles: usize) -> Self {
        Self {
            pc,
            exit_code,
            extra_cycles,
        }
    }
}

// Capture the journal output in a buffer that we can access afterwards.
#[derive(Clone, Default)]
struct Journal {
    buf: Rc<RefCell<Vec<u8>>>,
}

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.borrow_mut().flush()
    }
}

impl<'a> Executor<'a> {
    /// Construct a new [Executor] from a [MemoryImage] and entry point.
    ///
    /// Before a guest program is proven, the [Executor] is responsible for
    /// deciding where a zkVM program should be split into [Segment]s and what
    /// work will be done in each segment. This is the execution phase:
    /// the guest program is executed to determine how its proof should be
    /// divided into subparts.
    pub fn new(env: ExecutorEnv<'a>, image: MemoryImage, pc: u32) -> Self {
        let pre_image = image.clone();
        let monitor = MemoryMonitor::new(image, env.trace_callback.is_some());
        let loader = Loader::new();
        let init_cycles = loader.init_cycles();
        let fini_cycles = loader.fini_cycles();
        let const_cycles = init_cycles + fini_cycles + SHA_CYCLES + ZK_CYCLES;
        Self {
            env,
            pre_image,
            monitor,
            pc,
            init_cycles,
            body_cycles: 0,
            segment_cycle: init_cycles,
            segments: Vec::new(),
            insn_counter: 0,
            split_insn: None,
            const_cycles,
            pending_syscall: None,
            syscalls: Vec::new(),
            exit_code: None,
        }
    }

    /// Construct a new [Executor] from the ELF binary of the guest program you
    /// want to run and an [ExecutorEnv] containing relevant environmental
    /// configuration details.
    /// # Example
    /// ```
    /// use risc0_zkvm::{serde::to_vec, Executor, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let spec = SpecWithIters(BenchmarkSpec::SimpleLoop, 1);
    /// let env = ExecutorEnv::builder()
    ///     .add_input(&to_vec(&spec).unwrap())
    ///     .build()
    ///     .unwrap();
    /// let mut exec = Executor::from_elf(env, BENCH_ELF).unwrap();
    /// ```
    pub fn from_elf(env: ExecutorEnv<'a>, elf: &[u8]) -> Result<Self> {
        let program = Program::load_elf(&elf, MEM_SIZE as u32)?;
        let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;
        Ok(Self::new(env, image, program.entry))
    }

    /// Run the executor until [ExitCode::Paused] or [ExitCode::Halted] is
    /// reached, producing a [Session] as a result.
    /// # Example
    /// ```
    /// use risc0_zkvm::{serde::to_vec, Executor, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let spec = SpecWithIters(BenchmarkSpec::SimpleLoop, 1);
    /// let env = ExecutorEnv::builder()
    ///    .add_input(&to_vec(&spec).unwrap())
    ///    .build()
    ///    .unwrap();
    /// let mut exec = Executor::from_elf(env, BENCH_ELF).unwrap();
    /// let session = exec.run().unwrap();
    /// ```
    pub fn run(&mut self) -> Result<Session> {
        self.run_with_callback(|segment| Ok(Box::new(SimpleSegmentRef::new(segment))))
    }

    /// Run the executor until [ExitCode::Paused] or [ExitCode::Halted] is
    /// reached, producing a [Session] as a result.
    pub fn run_with_callback<F>(&mut self, mut callback: F) -> Result<Session>
    where
        F: FnMut(Segment) -> Result<Box<dyn SegmentRef>>,
    {
        if let Some(ExitCode::Halted(_)) = self.exit_code {
            bail!("cannot resume an execution which exited with ExitCode::Halted");
        }

        self.monitor.clear_session();

        let journal = Journal::default();
        self.env
            .io
            .borrow_mut()
            .with_write_fd(fileno::JOURNAL, journal.clone());

        let mut run_loop = || -> Result<ExitCode> {
            loop {
                if let Some(exit_code) = self.step() {
                    let total_cycles = self.total_cycles();
                    log::debug!("exit_code: {exit_code:?}, total_cycles: {total_cycles}");
                    assert!(total_cycles <= (1 << self.env.segment_limit_po2));
                    let pre_image = self.pre_image.clone();
                    let post_image = self.monitor.build_image(self.pc);
                    let post_image_id = post_image.compute_id();
                    let syscalls = take(&mut self.syscalls);
                    let faults = take(&mut self.monitor.faults);
                    let segment = Segment::new(
                        pre_image,
                        post_image_id,
                        faults,
                        syscalls,
                        exit_code,
                        self.split_insn,
                        log2_ceil(total_cycles.next_power_of_two()),
                        self.segments
                            .len()
                            .try_into()
                            .context("Too many segments to fit in u32")?,
                        self.body_cycles,
                    );
                    let segment_ref = callback(segment)?;
                    self.segments.push(segment_ref);
                    match exit_code {
                        ExitCode::SystemSplit => self.split(post_image),
                        ExitCode::SessionLimit => bail!("Session limit exceeded"),
                        ExitCode::Paused(inner) => {
                            log::debug!("Paused({inner}): {}", self.segment_cycle);
                            self.split(post_image);
                            return Ok(exit_code);
                        }
                        ExitCode::Halted(inner) => {
                            log::debug!("Halted({inner}): {}", self.segment_cycle);
                            return Ok(exit_code);
                        }
                        ExitCode::Fault(pc) => {
                            log::debug!("Fault: cycle:{} pc:{}", self.segment_cycle, pc);
                            return Ok(exit_code);
                        }
                    };
                };
            }
        };

        let exit_code = run_loop()?;
        if let ExitCode::Fault(pc) = exit_code {
            // Create a segment that shows that the next instruction will fault
            // get_fault_segment(pc, self.monitor)
        }
        self.exit_code = Some(exit_code);
        Ok(Session::new(
            take(&mut self.segments),
            journal.buf.take(),
            exit_code,
        ))
    }

    fn split(&mut self, pre_image: MemoryImage) {
        self.pre_image = pre_image;
        self.body_cycles = 0;
        self.split_insn = None;
        self.insn_counter = 0;
        self.segment_cycle = self.init_cycles;
        self.monitor.clear_segment();
    }

    /// Execute a single instruction.
    ///
    /// This can be directly used by debuggers.
    pub fn step(&mut self) -> Option<ExitCode> {
        if let Some(limit) = self.env.get_session_limit() {
            if self.session_cycle() >= limit {
                return Some(ExitCode::SessionLimit);
            }
        }

        let insn = self.monitor.load_u32(self.pc);
        let opcode = match OpCode::decode(insn, self.pc) {
            Err(_) => return Some(ExitCode::Fault(self.pc)),
            Ok(op) => op,
        };

        let op_result = if opcode.major == MajorType::ECall {
            match self.ecall() {
                Err(_) => return Some(ExitCode::Fault(self.pc)),
                Ok(OpRes) => OpRes,
            }
        } else {
            let registers = self.monitor.load_registers();
            let mut hart = HartState {
                registers,
                pc: self.pc,
                last_register_write: None,
            };

            let mut exec = InstructionExecutor {
                mem: &mut self.monitor,
                hart_state: &mut hart,
            };
            if let Err(_) = exec.step() {
                return Some(ExitCode::Fault(self.pc));
            }

            if let Some(idx) = hart.last_register_write {
                self.monitor.store_register(idx, hart.registers[idx]);
            }

            OpCodeResult::new(hart.pc, None, 0)
        };

        // try to execute the next instruction
        // if the segment limit is exceeded:
        // * don't increment the PC
        // * don't record any activity
        // * return ExitCode::SystemSplit
        // otherwise, commit memory and hart

        let segment_limit = self.env.get_segment_limit();
        let total_pending_cycles = self.total_cycles() + opcode.cycles + op_result.extra_cycles;
        // log::debug!(
        //     "cycle: {}, segment: {}, total: {}",
        //     self.segment_cycle,
        //     total_pending_cycles,
        //     self.total_cycles()
        // );
        let exit_code = if total_pending_cycles > segment_limit {
            self.split_insn = Some(self.insn_counter);
            log::debug!("split: [{}] pc: 0x{:08x}", self.segment_cycle, self.pc,);
            self.monitor.undo();
            Some(ExitCode::SystemSplit)
        } else {
            self.advance(opcode, op_result)
        };
        exit_code
    }

    fn advance(&mut self, opcode: OpCode, op_result: OpCodeResult) -> Option<ExitCode> {
        log::trace!(
            "[{}] pc: 0x{:08x}, insn: 0x{:08x} => {:?}",
            self.segment_cycle,
            self.pc,
            opcode.insn,
            opcode
        );

        if let Some(ref trace_callback) = self.env.trace_callback {
            trace_callback.borrow_mut()(TraceEvent::InstructionStart {
                cycle: self.session_cycle() as u32,
                pc: self.pc,
            })
            .unwrap();

            for event in self.monitor.trace_events.iter() {
                trace_callback.borrow_mut()(event.clone()).unwrap();
            }
        }

        self.pc = op_result.pc;
        self.insn_counter += 1;
        self.body_cycles += opcode.cycles + op_result.extra_cycles;
        let page_read_cycles = self.monitor.page_read_cycles;
        // log::debug!("page_read_cycles: {page_read_cycles}");
        self.segment_cycle = self.init_cycles + page_read_cycles + self.body_cycles;
        self.monitor.commit(self.session_cycle());
        if let Some(syscall) = self.pending_syscall.take() {
            self.syscalls.push(syscall);
        }
        op_result.exit_code
    }

    fn total_cycles(&self) -> usize {
        self.const_cycles
            + self.monitor.page_read_cycles
            + self.monitor.page_write_cycles
            + self.body_cycles
    }

    fn session_cycle(&self) -> usize {
        self.segments.len() * self.env.get_segment_limit() + self.segment_cycle
    }

    fn ecall(&mut self) -> Result<OpCodeResult> {
        match self.monitor.load_register(REG_T0) {
            ecall::HALT => self.ecall_halt(),
            ecall::INPUT => self.ecall_input(),
            ecall::SOFTWARE => self.ecall_software(),
            ecall::SHA => self.ecall_sha(),
            ecall::BIGINT => self.ecall_bigint(),
            ecall => bail!("Unknown ecall {ecall:?}"),
        }
    }

    fn ecall_halt(&mut self) -> Result<OpCodeResult> {
        let tot_reg = self.monitor.load_register(REG_A0);
        let output_ptr = self.monitor.load_register(REG_A1);
        let halt_type = tot_reg & 0xff;
        let user_exit = (tot_reg >> 8) & 0xff;
        self.monitor
            .load_array::<{ DIGEST_WORDS * WORD_SIZE }>(output_ptr);

        match halt_type {
            halt::TERMINATE => Ok(OpCodeResult::new(
                self.pc,
                Some(ExitCode::Halted(user_exit)),
                0,
            )),
            halt::PAUSE => Ok(OpCodeResult::new(
                self.pc + WORD_SIZE as u32,
                Some(ExitCode::Paused(user_exit)),
                0,
            )),
            _ => bail!("Illegal halt type: {halt_type}"),
        }
    }

    fn ecall_input(&mut self) -> Result<OpCodeResult> {
        log::debug!("ecall(input)");
        let in_addr = self.monitor.load_register(REG_A0);
        self.monitor
            .load_array::<{ DIGEST_WORDS * WORD_SIZE }>(in_addr);
        Ok(OpCodeResult::new(self.pc + WORD_SIZE as u32, None, 0))
    }

    fn ecall_sha(&mut self) -> Result<OpCodeResult> {
        let out_state_ptr = self.monitor.load_register(REG_A0);
        let in_state_ptr = self.monitor.load_register(REG_A1);
        let mut block1_ptr = self.monitor.load_register(REG_A2);
        let mut block2_ptr = self.monitor.load_register(REG_A3);
        let count = self.monitor.load_register(REG_A4);

        let in_state: [u8; DIGEST_BYTES] = self.monitor.load_array(in_state_ptr);
        let mut state: [u32; DIGEST_WORDS] = bytemuck::cast_slice(&in_state).try_into().unwrap();
        for word in &mut state {
            *word = word.to_be();
        }

        log::debug!("Initial sha state: {state:08x?}");
        for _ in 0..count {
            let mut block = [0u32; BLOCK_WORDS];
            for i in 0..DIGEST_WORDS {
                block[i] = self.monitor.load_u32(block1_ptr + (i * WORD_SIZE) as u32);
            }
            for i in 0..DIGEST_WORDS {
                block[DIGEST_WORDS + i] =
                    self.monitor.load_u32(block2_ptr + (i * WORD_SIZE) as u32);
            }
            log::debug!("Compressing block {block:02x?}");
            sha2::compress256(
                &mut state,
                &[*generic_array::GenericArray::from_slice(
                    bytemuck::cast_slice(&block),
                )],
            );

            block1_ptr += BLOCK_BYTES as u32;
            block2_ptr += BLOCK_BYTES as u32;
        }
        log::debug!("Final sha state: {state:08x?}");

        for word in &mut state {
            *word = u32::from_be(*word);
        }

        self.monitor
            .store_region(out_state_ptr, bytemuck::cast_slice(&state));

        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            SHA_CYCLES * count as usize,
        ))
    }

    // Computes the state transitions for the BIGINT ecall.
    // Take reads inputs x, y, and N and writes output z = x * y mod N.
    // Note that op is currently ignored but must be set to 0.
    fn ecall_bigint(&mut self) -> Result<OpCodeResult> {
        let z_ptr = self.monitor.load_register(REG_A0);
        let op = self.monitor.load_register(REG_A1);
        let x_ptr = self.monitor.load_register(REG_A2);
        let y_ptr = self.monitor.load_register(REG_A3);
        let n_ptr = self.monitor.load_register(REG_A4);

        let mut load_bigint_le_bytes = |ptr: u32| -> [u8; bigint::WIDTH_BYTES] {
            let mut arr = [0u32; bigint::WIDTH_WORDS];
            for i in 0..bigint::WIDTH_WORDS {
                arr[i] = self.monitor.load_u32(ptr + (i * WORD_SIZE) as u32).to_le();
            }
            bytemuck::cast(arr)
        };

        if op != 0 {
            anyhow::bail!("ecall_bigint preflight: op must be set to 0");
        }

        // Load inputs.
        let x = U256::from_le_bytes(load_bigint_le_bytes(x_ptr));
        let y = U256::from_le_bytes(load_bigint_le_bytes(y_ptr));
        let n = U256::from_le_bytes(load_bigint_le_bytes(n_ptr));

        // Compute modular multiplication, or simply multiplication if n == 0.
        let z: U256 = if n == U256::ZERO {
            x.checked_mul(&y).unwrap()
        } else {
            let (w_lo, w_hi) = x.mul_wide(&y);
            let w = w_hi.concat(&w_lo);
            let z = w.rem(&NonZero::<U512>::from_uint(n.resize()));
            z.resize()
        };

        // Store result.
        for (i, word) in bytemuck::cast::<_, [u32; bigint::WIDTH_WORDS]>(z.to_le_bytes())
            .into_iter()
            .enumerate()
        {
            self.monitor
                .store_u32(z_ptr + (i * WORD_SIZE) as u32, word.to_le());
        }

        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            BIGINT_CYCLES,
        ))
    }

    fn ecall_software(&mut self) -> Result<OpCodeResult> {
        let to_guest_ptr = self.monitor.load_register(REG_A0);
        let to_guest_words = self.monitor.load_register(REG_A1);
        let name_ptr = self.monitor.load_register(REG_A2);
        let syscall_name = self.monitor.load_string(name_ptr)?;
        log::trace!("Guest called syscall {syscall_name:?} requesting {to_guest_words} words back");

        let chunks = align_up(to_guest_words as usize, WORD_SIZE);

        let syscall = if let Some(syscall) = self.pending_syscall.clone() {
            log::debug!("Replay syscall: {syscall:?}");
            syscall
        } else {
            let mut to_guest = vec![0; to_guest_words as usize];
            let handler = self
                .env
                .get_syscall(&syscall_name)
                .ok_or(anyhow!("Unknown syscall: {syscall_name:?}"))?;
            let (a0, a1) =
                handler
                    .borrow_mut()
                    .syscall(&syscall_name, &mut self.monitor, &mut to_guest)?;
            let syscall = SyscallRecord {
                to_guest,
                regs: (a0, a1),
            };
            self.pending_syscall = Some(syscall.clone());
            syscall
        };

        let (a0, a1) = syscall.regs;
        self.monitor
            .store_region(to_guest_ptr, bytemuck::cast_slice(&syscall.to_guest));
        self.monitor.store_register(REG_A0, a0);
        self.monitor.store_register(REG_A1, a1);

        log::trace!("Syscall returned a0: {a0:#X}, a1: {a1:#X}, chunks: {chunks}");

        // One cycle for the ecall cycle, then one for each chunk or
        // portion thereof then one to save output (a0, a1)
        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            1 + chunks + 1,
        ))
    }
}

/// An event traced from the running VM.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum TraceEvent {
    /// An instruction has started at the given program counter
    InstructionStart {
        /// Cycle number since startup
        cycle: u32,
        /// Program counter of the instruction being executed
        pc: u32,
    },

    /// A register has been set
    RegisterSet {
        /// Register ID (0-16)
        reg: usize,
        /// New value in the register
        value: u32,
    },

    /// A memory location has been written
    MemorySet {
        /// Address of word that's been written
        addr: u32,
        /// Value of word that's been written
        value: u32,
    },
}

impl Debug for TraceEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InstructionStart { cycle, pc } => {
                write!(f, "InstructionStart({cycle}, 0x{pc:08X})")
            }
            Self::RegisterSet { reg, value } => write!(f, "RegisterSet({reg}, 0x{value:08X})"),
            Self::MemorySet { addr, value } => write!(f, "MemorySet(0x{addr:08X}, 0x{value:08X})"),
        }
    }
}
