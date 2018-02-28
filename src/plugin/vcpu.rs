// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::{Cell, RefCell};
use std::cmp::min;
use std::cmp::{self, Ord, PartialOrd, PartialEq};
use std::collections::btree_set::BTreeSet;
use std::mem::size_of;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex, RwLock};

use libc::{EINVAL, EPROTO, ENOENT, EPERM, EPIPE, EDEADLK, ENOTTY};

use protobuf;
use protobuf::Message;

use data_model::DataInit;
use kvm::{Vcpu, CpuId};
use kvm_sys::{kvm_regs, kvm_sregs, kvm_fpu, kvm_debugregs, kvm_msrs, kvm_msr_entry,
              KVM_CPUID_FLAG_SIGNIFCANT_INDEX};
use plugin_proto::*;

use super::*;

/// Identifier for an address space in the VM.
#[derive(Copy, Clone)]
pub enum IoSpace {
    Ioport,
    Mmio,
}

#[derive(Debug, Copy, Clone)]
struct Range(u64, u64);

impl Eq for Range {}

impl PartialEq for Range {
    fn eq(&self, other: &Range) -> bool {
        self.0 == other.0
    }
}

impl Ord for Range {
    fn cmp(&self, other: &Range) -> cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for Range {
    fn partial_cmp(&self, other: &Range) -> Option<cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

// Wrapper types to make the kvm register structs DataInit
#[derive(Copy, Clone)]
struct VcpuRegs(kvm_regs);
unsafe impl DataInit for VcpuRegs {}
#[derive(Copy, Clone)]
struct VcpuSregs(kvm_sregs);
unsafe impl DataInit for VcpuSregs {}
#[derive(Copy, Clone)]
struct VcpuFpu(kvm_fpu);
unsafe impl DataInit for VcpuFpu {}
#[derive(Copy, Clone)]
struct VcpuDebugregs(kvm_debugregs);
unsafe impl DataInit for VcpuDebugregs {}


fn get_vcpu_state(vcpu: &Vcpu, state_set: VcpuRequest_StateSet) -> SysResult<Vec<u8>> {
    Ok(match state_set {
           VcpuRequest_StateSet::REGS => VcpuRegs(vcpu.get_regs()?).as_slice().to_vec(),
           VcpuRequest_StateSet::SREGS => VcpuSregs(vcpu.get_sregs()?).as_slice().to_vec(),
           VcpuRequest_StateSet::FPU => VcpuFpu(vcpu.get_fpu()?).as_slice().to_vec(),
           VcpuRequest_StateSet::DEBUGREGS => {
               VcpuDebugregs(vcpu.get_debugregs()?).as_slice().to_vec()
           }
       })
}

fn set_vcpu_state(vcpu: &Vcpu, state_set: VcpuRequest_StateSet, state: &[u8]) -> SysResult<()> {
    match state_set {
        VcpuRequest_StateSet::REGS => {
            vcpu.set_regs(&VcpuRegs::from_slice(state)
                               .ok_or(SysError::new(EINVAL))?
                               .0)
        }
        VcpuRequest_StateSet::SREGS => {
            vcpu.set_sregs(&VcpuSregs::from_slice(state)
                                .ok_or(SysError::new(EINVAL))?
                                .0)
        }
        VcpuRequest_StateSet::FPU => {
            vcpu.set_fpu(&VcpuFpu::from_slice(state)
                              .ok_or(SysError::new(EINVAL))?
                              .0)
        }
        VcpuRequest_StateSet::DEBUGREGS => {
            vcpu.set_debugregs(&VcpuDebugregs::from_slice(state)
                                    .ok_or(SysError::new(EINVAL))?
                                    .0)
        }
    }
}


/// State shared by every VCPU, grouped together to make edits to the state coherent across VCPUs.
#[derive(Default)]
pub struct SharedVcpuState {
    ioport_regions: BTreeSet<Range>,
    mmio_regions: BTreeSet<Range>,
}

impl SharedVcpuState {
    /// Reserves the given range for handling by the plugin process.
    ///
    /// This will reject any reservation that overlaps with an existing reservation.
    pub fn reserve_range(&mut self, space: IoSpace, start: u64, length: u64) -> SysResult<()> {
        if length == 0 {
            return Err(SysError::new(EINVAL));
        }

        // Reject all cases where this reservation is part of another reservation.
        if self.is_reserved(space, start) {
            return Err(SysError::new(EPERM));
        }

        let last_address = match start.checked_add(length) {
            Some(end) => end - 1,
            None => return Err(SysError::new(EINVAL)),
        };

        let space = match space {
            IoSpace::Ioport => &mut self.ioport_regions,
            IoSpace::Mmio => &mut self.mmio_regions,
        };

        match space
                  .range(..Range(last_address, 0))
                  .next_back()
                  .cloned() {
            Some(Range(existing_start, _)) if existing_start >= start => Err(SysError::new(EPERM)),
            _ => {
                space.insert(Range(start, length));
                Ok(())
            }
        }
    }

    //// Releases a reservation previously made at `start` in the given `space`.
    pub fn unreserve_range(&mut self, space: IoSpace, start: u64) -> SysResult<()> {
        let range = Range(start, 0);
        let space = match space {
            IoSpace::Ioport => &mut self.ioport_regions,
            IoSpace::Mmio => &mut self.mmio_regions,
        };
        if space.remove(&range) {
            Ok(())
        } else {
            Err(SysError::new(ENOENT))
        }
    }

    fn is_reserved(&self, space: IoSpace, addr: u64) -> bool {
        if let Some(Range(start, len)) = self.first_before(space, addr) {
            let offset = addr - start;
            if offset < len {
                return true;
            }
        }
        false
    }

    fn first_before(&self, io_space: IoSpace, addr: u64) -> Option<Range> {
        let space = match io_space {
            IoSpace::Ioport => &self.ioport_regions,
            IoSpace::Mmio => &self.mmio_regions,
        };

        match addr.checked_add(1) {
            Some(next_addr) => space.range(..Range(next_addr, 0)).next_back().cloned(),
            None => None,
        }
    }
}

/// State specific to a VCPU, grouped so that each `PluginVcpu` object will share a canonical
/// version.
#[derive(Default)]
pub struct PerVcpuState {
    pause_request: Option<u64>,
}

impl PerVcpuState {
    /// Indicates that a VCPU should wait until the plugin process resumes the VCPU.
    ///
    /// This method will not cause a VCPU to pause immediately. Instead, the VCPU thread will
    /// continue running until a interrupted, at which point it will check for a pending pause. If
    /// there is another call to `request_pause` for this VCPU before that happens, the last pause
    /// request's `data` will be overwritten with the most recent `data.
    ///
    /// To get an immediate pause after calling `request_pause`, send a signal (with a registered
    /// handler) to the thread handling the VCPU corresponding to this state. This should interrupt
    /// the running VCPU, which should check for a pause with `PluginVcpu::pre_run`.
    pub fn request_pause(&mut self, data: u64) {
        self.pause_request = Some(data);
    }
}

enum VcpuRunData<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl<'a> VcpuRunData<'a> {
    fn is_write(&self) -> bool {
        match self {
            &VcpuRunData::Write(_) => true,
            _ => false,
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            &VcpuRunData::Read(ref s) => s,
            &VcpuRunData::Write(ref s) => s,
        }
    }

    fn copy_from_slice(&mut self, data: &[u8]) {
        match self {
            &mut VcpuRunData::Read(ref mut s) => {
                let copy_size = min(s.len(), data.len());
                s.copy_from_slice(&data[..copy_size]);
            }
            _ => {}
        }
    }
}

/// State object for a VCPU's connection with the plugin process.
///
/// This is used by a VCPU thread to allow the plugin process to handle vmexits. Each method may
/// block indefinitely while the plugin process is handling requests. In order to cleanly shutdown
/// during these blocking calls, the `connection` socket should be shutdown. This will end the
/// blocking calls,
pub struct PluginVcpu {
    shared_vcpu_state: Arc<RwLock<SharedVcpuState>>,
    per_vcpu_state: Arc<Mutex<PerVcpuState>>,
    connection: UnixDatagram,
    wait_reason: Cell<Option<VcpuResponse_Wait>>,
    request_buffer: RefCell<Vec<u8>>,
    response_buffer: RefCell<Vec<u8>>,
}

impl PluginVcpu {
    /// Creates the plugin state and connection container for a VCPU thread.
    pub fn new(shared_vcpu_state: Arc<RwLock<SharedVcpuState>>,
               per_vcpu_state: Arc<Mutex<PerVcpuState>>,
               connection: UnixDatagram)
               -> PluginVcpu {
        PluginVcpu {
            shared_vcpu_state,
            per_vcpu_state,
            connection,
            wait_reason: Default::default(),
            request_buffer: Default::default(),
            response_buffer: Default::default(),
        }
    }

    /// Tells the plugin process to initialize this VCPU.
    ///
    /// This should be called for each VCPU before the first run of any of the VCPUs in the VM.
    pub fn init(&self, vcpu: &Vcpu) -> SysResult<()> {
        let mut wait_reason = VcpuResponse_Wait::new();
        wait_reason.mut_init();
        self.wait_reason.set(Some(wait_reason));
        self.handle_until_resume(vcpu)?;
        Ok(())
    }

    /// The VCPU thread should call this before rerunning a VM in order to handle pending requests
    /// to this VCPU.
    pub fn pre_run(&self, vcpu: &Vcpu) -> SysResult<()> {
        let request = {
            let mut lock = self.per_vcpu_state.lock().map_err(|_| SysError::new(EDEADLK))?;
            lock.pause_request.take()
        };

        if let Some(user_data) = request {
            let mut wait_reason = VcpuResponse_Wait::new();
            wait_reason.mut_user().user = user_data;
            self.wait_reason.set(Some(wait_reason));
            self.handle_until_resume(vcpu)?;
        }
        Ok(())
    }

    fn process(&self, io_space: IoSpace, addr: u64, mut data: VcpuRunData, vcpu: &Vcpu) -> bool {
        let vcpu_state_lock = match self.shared_vcpu_state.read() {
            Ok(l) => l,
            Err(e) => {
                error!("error read locking shared cpu state: {:?}", e);
                return false;
            }
        };

        let first_before_addr = vcpu_state_lock.first_before(io_space, addr);
        // Drops the read lock as soon as possible, to prevent holding lock while blocked in
        // `handle_until_resume`.
        drop(vcpu_state_lock);

        match first_before_addr {
            Some(Range(start, len)) => {
                let offset = addr - start;
                if offset >= len {
                    return false;
                }
                let mut wait_reason = VcpuResponse_Wait::new();
                {
                    let io = wait_reason.mut_io();
                    io.space = match io_space {
                        IoSpace::Ioport => AddressSpace::IOPORT,
                        IoSpace::Mmio => AddressSpace::MMIO,
                    };
                    io.address = addr;
                    io.is_write = data.is_write();
                    io.data = data.as_slice().to_vec();
                }
                self.wait_reason.set(Some(wait_reason));
                match self.handle_until_resume(vcpu) {
                    Ok(resume_data) => data.copy_from_slice(&resume_data),
                    Err(e) if e.errno() == EPIPE => {}
                    Err(e) => error!("failed to process vcpu requests: {:?}", e),
                }
                true
            }
            None => false,
        }
    }

    /// Has the plugin process handle a IO port read.
    pub fn io_read(&self, addr: u64, data: &mut [u8], vcpu: &Vcpu) -> bool {
        self.process(IoSpace::Ioport, addr, VcpuRunData::Read(data), vcpu)
    }

    /// Has the plugin process handle a IO port write.
    pub fn io_write(&self, addr: u64, data: &[u8], vcpu: &Vcpu) -> bool {
        self.process(IoSpace::Ioport, addr, VcpuRunData::Write(data), vcpu)
    }

    /// Has the plugin process handle a MMIO read.
    pub fn mmio_read(&self, addr: u64, data: &mut [u8], vcpu: &Vcpu) -> bool {
        self.process(IoSpace::Mmio, addr, VcpuRunData::Read(data), vcpu)
    }

    /// Has the plugin process handle a MMIO write.
    pub fn mmio_write(&self, addr: u64, data: &[u8], vcpu: &Vcpu) -> bool {
        self.process(IoSpace::Mmio, addr, VcpuRunData::Write(data), vcpu)
    }

    fn handle_request(&self, vcpu: &Vcpu) -> SysResult<Option<Vec<u8>>> {
        let mut resume_data = None;
        let mut request_buffer = self.request_buffer.borrow_mut();
        request_buffer.resize(MAX_VCPU_DATAGRAM_SIZE, 0);

        let msg_size = self.connection
            .recv(&mut request_buffer)
            .map_err(io_to_sys_err)?;


        let mut request = protobuf::parse_from_bytes::<VcpuRequest>(&request_buffer[..msg_size])
            .map_err(proto_to_sys_err)?;

        let wait_reason = self.wait_reason.take();

        let mut response = VcpuResponse::new();
        let res = if request.has_wait() {
            match wait_reason {
                Some(wait_reason) => {
                    response.set_wait(wait_reason);
                    Ok(())
                }
                None => Err(SysError::new(EPROTO)),
            }
        } else if wait_reason.is_some() {
            // Any request other than getting the wait_reason while there is one pending is invalid.
            self.wait_reason.set(wait_reason);
            Err(SysError::new(EPROTO))
        } else if request.has_resume() {
            response.mut_resume();
            resume_data = Some(request.take_resume().take_data());
            Ok(())
        } else if request.has_get_state() {
            let response_state = response.mut_get_state();
            match get_vcpu_state(vcpu, request.get_get_state().set) {
                Ok(state) => {
                    response_state.state = state;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else if request.has_set_state() {
            response.mut_set_state();
            let set_state = request.get_set_state();
            set_vcpu_state(vcpu, set_state.set, set_state.get_state())
        } else if request.has_get_msrs() {
            let entry_data = &mut response.mut_get_msrs().entry_data;
            let entry_indices = &request.get_get_msrs().entry_indices;
            let mut msr_entries = Vec::with_capacity(entry_indices.len());
            for &index in entry_indices {
                msr_entries.push(kvm_msr_entry {
                                     index,
                                     ..Default::default()
                                 });
            }
            match vcpu.get_msrs(&mut msr_entries) {
                Ok(()) => {
                    for msr_entry in msr_entries {
                        entry_data.push(msr_entry.data);
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else if request.has_set_msrs() {
            response.mut_set_msrs();
            let request_entries = &request.get_set_msrs().entries;
            let vec_size_bytes = size_of::<kvm_msrs>() +
                                 (request_entries.len() * size_of::<kvm_msr_entry>());
            let vec: Vec<u8> = vec![0; vec_size_bytes];
            let kvm_msrs: &mut kvm_msrs = unsafe {
                // Converting the vector's memory to a struct is unsafe.  Carefully using the read-
                // only vector to size and set the members ensures no out-of-bounds erros below.
                &mut *(vec.as_ptr() as *mut kvm_msrs)
            };
            unsafe {
                // Mapping the unsized array to a slice is unsafe becase the length isn't known.
                // Providing the length used to create the struct guarantees the entire slice is
                // valid.
                let kvm_msr_entries: &mut [kvm_msr_entry] =
                    kvm_msrs.entries.as_mut_slice(request_entries.len());
                for (msr_entry, entry) in kvm_msr_entries.iter_mut().zip(request_entries.iter()) {
                    msr_entry.index = entry.index;
                    msr_entry.data = entry.data;
                }
            }
            kvm_msrs.nmsrs = request_entries.len() as u32;
            vcpu.set_msrs(&kvm_msrs)
        } else if request.has_set_cpuid() {
            response.mut_set_cpuid();
            let request_entries = &request.get_set_cpuid().entries;
            let mut cpuid = CpuId::new(request_entries.len());
            {
                let cpuid_entries = cpuid.mut_entries_slice();
                for (request_entry, cpuid_entry) in
                    request_entries.iter().zip(cpuid_entries.iter_mut()) {
                    cpuid_entry.function = request_entry.function;
                    if request_entry.has_index {
                        cpuid_entry.index = request_entry.index;
                        cpuid_entry.flags = KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
                    }
                    cpuid_entry.eax = request_entry.eax;
                    cpuid_entry.ebx = request_entry.ebx;
                    cpuid_entry.ecx = request_entry.ecx;
                    cpuid_entry.edx = request_entry.edx;
                }
            }
            vcpu.set_cpuid2(&cpuid)
        } else {
            Err(SysError::new(ENOTTY))
        };

        if let Err(e) = res {
            response.errno = e.errno();
        }

        let mut response_buffer = self.response_buffer.borrow_mut();
        response_buffer.clear();
        response
            .write_to_vec(&mut response_buffer)
            .map_err(proto_to_sys_err)?;
        self.connection
            .send(&response_buffer[..])
            .map_err(io_to_sys_err)?;

        Ok(resume_data)
    }

    fn handle_until_resume(&self, vcpu: &Vcpu) -> SysResult<Vec<u8>> {
        loop {
            if let Some(resume_data) = self.handle_request(vcpu)? {
                return Ok(resume_data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_vcpu_reserve() {
        let mut shared_vcpu_state = SharedVcpuState::default();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x10, 0)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x10, 0x10)
            .unwrap();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x0f, 0x10)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x10, 0x10)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x10, 0x15)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x12, 0x15)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x12, 0x01)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x0, 0x20)
            .unwrap_err();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x20, 0x05)
            .unwrap();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x25, 0x05)
            .unwrap();
        shared_vcpu_state
            .reserve_range(IoSpace::Ioport, 0x0, 0x10)
            .unwrap();
    }
}
