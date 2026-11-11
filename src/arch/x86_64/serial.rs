// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2026 Adam55Lord

//! The debug serial port. x86_64: a 16550 UART behind I/O ports (COM1). Other architectures
//! provide the same two names backed by their UART (PL011 on arm64, NS16550/SBI on RISC-V).

pub use super::uart16550::{init, COM1};
