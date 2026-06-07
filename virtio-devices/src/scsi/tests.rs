// Copyright © 2026 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Tests for the virtio-scsi module.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::commands::*;
    use super::super::protocol::*;
    use super::super::target::*;

    #[allow(dead_code)]
    fn create_test_config() -> ScsiLunConfig {
        ScsiLunConfig {
            target: 0,
            lun: 0,
            path: PathBuf::from("/dev/null"),
            readonly: false,
            direct: false,
            device_type: ScsiDeviceType::DirectAccess,
            vendor_id: "TEST    ".to_string(),
            product_id: "VIRTUAL DISK    ".to_string(),
            product_rev: "1.0 ".to_string(),
        }
    }

    #[test]
    fn test_scsi_lun_id() {
        let lun_id = ScsiLunId::new(1, 2);
        assert_eq!(lun_id.target, 1);
        assert_eq!(lun_id.lun, 2);
        assert_eq!(format!("{}", lun_id), "1:2");
    }

    #[test]
    fn test_scsi_lun_config_default() {
        let config = ScsiLunConfig::default();
        assert_eq!(config.target, 0);
        assert_eq!(config.lun, 0);
        assert!(!config.readonly);
        assert_eq!(config.device_type, ScsiDeviceType::DirectAccess);
    }

    #[test]
    fn test_scsi_device_type() {
        assert_eq!(ScsiDeviceType::DirectAccess as u8, 0x00);
        assert_eq!(ScsiDeviceType::CdDvd as u8, 0x05);
        assert_eq!(ScsiDeviceType::from(0x00), ScsiDeviceType::DirectAccess);
        assert_eq!(ScsiDeviceType::from(0xFF), ScsiDeviceType::Unknown);
    }

    #[test]
    fn test_virtio_scsi_config() {
        let config = VirtioScsiConfig::new(4);
        assert_eq!(config.num_queues, 4);
        assert_eq!(config.max_target, 255);
        assert_eq!(config.max_lun, 16383);
    }

    #[test]
    fn test_command_result_default() {
        let result = ScsiCommandResult::default();
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::GOOD);
        assert!(result.data_in.is_empty());
        assert!(result.sense.is_empty());
    }

    #[test]
    fn test_command_result_ok_with_data() {
        let data = vec![1, 2, 3, 4];
        let result = ScsiCommandResult::ok_with_data(data.clone());
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::GOOD);
        assert_eq!(result.data_in, data);
    }

    #[test]
    fn test_command_result_check_condition() {
        let result = ScsiCommandResult::check_condition(sense_key::ILLEGAL_REQUEST, 0x24, 0x00);
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::CHECK_CONDITION);
        assert!(!result.sense.is_empty());
    }

    #[test]
    fn test_bad_target_response() {
        let result = ScsiCommandResult::bad_target();
        assert_eq!(result.response, VIRTIO_SCSI_S_BAD_TARGET);
    }

    #[test]
    fn test_incorrect_lun_response() {
        let result = ScsiCommandResult::incorrect_lun();
        // For request queues, incorrect LUN returns OK with CHECK CONDITION
        // and sense data for LOGICAL UNIT NOT SUPPORTED
        assert_eq!(result.response, VIRTIO_SCSI_S_OK);
        assert_eq!(result.status, scsi_status::CHECK_CONDITION);
        assert!(!result.sense.is_empty());
        // Verify sense key is ILLEGAL_REQUEST (0x05)
        assert_eq!(result.sense[2] & 0x0F, sense_key::ILLEGAL_REQUEST);
        // Verify ASC is 0x25 (LOGICAL UNIT NOT SUPPORTED)
        assert_eq!(result.sense[12], 0x25);
    }

    #[test]
    fn test_lun_parsing() {
        // Test LUN parsing for single-level addressing (target, LUN)
        // Format: [format=1, target, lun_hi, lun_lo, 0, 0, 0, 0]
        let lun_bytes: [u8; 8] = [0x01, 0x01, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00];
        let result = parse_lun(&lun_bytes);
        assert!(result.is_some());
        let (target, lun) = result.unwrap();
        assert_eq!(target, 1);
        assert_eq!(lun, 5);
    }

    #[test]
    fn test_scsi_opcodes() {
        assert_eq!(scsi_opcode::TEST_UNIT_READY, 0x00);
        assert_eq!(scsi_opcode::INQUIRY, 0x12);
        assert_eq!(scsi_opcode::READ_10, 0x28);
        assert_eq!(scsi_opcode::WRITE_10, 0x2A);
        assert_eq!(scsi_opcode::READ_CAPACITY_10, 0x25);
        assert_eq!(scsi_opcode::PERSISTENT_RESERVE_IN, 0x5E);
        assert_eq!(scsi_opcode::PERSISTENT_RESERVE_OUT, 0x5F);
    }

    #[test]
    fn test_scsi_status_codes() {
        assert_eq!(scsi_status::GOOD, 0x00);
        assert_eq!(scsi_status::CHECK_CONDITION, 0x02);
        assert_eq!(scsi_status::BUSY, 0x08);
    }

    #[test]
    fn test_sense_keys() {
        assert_eq!(sense_key::NO_SENSE, 0x00);
        assert_eq!(sense_key::NOT_READY, 0x02);
        assert_eq!(sense_key::MEDIUM_ERROR, 0x03);
        assert_eq!(sense_key::ILLEGAL_REQUEST, 0x05);
    }
}
