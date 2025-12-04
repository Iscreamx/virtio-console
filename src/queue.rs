use core::error;


use axaddrspace::GuestPhysAddr;
use axaddrspace::HostPhysAddr;
use axhal::mem::phys_to_virt;

/// Virtio descriptor flags
pub const VRING_DESC_F_NEXT: u16 = 1;
pub const VRING_DESC_F_WRITE: u16 = 2;
pub const VRING_DESC_F_INDIRECT: u16 = 4;

/// The descriptor table element.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtqDesc {
    /// Address (guest-physical).
    pub addr: u64,
    /// Length.
    pub len: u32,
    /// The flags as indicated above.
    pub flags: u16,
    /// Next field if flags & NEXT
    pub next: u16,
}

/// The available ring.
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; 0], // Flexible array member
}

/// The used ring element.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtqUsedElem {
    /// Index of start of used descriptor chain.
    pub id: u32,
    /// Total length of the descriptor chain which was used (written to).
    pub len: u32,
}

/// The used ring.
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VirtqUsedElem; 0], // Flexible array member
}

/// A helper struct to manage a Virtqueue configuration and state.
#[derive(Debug)]
pub struct VirtQueue {
    /// The index of the queue.
    idx: u16,
    /// The size of the queue (number of descriptors).
    size: u16,
    /// The maximum size supported.
    max_size: u16,
    /// Whether the queue is ready.
    ready: bool,
    
    /// Guest Physical Address of the Descriptor Table.
    desc_table: GuestPhysAddr,
    /// Guest Physical Address of the Available Ring.
    avail_ring: GuestPhysAddr,
    /// Guest Physical Address of the Used Ring.
    used_ring: GuestPhysAddr,

    /// The last index we saw in the available ring.
    last_avail_idx: u16,
}

impl VirtQueue {
    pub fn new(idx: u16, max_size: u16) -> Self {
        Self {
            idx,
            size: 0,
            max_size,
            ready: false,
            desc_table: GuestPhysAddr::from(0usize),
            avail_ring: GuestPhysAddr::from(0usize),
            used_ring: GuestPhysAddr::from(0usize),
            last_avail_idx: 0,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn set_size(&mut self, size: u16) {
        if size > self.max_size || size == 0 || (size & (size - 1)) != 0 {
            // Size must be power of 2 and <= max_size
            return;
        }
        self.size = size;
    }

    pub fn set_desc_addr(&mut self, addr: u64) {
        self.desc_table = GuestPhysAddr::from(addr as usize);
    }

    pub fn set_avail_addr(&mut self, addr: u64) {
        self.avail_ring = GuestPhysAddr::from(addr as usize);
    }

    pub fn set_used_addr(&mut self, addr: u64) {
        self.used_ring = GuestPhysAddr::from(addr as usize);
    }

    pub fn desc_table_addr(&self) -> GuestPhysAddr {
        self.desc_table
    }

    pub fn avail_ring_addr(&self) -> GuestPhysAddr {
        self.avail_ring
    }

    pub fn used_ring_addr(&self) -> GuestPhysAddr {
        self.used_ring
    }
    
    pub fn last_avail_idx(&self) -> u16 {
        self.last_avail_idx
    }
    
    pub fn set_last_avail_idx(&mut self, idx: u16) {
        self.last_avail_idx = idx;
    }
    
    pub fn inc_last_avail_idx(&mut self) {
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
    }


    pub fn pop_available(&mut self, translate: &dyn Fn(GuestPhysAddr) -> Option<(HostPhysAddr, usize)>) -> Option<u16> {
        if !self.ready {
            error!("Attempted to pop from an unready VirtQueue");
            return None;
        }
        // info!("Popping available descriptor from VirtQueue {}", self.idx);
        // info!("VirtQueue state: size={}, last_avail_idx={}", self.size, self.last_avail_idx);

        let (avail_ring_hpa, _) = translate(self.avail_ring)?;
        let avail_ring_hva = phys_to_virt(avail_ring_hpa).as_usize() as *const u8;
        let avail_idx_ptr = unsafe { avail_ring_hva.add(2) as *const u16 };
        // info!("Available ring HVA: {:?}", avail_ring_hva);
        // info!("Reading available idx from ptr: {:?}", avail_idx_ptr);
        let avail_idx = unsafe { *avail_idx_ptr };
        // info!("Available idx: {}", avail_idx);
        if self.last_avail_idx == avail_idx {
            return None;
        }

        let ring_offset = 4 + (self.last_avail_idx % self.size) as usize * 2;
        let elem_hva = unsafe { avail_ring_hva.add(ring_offset) as *const u16 };
        let head_idx = unsafe { *elem_hva };
        // info!("Popped descriptor index: {}", head_idx);
        // info!("Updating last_avail_idx from {} to {}", self.last_avail_idx, self.last_avail_idx.wrapping_add(1));
        // info!("----------------------------------------");
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        Some(head_idx)
    }

    // get_descriptor
    pub fn get_descriptor(&self, idx: u16, translate: &dyn Fn(GuestPhysAddr) -> Option<(HostPhysAddr, usize)>) -> Option<VirtqDesc> {
        if idx >= self.size {
            return None;
        }
        let desc_size = core::mem::size_of::<VirtqDesc>();
        let offset = idx as usize * desc_size;
        let (desc_hpa, _) = translate(self.desc_table + offset)?;
        let desc_hva = phys_to_virt(desc_hpa).as_usize() as *const VirtqDesc;
        Some(unsafe { *desc_hva })
    }

    // add_used
    pub fn add_used(
        &mut self,
        head_idx: u16,
        len: u32,
        translate: &dyn Fn(GuestPhysAddr) -> Option<(HostPhysAddr, usize)>,
    ) -> axerrno::AxResult<()> {
        let (used_ring_hpa, _) = translate(self.used_ring).ok_or(axerrno::AxError::InvalidInput)?;
        let used_ring_hva = phys_to_virt(used_ring_hpa).as_usize() as *mut u8;

        let idx_ptr = unsafe { used_ring_hva.add(2) as *mut u16 };
        let idx = unsafe { *idx_ptr };

        let elem_size = core::mem::size_of::<VirtqUsedElem>();
        let ring_offset = 4 + (idx % self.size) as usize * elem_size;
        let elem_ptr = unsafe { used_ring_hva.add(ring_offset) as *mut VirtqUsedElem };

        unsafe {
            *elem_ptr = VirtqUsedElem { id: head_idx as u32, len };
            *idx_ptr = idx.wrapping_add(1);
        }
        Ok(())
    }
}