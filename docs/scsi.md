# Virtio SCSI

Cloud Hypervisor supports virtio-scsi, a paravirtualized SCSI host bus adapter
(HBA) that provides SCSI transport capabilities to guest VMs. This allows VMs
to access storage devices using the full SCSI protocol, including advanced
features like multiple LUNs per target.

## Overview

The virtio-scsi device is defined in the VIRTIO specification (Section 5.6)
and provides:

- SCSI command passthrough with full SCSI semantics
- Multiple logical units (LUNs) per target
- Support for standard SCSI commands
- Event notification for hotplug and device changes
- Task management functions (TMF)

## Configuration

### CLI Usage

Add a SCSI controller with a disk using the `--scsi` option:

```bash
cloud-hypervisor \
    --kernel /path/to/kernel \
    --scsi path=/path/to/disk.img
```

Full syntax:
```
--scsi path=<disk_path>,id=<device_id>,pci_segment=<segment>,iommu=on|off,
       num_queues=<n>,queue_size=<size>,target=<target_id>,lun=<lun_id>,
       readonly=on|off
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

### JSON/API Configuration

For more complex configurations with multiple LUNs, use JSON:

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
                    "readonly": false
                },
                {
                    "path": "/path/to/disk2.img",
                    "target": 0,
                    "lun": 1,
                    "readonly": true
                }
            ]
        }
    ]
}
```

## Hotplug

SCSI controllers can be added at runtime using the HTTP API:

```bash
curl -X PUT \
    -H "Content-Type: application/json" \
    -d '{"id":"scsi1","luns":[{"path":"/path/to/disk.img","target":0,"lun":0}]}' \
    http://localhost/api/v1/vm.add-scsi
```

## Supported SCSI Commands

The virtio-scsi implementation supports the following SCSI commands:

| Command | Opcode | Description |
|---------|--------|-------------|
| TEST UNIT READY | 0x00 | Check if LUN is ready |
| REQUEST SENSE | 0x03 | Get sense data |
| INQUIRY | 0x12 | Get device information |
| MODE SENSE (6) | 0x1A | Get device parameters |
| MODE SENSE (10) | 0x5A | Get device parameters (extended) |
| READ CAPACITY (10) | 0x25 | Get disk capacity |
| READ CAPACITY (16) | 0x9E | Get disk capacity (extended) |
| READ (10) | 0x28 | Read data (32-bit LBA) |
| READ (16) | 0x88 | Read data (64-bit LBA) |
| WRITE (10) | 0x2A | Write data (32-bit LBA) |
| WRITE (16) | 0x8A | Write data (64-bit LBA) |
| SYNCHRONIZE CACHE | 0x35 | Flush write cache |
| REPORT LUNS | 0xA0 | List available LUNs |

## Device Features

The virtio-scsi device advertises the following feature bits:

- `VIRTIO_SCSI_F_INOUT` (0): Support bidirectional commands
- `VIRTIO_SCSI_F_HOTPLUG` (1): Support LUN hotplug/unplug
- `VIRTIO_SCSI_F_CHANGE` (2): Support LUN parameter changes

## Queue Layout

The virtio-scsi device uses multiple virtqueues:

1. **Control Queue (Queue 0)**: Task management functions and async notifications
2. **Event Queue (Queue 1)**: Hotplug events and LUN changes
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

## Differences from virtio-blk

| Feature | virtio-scsi | virtio-blk |
|---------|-------------|------------|
| Multiple LUNs | Yes | No |
| SCSI commands | Full support | Limited |
| Device model | SCSI HBA | Block device |
| Complexity | Higher | Lower |
| Protocol overhead | Higher | Lower |

Use virtio-scsi when you need:
- Multiple disks on a single controller
- SCSI-specific features (INQUIRY, etc.)
- Migration from physical SCSI environments

Use virtio-blk when you need:
- Simple, high-performance block access
- Single disk per controller
- Lower CPU overhead

## Limitations

- No vhost-user backend support (planned)
- No persistent reservations (PR) support (planned)
- No multipath support
- No CD/DVD device emulation (planned)

## See Also

- [Device Model](device_model.md) - Overview of supported devices
- [Hotplug](hotplug.md) - Dynamic device addition/removal
- [I/O Throttling](io_throttling.md) - Rate limiting for storage devices
