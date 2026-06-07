# Virtio SCSI

Cloud Hypervisor supports a disk-backed virtio-scsi host bus adapter (HBA).
Each configured LUN is backed by a host file and is exposed to the guest as a
SCSI disk or read-only CD-ROM device.

## Overview

The virtio-scsi device is defined in the VIRTIO specification (Section 5.6)
and provides:

- Emulated SCSI commands for disk-backed LUNs
- Multiple logical units (LUNs) per target
- SCSI event queue support for reset/rescan/remove, parameter-change, and
  subscribed asynchronous notifications
- Basic task management/control queue handling

## Configuration

### CLI Usage

Add a SCSI controller with one LUN using the `--scsi` option:

```bash
cloud-hypervisor \
    --kernel /path/to/kernel \
    --scsi path=/path/to/disk.img
```

Full syntax:
```
--scsi path=<disk_path>,id=<device_id>,pci_segment=<segment>,iommu=on|off,
       num_queues=<n>,queue_size=<size>,target=<target_id>,lun=<lun_id>,
       readonly=on|off,direct=on|off,device_type=disk|cdrom
```

### Parameters

| Parameter | Description | Default |
|-----------|-------------|---------|
| path | Path to disk image (required for CLI) | - |
| id | Device identifier | Auto-generated |
| pci_segment | PCI segment for the device | 0 |
| iommu | Enable IOMMU protection | off |
| num_queues | Number of request queues | 1 |
| queue_size | Size of each virtqueue | 128 |
| target | SCSI target ID (0-255) | 0 |
| lun | SCSI LUN number (0-16383) | 0 |
| readonly | Read-only access to disk | off |
| direct | Use O_DIRECT for I/O (bypass page cache) | off |
| device_type | Device type: `disk` or `cdrom` | disk |
| vhost_user_socket | Path to vhost-user-scsi socket (currently unsupported; validation fails if set) | - |

### JSON/API Configuration

For configurations with multiple LUNs, use JSON or the HTTP API. Each
controller must contain at least one LUN, and each LUN must provide a `path`.

```json
{
    "scsi": [
        {
            "id": "scsi0",
            "num_queues": 4,
            "queue_size": 256,
            "luns": [
                {
                    "path": "/path/to/disk1.img",
                    "target": 0,
                    "lun": 0,
                    "readonly": false,
                    "direct": true,
                    "device_type": "disk"
                },
                {
                    "path": "/path/to/cdrom.iso",
                    "target": 0,
                    "lun": 1,
                    "readonly": true,
                    "device_type": "cdrom"
                }
            ]
        }
    ]
}
```

### CD/DVD Emulation

To expose an ISO image as a CD-ROM device:

```bash
cloud-hypervisor \
    --scsi path=/path/to/installer.iso,device_type=cdrom,readonly=on
```

The CD-ROM device appears as a SCSI device type 0x05 (MMC/CD-DVD) to the guest.

### Vhost-User Backend

`vhost_user_socket` is present in the configuration schema for compatibility,
but vhost-user-scsi is not supported. Any configuration that sets
`vhost_user_socket` is rejected during validation with
`vhost-user SCSI is not supported`.

```bash
cloud-hypervisor \
    --scsi vhost_user_socket=/path/to/vhost-scsi.sock
```

## Controller Hotplug

SCSI controllers can be added at runtime using the HTTP API:

```bash
curl -X PUT \
    -H "Content-Type: application/json" \
    -d '{"id":"scsi1","luns":[{"path":"/path/to/disk.img","target":0,"lun":0}]}' \
    http://localhost/api/v1/vm.add-scsi
```

The API adds a complete virtio-scsi controller. It does not provide a separate
API to add or remove individual LUNs on an existing controller.

## Supported SCSI Commands

The virtio-scsi implementation supports the following SCSI commands:

