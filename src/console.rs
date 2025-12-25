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

// Import the device API from axvisor_api for address translation
use axvisor_api::device::translate_gpa;

const QUEUE_MAX_SIZE: u16 = 256;
const NUM_QUEUES: usize = 2;

/// Represents a connection between two VMs for console data transfer.
///
/// This structure manages the shared memory buffers used for inter-VM communication.
#[derive(Clone, Debug)]
pub struct DeviceConsoleConnection {
    /// Target VM IDs that can send data to this device.
    pub send_vm_ids: Vec<usize>,
    /// Source VM IDs that this device can receive data from.
    pub recv_vm_ids: Vec<usize>,
    /// Host Physical Addresses (HPA) of the send buffers.
    pub send_buffers: Vec<usize>,
    /// Host Physical Addresses (HPA) of the receive buffers.
    pub recv_buffers: Vec<usize>,
    /// Size of the shared buffer in bytes.
    pub buffer_size: usize,
}

/// Header structure for the shared ring buffer.
///
/// Located at the beginning of the shared memory region.
#[repr(C)]
struct RingBufferHeader {
    /// Write position (monotonically increasing).
    write_idx: AtomicU64,
    /// Read position (monotonically increasing).
    read_idx: AtomicU64,
}

impl DeviceConsoleConnection {
    /// Creates a new DeviceConsoleConnection instance.
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

    /// Adds a target VM for sending data.
    pub fn add_send_vm(&mut self, vm_id: usize, buffer: usize) {
        self.send_vm_ids.push(vm_id);
        self.send_buffers.push(buffer);
    }

    /// Removes a target VM for sending data.
    pub fn remove_send_vm(&mut self, vm_id: usize) {
        if let Some(idx) = self.send_vm_ids.iter().position(|&id| id == vm_id) {
            self.send_vm_ids.remove(idx);
            self.send_buffers.remove(idx);
        }
    }

    /// Adds a source VM for receiving data.
    pub fn add_recv_vm(&mut self, vm_id: usize, buffer: usize) {
        self.recv_vm_ids.push(vm_id);
        self.recv_buffers.push(buffer);
    }

    /// Removes a source VM for receiving data.
    pub fn remove_recv_vm(&mut self, vm_id: usize) {
        if let Some(idx) = self.recv_vm_ids.iter().position(|&id| id == vm_id) {
            self.recv_vm_ids.remove(idx);
            self.recv_buffers.remove(idx);
        }
    }
}

/// Virtio Console Configuration Space.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

/// Internal state of the Virtio Console device.
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
    /// Initializes the internal state with default values.
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

    /// Returns a mutable reference to the currently selected queue.
    fn current_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Returns a reference to the currently selected queue.
    fn current_queue(&self) -> Option<&VirtQueue> {
        self.queues.get(self.queue_sel as usize)
    }

    /// Resets the device state to initial values.
    fn reset(&mut self) {
        self.status = 0;
        self.interrupt_status = 0;
        self.guest_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        
        for queue in &mut self.queues {
            queue.set_ready(false);
            queue.set_size(0);
            queue.set_desc_addr(0);
            queue.set_avail_addr(0);
            queue.set_used_addr(0);
            queue.set_last_avail_idx(0);
        }
    }
}

/// Virtio Serial (Console) Device Implementation.
pub struct VirtioConsole {
    base_gpa: GuestPhysAddr,
    length: usize,
    _irq_id: usize,
    state: Mutex<VirtioConsoleState>,
    connection: Mutex<Option<DeviceConsoleConnection>>,
}

impl VirtioConsole {
    /// Creates a new VirtioConsole device instance.
    ///
    /// # Arguments
    /// * `base_gpa` - The base Guest Physical Address of the device.
    /// * `length` - The length of the MMIO region.
    /// * `_irq_id` - The interrupt request ID assigned to this device.
    pub fn new(
        base_gpa: usize,
        length: usize,
        _irq_id: usize,
    ) -> Self {
        Self {
            base_gpa: GuestPhysAddr::from(base_gpa),
            length,
            _irq_id,
            state: Mutex::new(VirtioConsoleState::new()),
            connection: Mutex::new(None),
        }
    }

