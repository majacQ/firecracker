// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
mod mock_devices;
mod mock_resources;
mod mock_seccomp;
mod test_utils;

use std::io;
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use polly::event_manager::EventManager;
use seccomp::{BpfProgram, SeccompLevel};
use snapshot::Snapshot;
use utils::tempfile::TempFile;
use vmm::builder::build_microvm_from_snapshot;
use vmm::builder::{build_microvm_for_boot, setup_serial_device};
use vmm::default_syscalls::get_seccomp_filter;
use vmm::persist;
use vmm::persist::{snapshot_state_sanity_check, LoadSnapshotError, MicrovmState};
use vmm::resources::VmResources;
use vmm::version_map::VERSION_MAP;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::snapshot::{CreateSnapshotParams, SnapshotType};
use vmm::Vmm;

use crate::mock_devices::MockSerialInput;
use crate::mock_resources::NOISY_KERNEL_IMAGE;
use crate::mock_resources::{MockBootSourceConfig, MockVmConfig, MockVmResources};
use crate::mock_seccomp::MockSeccomp;
use crate::test_utils::{restore_stdin, set_panic_hook};

fn create_vmm(_kernel_image: Option<&str>, is_diff: bool) -> (Arc<Mutex<Vmm>>, EventManager) {
    let mut event_manager = EventManager::new().unwrap();
    let empty_seccomp_filter = get_seccomp_filter(SeccompLevel::None).unwrap();

    let boot_source_cfg = MockBootSourceConfig::new().with_default_boot_args();
    #[cfg(target_arch = "aarch64")]
    let boot_source_cfg: BootSourceConfig = boot_source_cfg.into();
    #[cfg(target_arch = "x86_64")]
    let boot_source_cfg: BootSourceConfig = match _kernel_image {
        Some(kernel) => boot_source_cfg.with_kernel(kernel).into(),
        None => boot_source_cfg.into(),
    };
    let mock_vm_res = MockVmResources::new().with_boot_source(boot_source_cfg);
    let resources: VmResources = if is_diff {
        mock_vm_res
            .with_vm_config(MockVmConfig::new().with_dirty_page_tracking().into())
            .into()
    } else {
        mock_vm_res.into()
    };

    (
        build_microvm_for_boot(&resources, &mut event_manager, &empty_seccomp_filter).unwrap(),
        event_manager,
    )
}

fn default_vmm(kernel_image: Option<&str>) -> (Arc<Mutex<Vmm>>, EventManager) {
    create_vmm(kernel_image, false)
}

#[cfg(target_arch = "x86_64")]
fn dirty_tracking_vmm(kernel_image: Option<&str>) -> (Arc<Mutex<Vmm>>, EventManager) {
    create_vmm(kernel_image, true)
}

fn wait_vmm_child_process(vmm_pid: i32) {
    // Parent process: wait for the vmm to exit.
    let mut vmm_status: i32 = -1;
    let pid_done = unsafe { libc::waitpid(vmm_pid, &mut vmm_status, 0) };
    assert_eq!(pid_done, vmm_pid);
    restore_stdin();
    // If any panics occurred, its exit status will be != 0.
    assert!(libc::WIFEXITED(vmm_status));
    assert_eq!(libc::WEXITSTATUS(vmm_status), 0);
}

#[test]
fn test_setup_serial_device() {
    let read_tempfile = TempFile::new().unwrap();
    let read_handle = MockSerialInput(read_tempfile.into_file());
    let mut event_manager = EventManager::new().unwrap();

    assert!(setup_serial_device(
        &mut event_manager,
        Box::new(read_handle),
        Box::new(io::stdout()),
    )
    .is_ok());
}

