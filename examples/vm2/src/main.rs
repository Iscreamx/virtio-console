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

    println!("\n--------------------------------");
    println!("virtio-console test start...");
    
    // 2. 建立连接 (连接到 VM1)
    println!("Establishing console connection...");
    let _ = hvc::hvc_establish_connect(&[1, 1]); 

    println!("VM2: Ready to echo...");

    let mut cycles: usize = 0;
    let timeout: usize = 300_000_000; 

    while cycles < timeout {
        // 尝试接收数据
        if let Some(c) = console.recv(true).expect("Failed to receive") {
            // 收到数据后立即回显
            console.send(c).expect("Failed to echo");
            cycles = 0; // 重置超时计数
        } else {
            // 每 10_000_000 次循环发送一个心跳，触发 Hypervisor 处理队列
            if cycles % 10_000_000 == 0 {
                let _ = console.send(0); // 发送心跳
            }
            cycles += 1;
            core::hint::spin_loop();
        }
    }

    let _ = hvc::hvc_unestablish_connect( &[1, 1]);
    println!("VM2: Exiting.");
}