    /// Helper function to translate GPA to HPA using the axvisor_api.
    ///
    /// Returns a tuple containing the Host Physical Address and the length of the
    /// contiguous physical memory region available from that address.
    fn translate_gpa(&self, gpa: GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        if let Some((hpa_val, len)) = translate_gpa(gpa.as_usize()) {
            // trace!("VirtioConsole: GPA {:#x} -> HPA {:#x} (len: {:#x})", gpa, hpa_val, len);
            return Some((PhysAddr::from(hpa_val), len));
        }
        warn!("VirtioConsole: Failed to translate GPA {:#x}", gpa);
        None
    }

    /// Adds a connection for sending data to a target VM.
    pub fn add_send_connection(&self, vm_id: usize, buffer: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.add_send_vm(vm_id, buffer);
            info!("Updated DeviceConsoleConnection: Added send VM {}", vm_id);
        } else {
            info!("Creating new DeviceConsoleConnection for send VM {}", vm_id);
            let new_conn = DeviceConsoleConnection::new(
                vec![vm_id], Vec::new(), vec![buffer], Vec::new(), 4096
            );
            *conn_guard = Some(new_conn);
        }
    }

    /// Removes a connection for sending data to a target VM.
    pub fn remove_send_connection(&self, vm_id: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.remove_send_vm(vm_id);
            info!("Updated DeviceConsoleConnection: Removed send VM {}", vm_id);
        }
    }

    /// Adds a connection for receiving data from a source VM.
    pub fn add_recv_connection(&self, vm_id: usize, buffer: usize) {
        {
            let mut conn_guard = self.connection.lock();
            if let Some(conn) = conn_guard.as_mut() {
                conn.add_recv_vm(vm_id, buffer);
                info!("Updated DeviceConsoleConnection: Added recv VM {}", vm_id);
            } else {
                info!("Creating new DeviceConsoleConnection for recv VM {}", vm_id);
                let new_conn = DeviceConsoleConnection::new(
                    Vec::new(), vec![vm_id], Vec::new(), vec![buffer], 4096
                );
                *conn_guard = Some(new_conn);
            }
        }
        // Trigger RX processing immediately upon connection to check for pending data
        let mut state = self.state.lock();
        self.process_rx_queue(&mut state);
    }

    /// Removes a connection for receiving data from a source VM.
    pub fn remove_recv_connection(&self, vm_id: usize) {
        let mut conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_mut() {
            conn.remove_recv_vm(vm_id);
            info!("Updated DeviceConsoleConnection: Removed recv VM {}", vm_id);
        }
    }

    /// Processes the Transmit (TX) queue.
    ///
    /// Reads data from the guest's TX queue and writes it to the shared ring buffers
    /// of connected VMs.
    fn process_tx_queue(&self, state: &mut VirtioConsoleState) {
        let queue = &mut state.queues[1]; // TX Queue is index 1

        // Define a closure for address translation that adapts to VirtQueue's requirement.
        // We ignore the limit here for the closure signature, but handle it inside the loop.
        let translator = |gpa| self.translate_gpa(gpa).map(|(hpa, _)| (hpa, usize::MAX));

        while let Some(head_idx) = queue.pop_available(&translator) {
            let mut curr_idx = head_idx;
            let mut received_data = Vec::new();
            let mut valid_chain = true;

            // Iterate over the descriptor chain to collect data
            loop {
                let desc = queue.get_descriptor(curr_idx, &translator).unwrap();
                if desc.addr == 0 || desc.len == 0 {
                    valid_chain = false;
                    break;
                }

                // Read-only descriptors contain data sent by the driver
                if (desc.flags & VRING_DESC_F_WRITE) == 0 && desc.len > 0 {
                    let gpa = GuestPhysAddr::from(desc.addr as usize);

                    // Translate GPA and check physical memory boundaries
                    match self.translate_gpa(gpa) {
                        Some((hpa, limit)) => {
                            let hva = hpa.as_usize() as *const u8;

                            // Safety Check: Ensure we don't read past the physical page boundary.
                            // Use the minimum of the requested length and the available contiguous memory.
                            let data_len = (desc.len as usize).min(limit);

                            if data_len < desc.len as usize {
                                warn!("VirtioConsole: TX descriptor crosses page boundary. Truncating copy (req: {}, avail: {})", desc.len, limit);
                            }

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
                // Mark descriptors as used
                let _ = queue.add_used(head_idx, received_data.len() as u32, &translator);
                state.interrupt_status |= VIRTIO_MMIO_INT_VRING;

                // Forward data to all connected send buffers
                let conn_guard = self.connection.lock();
                if let Some(conn) = conn_guard.as_ref() {
                    for (&vm_id, &buffer_addr) in conn.send_vm_ids.iter().zip(conn.send_buffers.iter()) {
                        let header = unsafe { &*(buffer_addr as *const RingBufferHeader) };
                        let data_ptr = (buffer_addr + core::mem::size_of::<RingBufferHeader>()) as *mut u8;
                        let capacity = conn.buffer_size - core::mem::size_of::<RingBufferHeader>();

                        let mut w_idx = header.write_idx.load(Ordering::Acquire);
                        let r_idx = header.read_idx.load(Ordering::Acquire);

                        for &byte in &received_data {
                            // Check if the ring buffer is full
                            if w_idx - r_idx >= capacity as u64 {
                                warn!("VirtioConsole: Ring buffer full for target VM[{}]", vm_id);
                                break; 
                            }
                            unsafe {
                                *data_ptr.add((w_idx as usize) % capacity) = byte;
                            }
                            w_idx += 1;
                        }
                        header.write_idx.store(w_idx, Ordering::Release);
                    }
                }
            } else {
                warn!("VirtioConsole: Invalid descriptor chain in TX queue");
                let _ = queue.add_used(head_idx, 0, &translator);
            }
        }
    }

    /// Processes the Receive (RX) queue.
    ///
    /// Reads data from the shared ring buffers and writes it to the guest's RX queue.
    fn process_rx_queue(&self, state: &mut VirtioConsoleState) {
        let queue = &mut state.queues[0]; // RX Queue is index 0
        let translator = |gpa| self.translate_gpa(gpa).map(|(hpa, _)| (hpa, usize::MAX));

        let conn_guard = self.connection.lock();
        if let Some(conn) = conn_guard.as_ref() {
            for (&_vm_id, &buffer_addr) in conn.recv_vm_ids.iter().zip(conn.recv_buffers.iter()) {
                let header = unsafe { &*(buffer_addr as *const RingBufferHeader) };
                let data_ptr = (buffer_addr + core::mem::size_of::<RingBufferHeader>()) as *const u8;
                let capacity = conn.buffer_size - core::mem::size_of::<RingBufferHeader>();

                // While there is data in the ring buffer
                while (header.write_idx.load(Ordering::Acquire) - header.read_idx.load(Ordering::Acquire)) > 0 {
                    if let Some(head_idx) = queue.pop_available(&translator) {
                        let mut r_idx = header.read_idx.load(Ordering::Acquire);
                        let w_idx = header.write_idx.load(Ordering::Acquire);
                        let available = (w_idx - r_idx) as usize;

                        let desc = queue.get_descriptor(head_idx, &translator).unwrap();
                        let gpa = desc.addr as usize;

                        // Translate GPA to HPA/HVA to write data
                        if let Some((hpa, limit)) = self.translate_gpa(GuestPhysAddr::from(gpa)) {
                            let hva = hpa.as_usize() as *mut u8;

                            // Calculate safe copy length:
                            // 1. Available data in ring buffer
                            // 2. Descriptor buffer size
                            // 3. Contiguous physical memory limit
                            let copy_len = desc.len.min(available as u32).min(limit as u32) as usize;

                            for i in 0..copy_len {
                                unsafe {
                                    *hva.add(i) = *data_ptr.add((r_idx as usize) % capacity);
                                }
                                r_idx += 1;
                            }

                            header.read_idx.store(r_idx, Ordering::Release);
                            let _ = queue.add_used(head_idx, copy_len as u32, &translator);

                            // Note: Interrupt injection is typically handled by the caller or main loop
                        } else {
                            // Translation failed, consume nothing but mark used (error handling)
                            warn!("VirtioConsole: Failed to translate RX buffer GPA {:#x}", gpa);
                            let _ = queue.add_used(head_idx, 0, &translator);
                        }
                    } else {
                        // No available descriptors in RX queue
                        break;
                    }
                }
            }
        }
    }

    /// Handles queue notifications (Doorbell).
    ///
    /// This is called when the guest writes to the QueueNotify register.
    fn notify_queue(&self, queue_idx: u32) {
        let mut state = self.state.lock();
        
        // Always check RX queue to simulate data reception (heartbeat mechanism).
        // This ensures that even if the guest only notifies TX, we still process incoming data.
        self.process_rx_queue(&mut state);

        match queue_idx {
            1 => self.process_tx_queue(&mut state),
            0 => {}, // RX queue notification is usually handled by process_rx_queue
            _ => warn!("VirtioConsole: Notify on unsupported queue index {}", queue_idx),
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

    /// Handles MMIO read operations.
    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        let mut state = self.state.lock();

        let val = match offset {
            // 1. Constant Registers
            VIRTIO_MMIO_MAGIC_VALUE => VIRTIO_MAGIC,
            VIRTIO_MMIO_VERSION => 2,
            VIRTIO_MMIO_DEVICE_ID => VIRTIO_DEVICE_ID_CONSOLE,
            VIRTIO_MMIO_VENDOR_ID => 0x554d4551,
            VIRTIO_MMIO_CONFIG_GENERATION => 0,
            
            // 2. Feature Bits
            VIRTIO_MMIO_DEVICE_FEATURES => {
                if state.device_features_sel == 0 { state.host_features as u32 }
                else { (state.host_features >> 32) as u32 }
            }
            
            // 3. Queue Configuration (Using map_or for concise Option handling)
            VIRTIO_MMIO_QUEUE_NUM_MAX => QUEUE_MAX_SIZE as u32,
            VIRTIO_MMIO_QUEUE_READY => state.current_queue().map_or(0, |q| q.is_ready() as u32),
            VIRTIO_MMIO_QUEUE_NUM => state.current_queue().map_or(0, |q| q.size() as u32),
            
            // 4. Queue Address Registers (Split into High/Low 32-bit parts)
            VIRTIO_MMIO_QUEUE_DESC_LOW => state.current_queue().map_or(0, |q| q.desc_table_addr().as_usize() as u32),
            VIRTIO_MMIO_QUEUE_DESC_HIGH => state.current_queue().map_or(0, |q| (q.desc_table_addr().as_usize() >> 32) as u32),
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => state.current_queue().map_or(0, |q| q.avail_ring_addr().as_usize() as u32),
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => state.current_queue().map_or(0, |q| (q.avail_ring_addr().as_usize() >> 32) as u32),
            VIRTIO_MMIO_QUEUE_USED_LOW => state.current_queue().map_or(0, |q| q.used_ring_addr().as_usize() as u32),
            VIRTIO_MMIO_QUEUE_USED_HIGH => state.current_queue().map_or(0, |q| (q.used_ring_addr().as_usize() >> 32) as u32),
            
            // 5. Status Registers
            VIRTIO_MMIO_INTERRUPT_STATUS => state.interrupt_status,
            VIRTIO_MMIO_STATUS => state.status,
            
            // 6. Configuration Space
            off if off >= VIRTIO_MMIO_CONFIG && off < VIRTIO_MMIO_CONFIG + core::mem::size_of::<ConsoleConfig>() => {
                let config_ptr = &state.config as *const ConsoleConfig as *const u8;
                let idx = off - VIRTIO_MMIO_CONFIG;
                unsafe {
                    match width {
                        AccessWidth::Byte => *config_ptr.add(idx) as u32,
                        AccessWidth::Word => *config_ptr.add(idx).cast::<u16>() as u32,
                        AccessWidth::Dword => *config_ptr.add(idx).cast::<u32>(),
                        _ => 0,
                    }
                }
            }

            _ => {
                warn!("VirtioConsole: Unhandled read at offset {:#x}", offset);
                0
            }
        };
        Ok(val as usize)
    }

    /// Handles MMIO write operations.
    fn handle_write(&self, addr: GuestPhysAddr, _width: AccessWidth, val: usize) -> AxResult {
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        let val = val as u32;

        // Handle Queue Notify outside of the lock to avoid potential deadlocks or long waits
        if offset == VIRTIO_MMIO_QUEUE_NOTIFY {
            self.notify_queue(val);
            return Ok(());
        }

        let mut state = self.state.lock();

        // Macro to simplify setting 64-bit addresses for queues (High/Low registers)
        macro_rules! update_queue_addr {
            ($getter:ident, $setter:ident, $high:expr) => {
                if let Some(queue) = state.current_queue_mut() {
                    let old = queue.$getter().as_usize() as u64;
                    let new = if $high {
                        (old & 0xFFFF_FFFF) | ((val as u64) << 32)
                    } else {
                        (old & !0xFFFF_FFFF) | (val as u64)
                    };
                    queue.$setter(new);
                }
            };
        }

        match offset {
            VIRTIO_MMIO_DEVICE_FEATURES_SEL => state.device_features_sel = val,
            VIRTIO_MMIO_DRIVER_FEATURES => {
                let mask = if state.driver_features_sel == 0 { 0 } else { 32 };
                state.guest_features = (state.guest_features & !(0xFFFFFFFF << mask)) | ((val as u64) << mask);
            }
            VIRTIO_MMIO_DRIVER_FEATURES_SEL => state.driver_features_sel = val,
            
            VIRTIO_MMIO_QUEUE_SEL => state.queue_sel = val,
            VIRTIO_MMIO_QUEUE_NUM => if let Some(q) = state.current_queue_mut() { q.set_size(val as u16); },
            
            // Use macro to handle address updates
            VIRTIO_MMIO_QUEUE_DESC_LOW => update_queue_addr!(desc_table_addr, set_desc_addr, false),
            VIRTIO_MMIO_QUEUE_DESC_HIGH => update_queue_addr!(desc_table_addr, set_desc_addr, true),
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => update_queue_addr!(avail_ring_addr, set_avail_addr, false),
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => update_queue_addr!(avail_ring_addr, set_avail_addr, true),
            VIRTIO_MMIO_QUEUE_USED_LOW => update_queue_addr!(used_ring_addr, set_used_addr, false),
            VIRTIO_MMIO_QUEUE_USED_HIGH => update_queue_addr!(used_ring_addr, set_used_addr, true),
            
            VIRTIO_MMIO_QUEUE_READY => {
                let ready = val == 1;
                if let Some(q) = state.current_queue_mut() { q.set_ready(ready); }
                else { error!("VirtioConsole: Queue {} READY set to {} (queue not found)", state.queue_sel, ready); }
            }
            
            VIRTIO_MMIO_STATUS => {
                state.status = val;
                if val == 0 { state.reset(); }
                if (val & VIRTIO_CONFIG_S_FAILED) != 0 { warn!("VirtioConsole: Driver set FAILED status"); }
            }
            
            VIRTIO_MMIO_INTERRUPT_ACK => state.interrupt_status &= !val,
            
            _ => warn!("VirtioConsole: Unhandled write at offset {:#x}, val {:#x}", offset, val),
        }

        Ok(())
    }
}