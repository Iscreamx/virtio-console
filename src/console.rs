use alloc::vec;
use alloc::{vec::Vec, boxed::Box, sync::Arc, string::{String, ToString}};
use spin::Mutex;

use axaddrspace::{
    GuestPhysAddr,
    GuestPhysAddrRange, 
    device::AccessWidth
};
use memory_addr::PhysAddr;
use axdevice_base::BaseDeviceOps;
use axerrno::AxResult;
use axvmconfig::EmulatedDeviceType;
use log::{info, warn, error};

use super::config::*;
use super::queue::{VirtQueue, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

use core::sync::atomic::{AtomicU64, Ordering};

const QUEUE_MAX_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

#[derive(Clone, Debug)]
pub struct DeviceConsoleConnection {
    pub send_vm_ids: Vec<usize>,         // Target VM IDs that can send data
    pub recv_vm_ids: Vec<usize>,         // Source VM IDs that can receive data
    pub send_buffers: Vec<usize>,        // Send buffer addresses, one-to-one with send_vm_ids
    pub recv_buffers: Vec<usize>,        // Receive buffer addresses, one-to-one with recv_vm_ids
    pub buffer_size: usize,
}

#[repr(C)]
struct RingBufferHeader {
    write_idx: AtomicU64, // Write position (monotonically increasing)
    read_idx: AtomicU64,  // Read position (monotonically increasing)
}

impl DeviceConsoleConnection {
    pub fn new(
        send_vm_ids: Vec<usize>,
        recv_vm_ids: Vec<usize>,
        send_buffers: Vec<usize>,
        recv_buffers: Vec<usize>,
        buffer_size: usize,
    ) -> Self {
        Self {
            send_vm_ids,
            recv_vm_ids,
            send_buffers,
            recv_buffers,
            buffer_size,
        }
    }

    /// Add a target VM for sending and its buffer
    pub fn add_send_vm(&mut self, vm_id: usize, buffer: usize) {
        self.send_vm_ids.push(vm_id);
        self.send_buffers.push(buffer);
    }

    /// Remove a specified target VM for sending
    pub fn remove_send_vm(&mut self, vm_id: usize) {
        if let Some(idx) = self.send_vm_ids.iter().position(|&id| id == vm_id) {
            self.send_vm_ids.remove(idx);
            self.send_buffers.remove(idx);
        }
    }
    /// Add a source VM for receiving and its buffer
    pub fn add_recv_vm(&mut self, vm_id: usize, buffer: usize) {
        self.recv_vm_ids.push(vm_id);
        self.recv_buffers.push(buffer);
    }

    /// Remove a specified source VM for receiving
    pub fn remove_recv_vm(&mut self, vm_id: usize) {
        if let Some(idx) = self.recv_vm_ids.iter().position(|&id| id == vm_id) {
            self.recv_vm_ids.remove(idx);
            self.recv_buffers.remove(idx);
        }
    }
}

/// Virtio Console Configuration Space
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

/// Internal state of the Virtio Console device
struct VirtioConsoleState {
    status: u32,
    interrupt_status: u32,
    host_features: u64,
    guest_features: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    queue_sel: u32,
    queues: [VirtQueue; NUM_QUEUES],
    config: ConsoleConfig,
}

impl VirtioConsoleState {
    fn new() -> Self {
        let queues = [
            VirtQueue::new(0, QUEUE_MAX_SIZE),
            VirtQueue::new(1, QUEUE_MAX_SIZE),
        ];
        
        Self {
            status: 0,
            interrupt_status: 0,
            // Feature bits: VIRTIO_F_VERSION_1 (32)
            host_features: 1 << 32, 
            guest_features: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queues,
            config: ConsoleConfig {
                cols: 80,
                rows: 24,
                max_nr_ports: 1,
                emerg_wr: 0,
            },
        }
    }

    fn current_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn current_queue(&self) -> Option<&VirtQueue> {
        self.queues.get(self.queue_sel as usize)
    }

    fn reset(&mut self) {
        self.status = 0;
        self.interrupt_status = 0;
        self.guest_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        
        // Reset all queues
        for queue in &mut self.queues {
            queue.set_ready(false);
            queue.set_size(0);
            queue.set_desc_addr(0);
            queue.set_avail_addr(0);
            queue.set_used_addr(0);
            queue.set_last_avail_idx(0);
        }
        
        // info!("VirtioConsole: Device reset");
    }
    
}

/// Virtio Serial (Console) Device
pub struct VirtioConsole {
    base_gpa: GuestPhysAddr,
    length: usize,
    _irq_id: usize,
    translate_fn: Arc<Box<dyn Fn(GuestPhysAddr) -> Option<(PhysAddr, usize)> + Send + Sync>>,
    state: Mutex<VirtioConsoleState>,
    connection: Mutex<Option<DeviceConsoleConnection>>,
}

impl VirtioConsole {
    pub fn new(
        base_gpa: usize,
        length: usize,
        _irq_id: usize,
        translate_fn: Arc<Box<dyn Fn(GuestPhysAddr) -> Option<(PhysAddr, usize)> + Send + Sync>>,
    ) -> Self {
        Self {
            base_gpa: GuestPhysAddr::from(base_gpa),
            length,
            _irq_id,
            translate_fn,
            state: Mutex::new(VirtioConsoleState::new()),
            connection: Mutex::new(None),
        }
    }

    fn translate(&self) -> &dyn Fn(GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        &**self.translate_fn
    }

    /// Add a target VM for sending and its buffer
    pub fn add_send_connection(&self, vm_id: usize, buffer: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.add_send_vm(vm_id, buffer);
            info!("Updated DeviceConsoleConnection after adding send VM: {}", vm_id);
            info!("  Send VMs: {:?}", conn.send_vm_ids);
            info!("  Recv VMs: {:?}", conn.recv_vm_ids);
        } else {
            info!("No existing DeviceConsoleConnection. Creating a new one for send VM: {}", vm_id);
            let new_conn = DeviceConsoleConnection::new(
                vec![vm_id],
                Vec::new(),
                vec![buffer],
                Vec::new(),
                4096,
            );
            *conn_guard = Some(new_conn);
        }
    }

    /// Remove a target VM for sending
    pub fn remove_send_connection(&self, vm_id: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.remove_send_vm(vm_id);
            info!("Updated DeviceConsoleConnection after removing send VM: {}", vm_id);
            info!("  Send VMs: {:?}", conn.send_vm_ids);
            info!("  Recv VMs: {:?}", conn.recv_vm_ids);
        }
    }

    /// Add a source VM for receiving and its buffer
    pub fn add_recv_connection(&self, vm_id: usize, buffer: usize) {
        {
            let mut conn_guard = self.connection.lock();
            if let Some(conn) = conn_guard.as_mut() {
                conn.add_recv_vm(vm_id, buffer);
                info!("Updated DeviceConsoleConnection after adding recv VM: {}", vm_id);
                info!("  Send VMs: {:?}", conn.send_vm_ids);
                info!("  Recv VMs: {:?}", conn.recv_vm_ids);
            } else {
                info!("No existing DeviceConsoleConnection. Creating a new one for recv VM: {}", vm_id);
                let new_conn = DeviceConsoleConnection::new(
                    Vec::new(),
                    vec![vm_id],
                    Vec::new(),
                    vec![buffer],
                    4096,
                );
                *conn_guard = Some(new_conn);
            }
        }

        let mut state = self.state.lock();
        self.process_rx_queue(&mut state);
    }

    /// Remove a source VM for receiving
    pub fn remove_recv_connection(&self, vm_id: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.remove_recv_vm(vm_id);
            info!("Updated DeviceConsoleConnection after removing recv VM: {}", vm_id);
            info!("  Send VMs: {:?}", conn.send_vm_ids);
            info!("  Recv VMs: {:?}", conn.recv_vm_ids);
        }
    }


    fn process_tx_queue(&self, state: &mut VirtioConsoleState) {
        let queue = &mut state.queues[1]; // TX Queue
        // info!("VirtioConsole: Processing TX Queue");
        // info!("VirtioConsole: Processing TX Queue. DescTable: {:#x}, Avail: {:#x}", 
        //    queue.desc_table_addr().as_usize(), 
        //    queue.avail_ring_addr().as_usize()
        //);
         // Process all available descriptor chains
        while let Some(head_idx) = queue.pop_available(self.translate()) {
            // info!("VirtioConsole: Processing TX descriptor chain starting at index {}", head_idx);
            let mut curr_idx = head_idx;
            let mut received_data = Vec::new();
            let mut valid_chain = true;

            loop {
                let desc = queue.get_descriptor(curr_idx, self.translate()).unwrap();
                if desc.addr == 0 || desc.len == 0 {
                    valid_chain = false;
                    break;
                }
                if (desc.flags & VRING_DESC_F_WRITE) == 0 && desc.len > 0 {
                    let gpa = GuestPhysAddr::from(desc.addr as usize);
                    match (self.translate_fn)(gpa) {
                        Some((hpa, limit)) => {
                            let hva = hpa.as_usize() as *const u8;
                            let data_len = desc.len.min(limit as u32) as usize;
                            let data = unsafe { core::slice::from_raw_parts(hva, data_len) };
                            received_data.extend_from_slice(data);
                        }
                        None => {
                            warn!("VirtioConsole: Invalid GPA {:#x} in TX descriptor", gpa);
                            valid_chain = false;
                            break;
                        }
                    }
                }
                if (desc.flags & VRING_DESC_F_NEXT) == 0 {
                    break;
                }
                curr_idx = desc.next;
                if curr_idx >= queue.size() {
                    valid_chain = false;
                    break;
                }
            }

            if valid_chain {
                let _ = queue.add_used(head_idx, received_data.len() as u32, self.translate());
                state.interrupt_status |= VIRTIO_MMIO_INT_VRING;

                // Forward to all send buffers
                let conn_guard = self.connection.lock();
                if let Some(conn) = conn_guard.as_ref() {
                    for (&vm_id, &buffer_addr) in conn.send_vm_ids.iter().zip(conn.send_buffers.iter()) {
                        // [Modification] Treat buffer as: Header + Data Ring
                        let header = unsafe { &*(buffer_addr as *const RingBufferHeader) };
                        let data_ptr = (buffer_addr + core::mem::size_of::<RingBufferHeader>()) as *mut u8;
                        let capacity = conn.buffer_size - core::mem::size_of::<RingBufferHeader>();

                        // Get current index
                        let mut w_idx = header.write_idx.load(Ordering::Acquire);
                        let r_idx = header.read_idx.load(Ordering::Acquire);

                        for &byte in &received_data {
                            // Check if buffer is full (write - read >= capacity)
                            if w_idx - r_idx >= capacity as u64 {
                                warn!("VirtioConsole: Ring buffer full for VM[{}]", vm_id);
                                break; 
                            }

                            // Write data (modulo mapping to ring space)
                            unsafe {
                                *data_ptr.add((w_idx as usize) % capacity) = byte;
                            }
                            w_idx += 1;
                        }

                        // Update write pointer
                        header.write_idx.store(w_idx, Ordering::Release);
                    }
                }
            } else {
                warn!("VirtioConsole: Invalid descriptor chain in TX queue");
                let _ = queue.add_used(head_idx, 0, self.translate());
            }
        }
        // info!("VirtioConsole: Finished processing TX Queue");
    }


    fn process_rx_queue(&self, state: &mut VirtioConsoleState) {
        let queue = &mut state.queues[0]; // RX Queue

        // Check connection info
        let conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_ref() {
            for (&vm_id, &buffer_addr) in conn.recv_vm_ids.iter().zip(conn.recv_buffers.iter()) {
                // [Modification] Read index from header
                let header = unsafe { &*(buffer_addr as *const RingBufferHeader) };
                let data_ptr = (buffer_addr + core::mem::size_of::<RingBufferHeader>()) as *const u8;
                let capacity = conn.buffer_size - core::mem::size_of::<RingBufferHeader>();

                // Get current index
                let w_idx = header.write_idx.load(Ordering::Acquire);
                let mut r_idx = header.read_idx.load(Ordering::Acquire);

                // Calculate available data
                let available = (w_idx - r_idx) as usize;
                if available == 0 {
                    continue;
                }

                // info!("Processing RX data from VM[{}] at buffer {:#x}, available: {}", vm_id, buffer_addr, available);

                if let Some(head_idx) = queue.pop_available(self.translate()) {
                    let desc = queue.get_descriptor(head_idx, self.translate()).unwrap();
                    let gpa = desc.addr as usize;
                    let (hpa, _) = (self.translate_fn)(GuestPhysAddr::from(gpa)).unwrap();
                    let hva = hpa.as_usize() as *mut u8;
                    
                    // Calculate how much data can be copied this time
                    let copy_len = desc.len.min(available as u32) as usize;
                    
                    // Read data from ring buffer
                    for i in 0..copy_len {
                        unsafe {
                            *hva.add(i) = *data_ptr.add((r_idx as usize) % capacity);
                        }
                        r_idx += 1;
                    }

                    // Update read pointer
                    header.read_idx.store(r_idx, Ordering::Release);

                    let _ = queue.add_used(head_idx, copy_len as u32, self.translate());
                    // state.interrupt_status |= VIRTIO_MMIO_INT_VRING;
                    // info!("Copied {} bytes from recv buffer of VM[{}]", copy_len, vm_id);
                }
            }
        } else {
            // info!("No DeviceConsoleConnection configured for RX processing");
        }

    }

    fn notify_queue(&self, queue_idx: u32) {
        let mut state = self.state.lock();
        
        // In the case where cross-core interrupt injection is not supported, use each Guest Notify (usually TX)
        // to also check the RX queue.
        // In this way, as long as the Guest keeps sending heartbeat (Ping), the Hypervisor will continuously
        // synchronize the shared buffer data to the Guest's RX queue, simulating the effect of "receiving data".
        self.process_rx_queue(&mut state);

        match queue_idx {
            1 => self.process_tx_queue(&mut state),
            0 => {},
            _ => warn!("VirtioConsole: notify on unsupported queue {}", queue_idx),
        }
    }

}

