#![no_std]
#![no_main]

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

mod hal;
mod hvc;
use hal::MyHal;

#[unsafe(no_mangle)]
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
    // 忙等待，等待 VM2 启动 (约 6s)
    for _ in 0..1_000_000_000 {
        core::hint::spin_loop();
    }
    for _ in 0..1_000_000_000 {
        core::hint::spin_loop();
    }
    for _ in 0..1_000_000_000 {
        core::hint::spin_loop();
    }

    // 3. 主动交互循环
    println!("VM1: Starting interaction loop...");
    
    for i in 0..10 {
        // 发送 Ping，主动触发 Hypervisor 处理
        let msg = alloc::format!("Ping {}\n", i);
        print!("VM1: Sending -> {}", msg);
        for &c in msg.as_bytes() {
            console.send(c).expect("Failed to send");
        }

        // 等待回复 (约 1s)
        let mut received = false;
        let mut cycles = 0;
        
        while cycles < 2_000_000_000 {
            if cycles % 10_000_000 == 0 {
                let _ = console.send(0); 
            }

            if let Some(c) = console.recv(true).expect("Failed to recv") {
                if c != 0 {
                    print!("{}", c as char);
                    received = true;
                    if c == b'\n' {
                        break;
                    }
                }
            }
            cycles += 1;
            core::hint::spin_loop();
        }
        
        if !received {
            println!("VM1: No reply received for round {}", i);
        } else {
            println!("");
        }
    }
    
    let _ = hvc::hvc_unestablish_connect( &[1, 2]);
    println!("\nVM1: Exiting.");
}