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

static mut CONSOLE: Option<VirtIOConsole<MyHal, MmioTransport>> = None;
static mut IRQ_COUNT: usize = 0;

fn irq_handler_33() {
    println!("IRQ 33 received!");

    unsafe {
        IRQ_COUNT += 1;
        if let Some(ref mut console) = CONSOLE {
            // 1. 关键：发送一个空字节（心跳），触发 Hypervisor 的 notify_queue
            // Hypervisor 在处理 notify 时会调用 process_rx_queue，把数据从共享缓冲区拉进 VirtIO 队列
            let _ = console.send(0); 

            // 2. 现在再尝试接收，数据应该已经由 Hypervisor 搬运过来了
            while let Ok(Some(c)) = console.recv(true) {
                let c: u8 = c; 
                if c != 0 {
                    axstd::print!("{}", c as char);
                }
            }
        }
    }
    println!("IRQ 33 handled.");
}

#[cfg_attr(feature = "axstd", unsafe(no_mangle))]
fn main() {
    // VM2 的 MMIO 地址
    let paddr: PhysAddr = PhysAddr::from(0x0a00_0000);
    let mmio_base = phys_to_virt(paddr);
    println!("MMIO base address: {:?}", mmio_base);

    // 1. 初始化驱动
    let header = unsafe {
        NonNull::new_unchecked(mmio_base.as_mut_ptr().cast())
    };
    let transport = unsafe {
        MmioTransport::new(header)
    }.expect("Failed to create MMIO transport");

    let mut console = VirtIOConsole::<MyHal, _>::new(transport)
        .expect("Failed to create console driver");
    
    unsafe {
        CONSOLE = Some(console);
    }

    println!("\n--------------------------------");
    println!("virtio-console test start...");
    
    println!("Establishing console connection...");
    let _ = hvc::hvc_establish_connect(&[1, 1]); 

    axhal::irq::register(33 , irq_handler_33);
    axhal::irq::set_enable(33, true);

    println!("VM2: Ready to echo...");


    while unsafe { IRQ_COUNT < 5 } {
        core::hint::spin_loop();
    }
    let _ = hvc::hvc_unestablish_connect( &[1, 1]);
    println!("\nVM2: Exiting.");
}
