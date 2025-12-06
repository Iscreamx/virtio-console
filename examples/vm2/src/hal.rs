use arceos_api::mem::ax_alloc_coherent;
use axhal::mem::{PhysAddr, VirtAddr, phys_to_virt, virt_to_phys};
use virtio_drivers::{
    Hal,
    BufferDirection,
};
use core::alloc::Layout;
use core::ptr::NonNull;
pub struct MyHal;

unsafe impl Hal for MyHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        let layout = Layout::from_size_align(pages * 0x1000, 0x1000).unwrap();
        let dma_info = unsafe { ax_alloc_coherent(layout) }.expect("DMA alloc failed");
        let paddr = dma_info.bus_addr.as_u64() as usize;
        let vaddr = dma_info.cpu_addr;
        (paddr, vaddr)
    }

    unsafe fn dma_dealloc(_paddr: usize, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        let vaddr = phys_to_virt(PhysAddr::from(paddr));
        NonNull::new(vaddr.as_mut_ptr()).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        let ptr = buffer.as_ptr() as *const u8;
        let vaddr = VirtAddr::from(ptr as usize);
        let paddr = virt_to_phys(vaddr);
        paddr.into()
    }

    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}