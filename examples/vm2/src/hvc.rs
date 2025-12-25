use core::{arch::asm, usize};

use axhvc::HyperCallCode;

#[inline(always)]
fn trigger_hypercall(
    code: HyperCallCode,
    num_args: u64, // 新增：参数数量
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "hvc #0",
            in("x0") code as u64,
            in("x1") num_args, // 用 x1 传递参数数量
            in("x2") arg1,
            in("x3") arg2,
            in("x4") arg3,
            in("x5") arg4,
            in("x6") arg5,
            lateout("x0") result,
            options(nostack, preserves_flags)
        );
    }
    result
}

pub fn hvc_establish_connect(args: &[u64]) -> isize {
    trigger_hypercall(
        HyperCallCode::HConEstablishConnect,
        args.len() as u64,
        args.get(0).copied().unwrap_or(0),
        args.get(1).copied().unwrap_or(0),
        args.get(2).copied().unwrap_or(0),
        args.get(3).copied().unwrap_or(0),
        args.get(4).copied().unwrap_or(0),
    )
}

pub fn hvc_unestablish_connect(args: &[u64]) -> isize {
    trigger_hypercall(
        HyperCallCode::HConUnEstablishConnect,
        args.len() as u64,
        args.get(0).copied().unwrap_or(0),
        args.get(1).copied().unwrap_or(0),
        args.get(2).copied().unwrap_or(0),
        args.get(3).copied().unwrap_or(0),
        args.get(4).copied().unwrap_or(0),
    )
}

pub fn hvc_send_ipi(target_vm_id: usize, target_vcpu_id: usize, vector: usize) -> isize {
    trigger_hypercall(
        HyperCallCode::HIVCSendIPI, 
        3,                          // 参数数量: 3
        target_vm_id as u64,        // arg1 -> x2
        target_vcpu_id as u64,      // arg2 -> x3
        vector as u64,              // arg3 -> x4
        0,                          // arg4 (unused)
        0,                          // arg5 (unused)
    )
}