#[test]
fn test_build_microvm() {
    // Error case: no boot source configured.
    {
        let resources: VmResources = MockVmResources::new().into();
        let mut event_manager = EventManager::new().unwrap();
        let empty_seccomp_filter = get_seccomp_filter(SeccompLevel::None).unwrap();

        let vmm_ret = build_microvm_for_boot(&resources, &mut event_manager, &empty_seccomp_filter);
        assert_eq!(format!("{:?}", vmm_ret.err()), "Some(MissingKernelConfig)");
    }

    // Success case.
    // Child process will run the vmm and exit.
    // Parent will wait for child to exit and assert on exit status 0.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child process: build and run vmm.
            // If the vmm thread panics, the `wait()` in the parent doesn't exit.
            // Force the child to exit on panic to unblock the waiting parent.
            set_panic_hook();

            let (vmm, mut event_manager) = default_vmm(None);

            // On x86_64, the vmm should exit once its workload completes and signals the exit event.
            // On aarch64, the test kernel doesn't exit, so the vmm is force-stopped.
            let _ = event_manager.run_with_timeout(500).unwrap();

            #[cfg(target_arch = "x86_64")]
            vmm.lock().unwrap().stop(-1); // If we got here, something went wrong.
            #[cfg(target_arch = "aarch64")]
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
fn test_vmm_seccomp() {
    // Tests the behavior of a customized seccomp filter on the VMM.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child process: build vmm and (try to) run it.
            let boot_source_cfg: BootSourceConfig =
                MockBootSourceConfig::new().with_default_boot_args().into();
            let resources: VmResources = MockVmResources::new()
                .with_boot_source(boot_source_cfg)
                .into();
            let mut event_manager = EventManager::new().unwrap();

            // The customer "forgot" to whitelist the KVM_RUN ioctl.
            let filter: BpfProgram = MockSeccomp::new().without_kvm_run().into();
            let vmm = build_microvm_for_boot(&resources, &mut event_manager, &filter).unwrap();
            // Give the vCPUs a chance to attempt KVM_RUN.
            thread::sleep(Duration::from_millis(200));
            // Should never get here.
            vmm.lock().unwrap().stop(-1);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            let mut vmm_status: i32 = -1;
            let pid_done = unsafe { libc::waitpid(vmm_pid, &mut vmm_status, 0) };
            assert_eq!(pid_done, vmm_pid);
            restore_stdin();
            // The seccomp fault should have caused death by SIGSYS.
            assert!(libc::WIFSIGNALED(vmm_status));
            assert_eq!(libc::WTERMSIG(vmm_status), libc::SIGSYS);
        }
    }
}