impl BaseDeviceOps<GuestPhysAddrRange> for VirtioConsole {
    fn emu_type(&self) -> EmulatedDeviceType {
        EmulatedDeviceType::VirtioConsole
    }

    fn address_range(&self) -> GuestPhysAddrRange {
        let start = self.base_gpa;
        let end = GuestPhysAddr::from(start.as_usize() + self.length);
        GuestPhysAddrRange::new(start, end)
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        // info!("VirtioConsole: handle_read at addr {:#x}, width {:?}", addr.as_usize(), width);
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        let state = self.state.lock();

        let val = match offset {
            VIRTIO_MMIO_MAGIC_VALUE => VIRTIO_MAGIC,
            VIRTIO_MMIO_VERSION => 2, 
            VIRTIO_MMIO_DEVICE_ID => VIRTIO_DEVICE_ID_CONSOLE,
            VIRTIO_MMIO_VENDOR_ID => 0x554d4551, 
            
            VIRTIO_MMIO_DEVICE_FEATURES => {
                if state.device_features_sel == 0 {
                    state.host_features as u32
                } else {
                    (state.host_features >> 32) as u32
                }
            }
            
            VIRTIO_MMIO_QUEUE_NUM_MAX => QUEUE_MAX_SIZE as u32,
            
            VIRTIO_MMIO_QUEUE_READY => {
                state.current_queue().map(|q| q.is_ready() as u32).unwrap_or(0)
            }
            
            VIRTIO_MMIO_QUEUE_NUM => {
                state.current_queue().map(|q| q.size() as u32).unwrap_or(0)
            }
            
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                state.current_queue().map(|q| q.desc_table_addr().as_usize() as u32).unwrap_or(0)
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                state.current_queue().map(|q| (q.desc_table_addr().as_usize() >> 32) as u32).unwrap_or(0)
            }
            
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                state.current_queue().map(|q| q.avail_ring_addr().as_usize() as u32).unwrap_or(0)
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                state.current_queue().map(|q| (q.avail_ring_addr().as_usize() >> 32) as u32).unwrap_or(0)
            }
            
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                state.current_queue().map(|q| q.used_ring_addr().as_usize() as u32).unwrap_or(0)
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                state.current_queue().map(|q| (q.used_ring_addr().as_usize() >> 32) as u32).unwrap_or(0)
            }
            
            VIRTIO_MMIO_INTERRUPT_STATUS => state.interrupt_status,
            VIRTIO_MMIO_STATUS => state.status,
            VIRTIO_MMIO_CONFIG_GENERATION => 0,
            
            off if off >= VIRTIO_MMIO_CONFIG && off < VIRTIO_MMIO_CONFIG + core::mem::size_of::<ConsoleConfig>() => {
                let config_offset = off - VIRTIO_MMIO_CONFIG;
                let config_bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&state.config as *const ConsoleConfig) as *const u8,
                        core::mem::size_of::<ConsoleConfig>(),
                    )
                };
                let val = match width {
                    AccessWidth::Byte => config_bytes[config_offset] as u32,
                    AccessWidth::Word => {
                        let mut buf = [0u8; 2];
                        buf.copy_from_slice(&config_bytes[config_offset..config_offset+2]);
                        u16::from_le_bytes(buf) as u32
                    }
                    AccessWidth::Dword => {
                        let mut buf = [0u8; 4];
                        buf.copy_from_slice(&config_bytes[config_offset..config_offset+4]);
                        u32::from_le_bytes(buf)
                    }
                    _ => 0,
                };
                val
            }

            _ => {
                warn!("VirtioConsole: Unhandled read at offset {:#x}", offset);
                0
            }
        };
        Ok(val as usize)
    }

    fn handle_write(&self, addr: GuestPhysAddr, _width: AccessWidth, val: usize) -> AxResult {
        // info!("VirtioConsole: handle_write at addr {:#x}, width {:?}, val {:#x}",
        //     addr.as_usize(), width, val);
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        let val = val as u32;

        // Special handling for Notify to avoid long-term locking
        if offset == VIRTIO_MMIO_QUEUE_NOTIFY {
            // info!("VirtioConsole: QUEUE_NOTIFY for queue {}", val);
            self.notify_queue(val);
            return Ok(());
        }

        let mut state = self.state.lock();

        match offset {
            VIRTIO_MMIO_DEVICE_FEATURES_SEL => state.device_features_sel = val,
            VIRTIO_MMIO_DRIVER_FEATURES => {
                if state.driver_features_sel == 0 {
                    state.guest_features = (state.guest_features & !0xFFFF_FFFF) | (val as u64);
                } else {
                    state.guest_features = (state.guest_features & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_DRIVER_FEATURES_SEL => state.driver_features_sel = val,
            
            VIRTIO_MMIO_QUEUE_SEL => {
                state.queue_sel = val;
            }
            
            VIRTIO_MMIO_QUEUE_NUM => {
                if let Some(queue) = state.current_queue_mut() {
                    queue.set_size(val as u16);
                }
            }
            
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.desc_table_addr().as_usize() as u64 & 0xFFFF_FFFF_0000_0000) | val as u64;
                    queue.set_desc_addr(addr);
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.desc_table_addr().as_usize() as u64 & 0xFFFF_FFFF) | ((val as u64) << 32);
                    queue.set_desc_addr(addr);
                }
            }
            
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.avail_ring_addr().as_usize() as u64 & 0xFFFF_FFFF_0000_0000) | val as u64;
                    queue.set_avail_addr(addr);
                }
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.avail_ring_addr().as_usize() as u64 & 0xFFFF_FFFF) | ((val as u64) << 32);
                    queue.set_avail_addr(addr);
                }
            }
            
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.used_ring_addr().as_usize() as u64 & 0xFFFF_FFFF_0000_0000) | val as u64;
                    queue.set_used_addr(addr);
                }
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                if let Some(queue) = state.current_queue_mut() {
                    let addr = (queue.used_ring_addr().as_usize() as u64 & 0xFFFF_FFFF) | ((val as u64) << 32);
                    queue.set_used_addr(addr);
                }
            }
            
            VIRTIO_MMIO_QUEUE_READY => {
                let queue_sel = state.queue_sel;
                let queue_ready = val == 1;
                
                if let Some(queue) = state.current_queue_mut() {
                    queue.set_ready(queue_ready);
                    // info!("VirtioConsole: Queue {} READY set to {}", queue_sel, queue_ready);
                } else {
                    error!("VirtioConsole: Queue {} READY set to {} (queue not found)", queue_sel, queue_ready);
                }
            }
            
            VIRTIO_MMIO_STATUS => {
                state.status = val;
                
                // Handle device reset: when status is set to 0
                if val == 0 {
                    state.reset();
                }
                
                // Check if FAILED bit is set
                if (val & VIRTIO_CONFIG_S_FAILED) != 0 {
                    warn!("VirtioConsole: Driver set FAILED status");
                }
            }
            
            VIRTIO_MMIO_INTERRUPT_ACK => {
                state.interrupt_status &= !val;
            }
            
            _ => {
                warn!("VirtioConsole: Unhandled write at offset {:#x}, val {:#x}", offset, val);
            }
        }

        Ok(())
    }
}