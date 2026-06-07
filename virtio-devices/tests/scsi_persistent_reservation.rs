// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use virtio_devices::scsi::{
    DiskOps, ScsiCommandProcessor, ScsiLunConfig, VIRTIO_SCSI_S_OK, scsi_opcode, scsi_status,
};

const PR_IN_READ_KEYS: u8 = 0x00;
const PR_IN_READ_RESERVATION: u8 = 0x01;
const PR_OUT_REGISTER: u8 = 0x00;
const PR_OUT_RESERVE: u8 = 0x01;
const PR_TYPE_EXCLUSIVE_ACCESS: u8 = 0x03;

struct MemDisk(Cursor<Vec<u8>>);

impl Read for MemDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for MemDisk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Seek for MemDisk {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl DiskOps for MemDisk {}

fn pr_out_payload(reservation_key: u64, service_action_key: u64) -> Vec<u8> {
    let mut data = vec![0u8; 24];
    data[0..8].copy_from_slice(&reservation_key.to_be_bytes());
    data[8..16].copy_from_slice(&service_action_key.to_be_bytes());
    data
}

fn pr_out_cdb(service_action: u8, reservation_type: u8) -> [u8; 10] {
    [
        scsi_opcode::PERSISTENT_RESERVE_OUT,
        service_action,
        reservation_type,
        0,
        0,
        0,
        0,
        0,
        24,
        0,
    ]
}

fn pr_in_cdb(service_action: u8) -> [u8; 10] {
    [
        scsi_opcode::PERSISTENT_RESERVE_IN,
        service_action,
        0,
        0,
        0,
        0,
        0,
        0,
        64,
        0,
    ]
}

#[test]
fn test_virtio_scsi_persistent_reservation() {
    let mut processor = ScsiCommandProcessor::new(ScsiLunConfig::default(), 1024 * 1024, 512);
    let mut disk = MemDisk(Cursor::new(vec![0u8; 1024 * 1024]));

    let register = pr_out_cdb(PR_OUT_REGISTER, 0);
    let result = processor.process_command(&register, &pr_out_payload(0, 0x1234), 0, &mut disk);
    assert_eq!(result.response, VIRTIO_SCSI_S_OK);
    assert_eq!(result.status, scsi_status::GOOD);

    let reserve = pr_out_cdb(PR_OUT_RESERVE, PR_TYPE_EXCLUSIVE_ACCESS);
    let result = processor.process_command(&reserve, &pr_out_payload(0x1234, 0), 0, &mut disk);
    assert_eq!(result.response, VIRTIO_SCSI_S_OK);
    assert_eq!(result.status, scsi_status::GOOD);

    let read_keys = pr_in_cdb(PR_IN_READ_KEYS);
    let result = processor.process_command(&read_keys, &[], 64, &mut disk);
    assert_eq!(result.status, scsi_status::GOOD);
    assert_eq!(
        u32::from_be_bytes(result.data_in[4..8].try_into().unwrap()),
        8
    );
    assert_eq!(
        u64::from_be_bytes(result.data_in[8..16].try_into().unwrap()),
        0x1234
    );

    let read_reservation = pr_in_cdb(PR_IN_READ_RESERVATION);
    let result = processor.process_command(&read_reservation, &[], 64, &mut disk);
    assert_eq!(result.status, scsi_status::GOOD);
    assert_eq!(
        u32::from_be_bytes(result.data_in[4..8].try_into().unwrap()),
        16
    );
    assert_eq!(
        u64::from_be_bytes(result.data_in[8..16].try_into().unwrap()),
        0x1234
    );
    assert_eq!(result.data_in[21], PR_TYPE_EXCLUSIVE_ACCESS);
}