| Command | Opcode | Description |
|---------|--------|-------------|
| TEST UNIT READY | 0x00 | Check if LUN is ready |
| REQUEST SENSE | 0x03 | Get sense data |
| INQUIRY | 0x12 | Get device information |
| MODE SELECT (6) | 0x15 | Validate supported mode parameters |
| MODE SENSE (6) | 0x1A | Get device parameters |
| START STOP UNIT | 0x1B | Update LUN ready state |
| MODE SENSE (10) | 0x5A | Get device parameters (extended) |
| MODE SELECT (10) | 0x55 | Validate supported mode parameters |
| READ CAPACITY (10) | 0x25 | Get disk capacity |
| READ CAPACITY (16) | 0x9E | Get disk capacity (extended) |
| READ (10) | 0x28 | Read data (32-bit LBA) |
| READ (16) | 0x88 | Read data (64-bit LBA) |
| WRITE (10) | 0x2A | Write data (32-bit LBA) |
| WRITE (16) | 0x8A | Write data (64-bit LBA) |
| SYNCHRONIZE CACHE | 0x35 | Flush write cache |
| PERSISTENT RESERVE IN | 0x5E | Report per-LUN reservation state |
| PERSISTENT RESERVE OUT | 0x5F | Update per-LUN reservation state |
| REPORT LUNS | 0xA0 | Report the addressed LUN |
| UNMAP | 0x42 | Punch holes in the backing file when supported by the host filesystem |
| PREVENT ALLOW MEDIUM REMOVAL | 0x1E | Accepted as a no-op |

## Device Features

The virtio-scsi device advertises the following feature bits:

- `VIRTIO_SCSI_F_INOUT` (0): Support bidirectional commands
- `VIRTIO_SCSI_F_HOTPLUG` (1): Support SCSI transport reset events for
  rescan/remove notifications
- `VIRTIO_SCSI_F_CHANGE` (2): Support LUN parameter-change events

## Queue Layout

The virtio-scsi device uses multiple virtqueues:

1. **Control Queue (Queue 0)**: Task management functions and async notification
   query/subscribe requests
2. **Event Queue (Queue 1)**: Transport reset, async notification, and parameter
   change events
3. **Request Queues (Queue 2+)**: SCSI command I/O

The number of request queues can be configured with `num_queues`.

## Guest Requirements

The guest kernel must have virtio-scsi support enabled:

- Linux: `CONFIG_SCSI_VIRTIO=y` or `CONFIG_SCSI_VIRTIO=m`
- The `virtio_scsi` module should be loaded

## Performance Considerations

- Multiple request queues (`num_queues`) can improve performance with
  multi-threaded I/O workloads
- Larger queue sizes (`queue_size`) may improve throughput for large I/O
- Use `direct=on` for O_DIRECT access to underlying storage
- UNMAP uses host hole punching (`fallocate(FALLOC_FL_PUNCH_HOLE)`); unsupported
  backing filesystems or files return a SCSI command error.

## Differences from virtio-blk

| Feature | virtio-scsi | virtio-blk |
|---------|-------------|------------|
| Multiple LUNs | Yes | No |
| SCSI commands | Emulated disk/CD-ROM command set | Limited |
| Device model | SCSI HBA | Block device |
| Complexity | Higher | Lower |
| Protocol overhead | Higher | Lower |

Use virtio-scsi when you need:
- Multiple disks on a single controller
- SCSI-specific features such as INQUIRY, REPORT LUNS, persistent reservation
  command compatibility, and UNMAP
- Migration from physical SCSI environments

Use virtio-blk when you need:
- Simple, high-performance block access
- Single disk per controller
- Lower CPU overhead

## Limitations

- Vhost-user-scsi is unsupported; configurations with `vhost_user_socket` fail
  validation.
- Persistent reservation commands are implemented with Cloud Hypervisor per-LUN
  state for guest compatibility. They are not backed by host storage
  reservations and are not shared with other VMs or vhost-user backends.
- No multipath support.
- No T10 Protection Information (DIF/DIX) support.
- CD/DVD emulation is read-only

## See Also

- [Device Model](device_model.md) - Overview of supported devices
- [Hotplug](hotplug.md) - Dynamic device addition/removal
- [I/O Throttling](io_throttling.md) - Rate limiting for storage devices