#[test]
fn test_exit_vcpus() {
    // Tests that exiting vCPUs works.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();

            let (vmm, _) = default_vmm(None);

            assert!(vmm.lock().unwrap().exit_vcpus().is_ok());
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
fn test_pause_resume_microvm() {
    // Tests that pausing and resuming a microVM work as expected.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            // Child process: build and run vmm, then attempts to pause and resume it.
            set_panic_hook();

            let (vmm, mut event_manager) = default_vmm(None);

            // There's a race between this thread and the vcpu thread, but this thread
            // should be able to pause vcpu thread before it finishes running its test-binary.
            assert!(vmm.lock().unwrap().pause_vm().is_ok());
            // Pausing again the microVM should not fail (microVM remains in the
            // `Paused` state).
            assert!(vmm.lock().unwrap().pause_vm().is_ok());
            assert!(vmm.lock().unwrap().resume_vm().is_ok());

            let _ = event_manager.run_with_timeout(500).unwrap();

            #[cfg(target_arch = "x86_64")]
            vmm.lock().unwrap().stop(-1); // If we got here, something went wrong.
            #[cfg(target_arch = "aarch64")]
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
fn test_dirty_bitmap_error() {
    // Error case: dirty tracking disabled.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();

            let (vmm, mut event_manager) = default_vmm(None);

            // The vmm will start with dirty page tracking = OFF.
            // With dirty tracking disabled, the underlying KVM_GET_DIRTY_LOG ioctl will fail
            // with errno 2 (ENOENT) because KVM can't find any guest memory regions with dirty
            // page tracking enabled.
            assert_eq!(
                format!("{:?}", vmm.lock().unwrap().get_dirty_bitmap().err()),
                "Some(DirtyBitmap(Error(2)))"
            );

            let _ = event_manager.run_with_timeout(500).unwrap();

            #[cfg(target_arch = "x86_64")]
            vmm.lock().unwrap().stop(-1); // If we got here, something went wrong.
            #[cfg(target_arch = "aarch64")]
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn test_dirty_bitmap_success() {
    // This test is `x86_64`-only until we come up with an `aarch64` kernel that dirties a lot
    // of pages.
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();

            // The vmm will start with dirty page tracking = ON.
            let (vmm, _) = dirty_tracking_vmm(Some(NOISY_KERNEL_IMAGE));

            // Let it churn for a while and dirty some pages...
            thread::sleep(Duration::from_millis(100));
            let bitmap = vmm.lock().unwrap().get_dirty_bitmap().unwrap();
            let num_dirty_pages: u32 = bitmap
                .iter()
                .map(|(_, bitmap_per_region)| {
                    // Gently coerce to u32
                    let num_dirty_pages_per_region: u32 =
                        bitmap_per_region.iter().map(|n| n.count_ones()).sum();
                    num_dirty_pages_per_region
                })
                .sum();
            assert!(num_dirty_pages > 0);
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
fn test_disallow_snapshots_without_pausing() {
    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();
            let (vmm, _) = default_vmm(Some(NOISY_KERNEL_IMAGE));

            // Verify saving state while running is not allowed.
            // Can't do unwrap_err() because MicrovmState doesn't impl Debug.
            match vmm.lock().unwrap().save_state() {
                Err(e) => assert!(format!("{:?}", e).contains("NotAllowed")),
                Ok(_) => panic!("Should not be allowed."),
            };

            // Pause microVM.
            vmm.lock().unwrap().pause_vm().unwrap();
            // It is now allowed.
            vmm.lock().unwrap().save_state().unwrap();
            // Stop.
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

fn verify_create_snapshot(is_diff: bool) -> (TempFile, TempFile) {
    let snapshot_file = TempFile::new().unwrap();
    let memory_file = TempFile::new().unwrap();

    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();

            let (vmm, _) = create_vmm(Some(NOISY_KERNEL_IMAGE), is_diff);

            // Be sure that the microVM is running.
            thread::sleep(Duration::from_millis(200));

            // Pause microVM.
            vmm.lock().unwrap().pause_vm().unwrap();

            // Create snapshot.
            let snapshot_type = match is_diff {
                true => SnapshotType::Diff,
                false => SnapshotType::Full,
            };
            let snapshot_params = CreateSnapshotParams {
                snapshot_type,
                snapshot_path: snapshot_file.as_path().to_path_buf(),
                mem_file_path: memory_file.as_path().to_path_buf(),
                version: Some(String::from("0.24.0")),
            };

            {
                let mut locked_vmm = vmm.lock().unwrap();
                persist::create_snapshot(&mut locked_vmm, &snapshot_params, VERSION_MAP.clone())
                    .unwrap();
            }

            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);

            // Check that we can deserialize the microVM state from `snapshot_file`.
            let snapshot_path = snapshot_file.as_path().to_path_buf();
            let snapshot_file_metadata = std::fs::metadata(snapshot_path).unwrap();
            let snapshot_len = snapshot_file_metadata.len() as usize;
            let restored_microvm_state: MicrovmState = Snapshot::load(
                &mut snapshot_file.as_file(),
                snapshot_len,
                VERSION_MAP.clone(),
            )
            .unwrap();

            // Check memory file size.
            let memory_file_size_mib = memory_file.as_file().metadata().unwrap().len() >> 20;
            assert_eq!(
                restored_microvm_state.vm_info.mem_size_mib,
                memory_file_size_mib
            );

            // Verify deserialized data.
            // The default vmm has no devices and one vCPU.
            assert_eq!(restored_microvm_state.device_states.block_devices.len(), 0);
            assert_eq!(restored_microvm_state.device_states.net_devices.len(), 0);
            assert!(restored_microvm_state.device_states.vsock_device.is_none());
            assert_eq!(restored_microvm_state.vcpu_states.len(), 1);
        }
    }
    (snapshot_file, memory_file)
}

fn verify_load_snapshot(snapshot_file: TempFile, memory_file: TempFile) {
    use vm_memory::GuestMemoryMmap;
    use vmm::memory_snapshot::SnapshotMemory;

    let pid = unsafe { libc::fork() };
    match pid {
        0 => {
            set_panic_hook();
            let mut event_manager = EventManager::new().unwrap();
            let empty_seccomp_filter = get_seccomp_filter(SeccompLevel::None).unwrap();

            // Deserialize microVM state.
            let snapshot_file_metadata = snapshot_file.as_file().metadata().unwrap();
            let snapshot_len = snapshot_file_metadata.len() as usize;
            snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
            let microvm_state: MicrovmState = Snapshot::load(
                &mut snapshot_file.as_file(),
                snapshot_len,
                VERSION_MAP.clone(),
            )
            .unwrap();
            let mem =
                GuestMemoryMmap::restore(memory_file.as_file(), &microvm_state.memory_state, false)
                    .unwrap();

            // Build microVM from state.
            let vmm = build_microvm_from_snapshot(
                &mut event_manager,
                microvm_state,
                mem,
                false,
                &empty_seccomp_filter,
            )
            .unwrap();
            // For now we're happy we got this far, we don't test what the guest is actually doing.
            vmm.lock().unwrap().stop(0);
        }
        vmm_pid => {
            // Parent process: wait for the vmm to exit.
            wait_vmm_child_process(vmm_pid);
        }
    }
}

#[test]
fn test_create_and_load_snapshot() {
    // Create diff snapshot.
    let (snapshot_file, memory_file) = verify_create_snapshot(true);
    // Create a new microVm from snapshot. This only tests code-level logic; it verifies
    // that a microVM can be built with no errors from given snapshot.
    // It does _not_ verify that the guest is actually restored properly. We're using
    // python integration tests for that.
    verify_load_snapshot(snapshot_file, memory_file);

    // Create full snapshot.
    let (snapshot_file, memory_file) = verify_create_snapshot(false);
    // Create a new microVm from snapshot. This only tests code-level logic; it verifies
    // that a microVM can be built with no errors from given snapshot.
    // It does _not_ verify that the guest is actually restored properly. We're using
    // python integration tests for that.
    verify_load_snapshot(snapshot_file, memory_file);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn test_snapshot_cpu_vendor() {
    use vmm::persist::validate_x86_64_cpu_vendor;
    // Create a diff snapshot.
    let (snapshot_file, _) = verify_create_snapshot(true);

    // Deserialize the microVM state.
    let snapshot_file_metadata = snapshot_file.as_file().metadata().unwrap();
    let snapshot_len = snapshot_file_metadata.len() as usize;
    snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
    let microvm_state: MicrovmState = Snapshot::load(
        &mut snapshot_file.as_file(),
        snapshot_len,
        VERSION_MAP.clone(),
    )
    .unwrap();

    // Check if the snapshot created above passes validation since
    // the snapshot was created locally.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_ok());
}

#[test]
fn test_snapshot_load_sanity_checks() {
    use vmm::vmm_config::machine_config::MAX_SUPPORTED_VCPUS;

    // Create a diff snapshot.
    let (snapshot_file, _) = verify_create_snapshot(true);

    // Deserialize the microVM state.
    let snapshot_file_metadata = snapshot_file.as_file().metadata().unwrap();
    let snapshot_len = snapshot_file_metadata.len() as usize;
    snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
    let mut microvm_state: MicrovmState = Snapshot::load(
        &mut snapshot_file.as_file(),
        snapshot_len,
        VERSION_MAP.clone(),
    )
    .unwrap();

    assert!(snapshot_state_sanity_check(&microvm_state).is_ok());

    // Remove memory regions.
    microvm_state.memory_state.regions.clear();

    // Validate sanity checks fail because there is no mem region in state.
    let err = snapshot_state_sanity_check(&microvm_state).unwrap_err();
    match err {
        LoadSnapshotError::InvalidSnapshot(err_msg) => {
            assert_eq!(err_msg, "No memory region defined.")
        }
        _ => unreachable!(),
    }

    // Create MAX_SUPPORTED_VCPUS vCPUs starting from 1 vCPU.
    for _ in 0..(MAX_SUPPORTED_VCPUS as f64).log2() as usize {
        microvm_state
            .vcpu_states
            .append(&mut microvm_state.vcpu_states.clone());
    }

    // After this line we will have 33 vCPUs, FC max si 32.
    microvm_state
        .vcpu_states
        .push(microvm_state.vcpu_states[0].clone());

    // Validate sanity checks fail because there are too many vCPUs.
    let err = snapshot_state_sanity_check(&microvm_state).unwrap_err();
    match err {
        LoadSnapshotError::InvalidSnapshot(err_msg) => assert_eq!(err_msg, "Invalid vCPU count."),
        _ => unreachable!(),
    }

    // Remove all vCPUs states from microvm state.
    microvm_state.vcpu_states.clear();

    // Validate sanity checks fail because there is no vCPU in state.
    let err = snapshot_state_sanity_check(&microvm_state).unwrap_err();
    match err {
        LoadSnapshotError::InvalidSnapshot(err_msg) => assert_eq!(err_msg, "Invalid vCPU count."),
        _ => unreachable!(),
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn test_snapshot_cpu_vendor_mismatch() {
    use vmm::persist::validate_x86_64_cpu_vendor;
    // Create a diff snapshot.
    let (snapshot_file, _) = verify_create_snapshot(true);

    // Deserialize the microVM state.
    let snapshot_file_metadata = snapshot_file.as_file().metadata().unwrap();
    let snapshot_len = snapshot_file_metadata.len() as usize;
    snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
    let mut microvm_state: MicrovmState = Snapshot::load(
        &mut snapshot_file.as_file(),
        snapshot_len,
        VERSION_MAP.clone(),
    )
    .unwrap();

    // Check if the snapshot created above passes validation since
    // the snapshot was created locally.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_ok());

    // Modify the vendor id in CPUID.
    for entry in microvm_state.vcpu_states[0].cpuid.as_mut_slice().iter_mut() {
        if entry.function == 0 && entry.index == 0 {
            // Fail if vendor id is NULL as this needs furhter investigation.
            assert_ne!(entry.ebx, 0);
            assert_ne!(entry.ecx, 0);
            assert_ne!(entry.edx, 0);
            entry.ebx = 0;
            break;
        }
    }

    // This must fail as the cpu vendor has been mangled.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_err());

    // Negative test: remove the vendor id from cpuid.
    for entry in microvm_state.vcpu_states[0].cpuid.as_mut_slice().iter_mut() {
        if entry.function == 0 && entry.index == 0 {
            entry.function = 1234;
        }
    }

    // This must fail as the cpu vendor has been mangled.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_err());
}

#[cfg(target_arch = "x86_64")]
#[test]
fn test_snapshot_cpu_vendor_missing() {
    use vmm::persist::validate_x86_64_cpu_vendor;
    // Create a diff snapshot.
    let (snapshot_file, _) = verify_create_snapshot(true);

    // Deserialize the microVM state.
    let snapshot_file_metadata = snapshot_file.as_file().metadata().unwrap();
    let snapshot_len = snapshot_file_metadata.len() as usize;
    snapshot_file.as_file().seek(SeekFrom::Start(0)).unwrap();
    let mut microvm_state: MicrovmState = Snapshot::load(
        &mut snapshot_file.as_file(),
        snapshot_len,
        VERSION_MAP.clone(),
    )
    .unwrap();

    // Check if the snapshot created above passes validation since
    // the snapshot was created locally.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_ok());

    // Negative test: remove the vendor id from cpuid.
    for entry in microvm_state.vcpu_states[0].cpuid.as_mut_slice().iter_mut() {
        if entry.function == 0 && entry.index == 0 {
            entry.function = 1234;
        }
    }

    // This must fail as the cpu vendor entry does not exist.
    assert!(validate_x86_64_cpu_vendor(&microvm_state).is_err());
}
