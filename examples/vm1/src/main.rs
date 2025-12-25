#![cfg_attr(feature = "axstd", no_std)]
#![cfg_attr(feature = "axstd", no_main)]

cfg_if::cfg_if! {
    if #[cfg(all(target_arch = "aarch64", feature = "aarch64-qemu-virt"))] {
        extern crate axplat_aarch64_qemu_virt;
    } else if #[cfg(all(target_arch = "aarch64", feature = "aarch64-raspi4"))] {
        extern crate axplat_aarch64_raspi;
    } else if #[cfg(all(target_arch = "aarch64", feature = "aarch64-phytium-pi"))] {
        extern crate axplat_aarch64_phytium_pi;
    } else if #[cfg(all(target_arch = "aarch64", feature = "aarch64-bsta1000b"))] {
        extern crate axplat_aarch64_bsta1000b;
    } else if #[cfg(all(target_arch = "x86_64", feature = "x86-pc"))] {
        extern crate axplat_x86_pc;
    } else if #[cfg(all(target_arch = "riscv64", feature = "riscv64-qemu-virt"))] {
        extern crate axplat_riscv64_qemu_virt;
    } else if #[cfg(all(target_arch = "loongarch64", feature = "loongarch64-qemu-virt"))] {
        extern crate axplat_loongarch64_qemu_virt;
    } else if #[cfg(all(target_arch = "aarch64", feature = "aarch64-dyn"))] {
        extern crate axplat_aarch64_dyn;
    } else {
        #[cfg(target_os = "none")] // ignore in rust-analyzer & cargo test
        compile_error!("No platform crate linked!\n\nPlease add `extern crate <platform>` in your code.");
    }
}

#[macro_use]
extern crate axstd;
extern crate axhal;
extern crate alloc;
extern crate arceos_api;
extern crate virtio_drivers;

use core::ptr::NonNull;
use axhal::mem::{PhysAddr, phys_to_virt};
use virtio_drivers::{
    device::console::VirtIOConsole,
    transport::mmio::MmioTransport,
};

#[cfg(feature = "axstd")]
use axstd::println;

mod hal;
mod hvc;
use hal::MyHal;

fn irq_handler_33() {
    println!("SUCCESS: VM2 received Cross-VM IPI!");
}

#[cfg_attr(feature = "axstd", unsafe(no_mangle))]
fn main() {
    let paddr: PhysAddr = PhysAddr::from(0x0a00_0000); // VM1 MMIO Base
    let mmio_base = phys_to_virt(paddr);
    println!("MMIO base address: {:?}", mmio_base);

    // 1. 先初始化驱动
    let header = unsafe {
        NonNull::new_unchecked(mmio_base.as_mut_ptr().cast())
    };
    let transport = unsafe {
        MmioTransport::new(header)
    }.expect("Failed to create MMIO transport");

    let mut console = VirtIOConsole::<MyHal, _>::new(transport)
        .expect("Failed to create console driver");

    println!("\n--------------------------------");
    println!("virtio-console test start...");
    
    // 2. 建立连接
    println!("Establishing console connection...");
    let _ = hvc::hvc_establish_connect(&[1, 2]);

    println!("VM1: Waiting for VM2 to boot...");

    println!("VM1: Starting interaction loop (5 rounds)...");
    
    for i in 0..5 {
        for _ in 0..1_000_000_000 {
            core::hint::spin_loop();
        }

        let msg = alloc::format!("Ping {}\n", i);
        print!("VM1: Sending -> {}", msg);
        for &c in msg.as_bytes() {
            console.send(c).expect("Failed to send");
        }

        let ret = hvc::hvc_send_ipi(2, 0, 33);
        if ret == 0 {
        } else {
            println!("VM1: Failed to inject IPI, error code: {}", ret);
        }
    }
    

    println!("\nVM1: All 5 pings sent. Exiting.");
    let _ = hvc::hvc_unestablish_connect( &[1, 2]);
    println!("\nVM1: Exiting.");
}
