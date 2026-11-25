#!/usr/bin/env python3
"""
GPT Disk Image Builder for Magma OS

Creates a 160 MiB (+1.25% System Reserved tail) disk image with 3 GPT partitions:
- Partition 1: MAGMA_BOOT (ESP) - FAT32, 40 MiB
- Partition 2: MAGMA_DATA (Raw/Custom CoW FS) - 64 MiB
- Partition 3: MAGMA_RECOVERY (Recovery Vault) - FAT32, 40 MiB, read-only
"""

import struct
import zlib
import os
import sys
import uuid

# Constants
SECTOR_SIZE = 512
BASE_SECTORS = 327680  # 160 MiB = 167,772,160 bytes
SYSTEM_RESERVED_SECTORS = 4096  # 2 MiB (1.25% overhead)
TOTAL_SECTORS = BASE_SECTORS + SYSTEM_RESERVED_SECTORS  # 331,776 sectors = 169,869,312 bytes

# Partition layout (1 MiB aligned)
PARTITION_1_START = 2048
PARTITION_1_END = 83967  # 81,920 sectors = 40 MiB

PARTITION_2_START = 83968
PARTITION_2_END = 215039  # 131,072 sectors = 64 MiB

PARTITION_3_START = 215040
PARTITION_3_END = 296959  # 81,920 sectors = 40 MiB

# GPT LBA layout
PRIMARY_HEADER_LBA = 1
PRIMARY_ENTRIES_LBA = 2
PRIMARY_ENTRIES_SECTORS = 32

BACKUP_HEADER_LBA = TOTAL_SECTORS - 1  # LBA 331775
BACKUP_ENTRIES_LBA = TOTAL_SECTORS - 33  # LBA 331743..331774 (32 sectors)
FIRST_USABLE_LBA = 2048
LAST_USABLE_LBA = BACKUP_ENTRIES_LBA - 1  # LBA 331742

# Expected image size bounds (Base + up to 1.5% System Reserved tolerance)
BASE_SIZE = BASE_SECTORS * SECTOR_SIZE
MAX_SIZE = int(BASE_SIZE * 1.015)

# GUIDs (UEFI mixed-endian format)
EFI_SYSTEM_PARTITION_GUID = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").bytes_le
MAGMA_DATA_PARTITION_GUID = uuid.UUID("0FC63DAF-8483-4772-8E79-3D69D8477DE4").bytes_le
RECOVERY_PARTITION_GUID = uuid.UUID("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC").bytes_le
DISK_GUID = uuid.UUID("A1B2C3D4-E5F6-7890-1234-56789ABCDEF0").bytes_le

# Partition attributes
ATTR_READ_ONLY = 1 << 60


def write_bytes(disk: bytearray, offset: int, data: bytes | bytearray) -> None:
    """Bounds-checked exact-length write into the disk buffer."""
    end = offset + len(data)
    if offset < 0 or end > len(disk):
        raise ValueError(f"Out-of-bounds write {offset}..{end} (disk={len(disk)})")
    disk[offset:end] = data


def create_protective_mbr() -> bytes:
    """Create a UEFI-compliant Protective MBR at LBA 0."""
    mbr = bytearray(SECTOR_SIZE)
    mbr[446] = 0x00
    mbr[447:450] = b"\x00\x02\x00"
    mbr[450] = 0xEE
    mbr[451:454] = b"\xFF\xFF\xFF"
    mbr[454:458] = struct.pack("<I", 1)
    mbr[458:462] = struct.pack("<I", TOTAL_SECTORS - 1)
    mbr[510:512] = b"\x55\xAA"
    return bytes(mbr)


def create_gpt_header(
    my_lba: int,
    alternate_lba: int,
    partition_entry_lba: int,
    partition_entry_count: int,
    disk_guid: bytes,
    partition_array_crc: int,
) -> bytes:
    """Create a 92-byte GPT header with valid HeaderCRC32."""
    header = bytearray(92)
    header[0:8] = b"EFI PART"
    struct.pack_into("<I", header, 8, 0x00010000)
    struct.pack_into("<I", header, 12, 92)
    struct.pack_into("<I", header, 16, 0)
    struct.pack_into("<I", header, 20, 0)
    struct.pack_into("<Q", header, 24, my_lba)
    struct.pack_into("<Q", header, 32, alternate_lba)
    struct.pack_into("<Q", header, 40, FIRST_USABLE_LBA)
    struct.pack_into("<Q", header, 48, LAST_USABLE_LBA)
    header[56:72] = disk_guid
    struct.pack_into("<Q", header, 72, partition_entry_lba)
    struct.pack_into("<I", header, 80, partition_entry_count)
    struct.pack_into("<I", header, 84, 128)
    struct.pack_into("<I", header, 88, partition_array_crc)

    crc = zlib.crc32(bytes(header)) & 0xFFFFFFFF
    struct.pack_into("<I", header, 16, crc)
    return bytes(header)


def create_partition_entry(
    partition_type_guid: bytes,
    unique_guid: bytes,
    starting_lba: int,
    ending_lba: int,
    attributes: int = 0,
    name: str = "",
) -> bytes:
    """Create a 128-byte GPT partition entry."""
    entry = bytearray(128)
    entry[0:16] = partition_type_guid
    entry[16:32] = unique_guid
    struct.pack_into("<Q", entry, 32, starting_lba)
    struct.pack_into("<Q", entry, 40, ending_lba)
    struct.pack_into("<Q", entry, 48, attributes)
    name_utf16 = name.encode("utf-16le")[:72]
    entry[56:128] = name_utf16.ljust(72, b"\x00")
    return bytes(entry)


def create_fat32_boot_sector(
    total_sectors_32: int,
    fat_size_32: int,
    volume_label: str,
    sectors_per_cluster: int = 1,
    reserved_sectors: int = 32,
    num_fats: int = 2,
    media_type: int = 0xF8,
    fs_info_sector: int = 1,
    backup_boot_sector: int = 6,
    root_cluster: int = 2,
) -> bytes:
    """Create a FAT32 Volume Boot Record (VBR)."""
    vbr = bytearray(SECTOR_SIZE)
    vbr[0:3] = b"\xEB\x58\x90"
    vbr[3:11] = b"MAGMAOS "
    struct.pack_into("<H", vbr, 11, SECTOR_SIZE)
    vbr[13] = sectors_per_cluster
    struct.pack_into("<H", vbr, 14, reserved_sectors)
    vbr[16] = num_fats
    struct.pack_into("<H", vbr, 17, 0)
    struct.pack_into("<H", vbr, 19, 0)
    vbr[21] = media_type
    struct.pack_into("<H", vbr, 22, 0)
    struct.pack_into("<H", vbr, 24, 63)
    struct.pack_into("<H", vbr, 26, 255)
    struct.pack_into("<I", vbr, 28, 0)
    struct.pack_into("<I", vbr, 32, total_sectors_32)
    struct.pack_into("<I", vbr, 36, fat_size_32)
    struct.pack_into("<H", vbr, 40, 0)
    struct.pack_into("<H", vbr, 42, 0)
    struct.pack_into("<I", vbr, 44, root_cluster)
    struct.pack_into("<H", vbr, 48, fs_info_sector)
    struct.pack_into("<H", vbr, 50, backup_boot_sector)
    vbr[64] = 0x80
    vbr[66] = 0x29
    struct.pack_into("<I", vbr, 67, 0x12345678)
    vbr[71:82] = volume_label.encode("ascii")[:11].ljust(11)
    vbr[82:90] = b"FAT32   "
    vbr[90:510] = b"\x90" * 420
    vbr[510:512] = b"\x55\xAA"
    return bytes(vbr)


def create_fat32_fsinfo() -> bytes:
    """Create a valid FAT32 FSInfo sector."""
    fsinfo = bytearray(SECTOR_SIZE)
    struct.pack_into("<I", fsinfo, 0, 0x41615252)
    fsinfo[4:484] = b"\x00" * 480
    struct.pack_into("<I", fsinfo, 484, 0x61417272)
    struct.pack_into("<I", fsinfo, 488, 0xFFFFFFFF)
    struct.pack_into("<I", fsinfo, 492, 0xFFFFFFFF)
    struct.pack_into("<I", fsinfo, 508, 0xAA550000)
    return bytes(fsinfo)


def lfn_checksum(short_name_11: bytes) -> int:
    """Compute VFAT 8.3 short-name checksum for Long File Name entries."""
    s = 0
    for b in short_name_11:
        s = (((s & 1) << 7) + (s >> 1) + b) & 0xFF
    return s


def make_lfn_entries(long_name: str, short_name_11: bytes) -> list[bytes]:
    """Build VFAT Long File Name (LFN) 32-byte entries in reverse disk order."""
    chk = lfn_checksum(short_name_11)
    utf16_units = [ord(c) for c in long_name] + [0x0000]
    while len(utf16_units) % 13 != 0:
        utf16_units.append(0xFFFF)

    num_entries = len(utf16_units) // 13
    entries = []
    for idx in range(num_entries):
        seq = idx + 1
        if idx == num_entries - 1:
            seq |= 0x40
        chunk = utf16_units[idx * 13 : (idx + 1) * 13]
        raw = bytearray(32)
        raw[0] = seq
        for j in range(5):
            struct.pack_into("<H", raw, 1 + j * 2, chunk[j])
        raw[11] = 0x0F
        raw[12] = 0x00
        raw[13] = chk
        for j in range(6):
            struct.pack_into("<H", raw, 14 + j * 2, chunk[5 + j])
        struct.pack_into("<H", raw, 26, 0)
        for j in range(2):
            struct.pack_into("<H", raw, 28 + j * 2, chunk[11 + j])
        entries.append(bytes(raw))

    entries.reverse()
    return entries


class Fat32Writer:
    """Self-contained FAT32 formatter and file/directory writer."""

    def __init__(
        self,
        disk: bytearray,
        partition_lba: int,
        volume_sectors: int,
        volume_label: str,
        reserved_sectors: int = 32,
        num_fats: int = 2,
        sectors_per_fat: int = 636,
    ):
        self.disk = disk
        self.partition_lba = partition_lba
        self.volume_sectors = volume_sectors
        self.reserved_sectors = reserved_sectors
        self.num_fats = num_fats
        self.sectors_per_fat = sectors_per_fat
        self.first_data_sector = reserved_sectors + num_fats * sectors_per_fat

        part_offset = partition_lba * SECTOR_SIZE
        self.part_offset = part_offset
        self.fat_start = part_offset + reserved_sectors * SECTOR_SIZE
        self.data_start = part_offset + self.first_data_sector * SECTOR_SIZE

        # Write VBR, FSInfo, and Backup VBR
        vbr = create_fat32_boot_sector(
            total_sectors_32=volume_sectors,
            fat_size_32=sectors_per_fat,
            volume_label=volume_label,
            reserved_sectors=reserved_sectors,
            num_fats=num_fats,
        )
        fsinfo = create_fat32_fsinfo()
        write_bytes(disk, part_offset, vbr)
        write_bytes(disk, part_offset + SECTOR_SIZE, fsinfo)
        write_bytes(disk, part_offset + 6 * SECTOR_SIZE, vbr)

        # Initialize FAT1 and FAT2 with FAT[0], FAT[1], and FAT[2] (root dir)
        self.next_cluster = 3
        self.dir_slots: dict[int, int] = {2: 2}  # cluster -> next 32-byte slot index
        self._set_fat(0, 0x0FFFFFF8)
        self._set_fat(1, 0x0FFFFFFF)
        self._set_fat(2, 0x0FFFFFFF)

        # Initialize root directory (cluster 2)
        root_off = self._cluster_offset(2)
        write_bytes(disk, root_off, self._pack_83_raw(b".          ", 0x10, 2, 0))
        write_bytes(disk, root_off + 32, self._pack_83_raw(b"..         ", 0x10, 2, 0))

    def _cluster_offset(self, cluster: int) -> int:
        return self.data_start + (cluster - 2) * SECTOR_SIZE

    def _set_fat(self, cluster: int, value: int) -> None:
        packed = struct.pack("<I", value)
        for f in range(self.num_fats):
            fat_off = self.fat_start + f * self.sectors_per_fat * SECTOR_SIZE + cluster * 4
            write_bytes(self.disk, fat_off, packed)

    def _alloc_chain(self, num_clusters: int) -> int:
        start = self.next_cluster
        for i in range(num_clusters):
            c = start + i
            nxt = (c + 1) if (i + 1 < num_clusters) else 0x0FFFFFFF
            self._set_fat(c, nxt)
        self.next_cluster += num_clusters
        return start

    @staticmethod
    def _format_83(name: str) -> tuple[bytes, bool]:
        """Return (11-byte 8.3 name, needs_lfn)."""
        if "." in name:
            base, ext = name.rsplit(".", 1)
        else:
            base, ext = name, ""
        is_exact_83 = (
            len(base) <= 8
            and len(ext) <= 3
            and name == name.upper()
            and " " not in name
        )
        if is_exact_83:
            s11 = base.ljust(8).encode("ascii") + ext.ljust(3).encode("ascii")
            return s11, False
        clean_base = "".join(c for c in base.upper() if c.isalnum() or c in "_-")[:6] or "FILE"
        short_base = f"{clean_base}~1".ljust(8)[:8]
        short_ext = ext.upper()[:3].ljust(3)
        return (short_base + short_ext).encode("ascii"), True

    @staticmethod
    def _pack_83_raw(short_11: bytes, attrs: int, cluster: int, size: int) -> bytes:
        entry = bytearray(32)
        entry[0:11] = short_11
        entry[11] = attrs
        struct.pack_into("<H", entry, 20, (cluster >> 16) & 0xFFFF)
        struct.pack_into("<H", entry, 26, cluster & 0xFFFF)
        struct.pack_into("<I", entry, 28, size)
        return bytes(entry)

    def _append_dir_entries(self, dir_cluster: int, entries: list[bytes]) -> None:
        slot = self.dir_slots[dir_cluster]
        if slot + len(entries) > 16:
            raise RuntimeError(f"Directory cluster {dir_cluster} exceeded 16 entries")
        base_off = self._cluster_offset(dir_cluster)
        for idx, raw in enumerate(entries):
            write_bytes(self.disk, base_off + (slot + idx) * 32, raw)
        self.dir_slots[dir_cluster] = slot + len(entries)

    def mkdir(self, parent_cluster: int, name: str) -> int:
        """Create a subdirectory under parent_cluster and return its cluster."""
        dir_cluster = self._alloc_chain(1)
        self.dir_slots[dir_cluster] = 2

        dot_parent = 0 if parent_cluster == 2 else parent_cluster
        dir_off = self._cluster_offset(dir_cluster)
        write_bytes(self.disk, dir_off, self._pack_83_raw(b".          ", 0x10, dir_cluster, 0))
        write_bytes(self.disk, dir_off + 32, self._pack_83_raw(b"..         ", 0x10, dot_parent, 0))

        short_11, needs_lfn = self._format_83(name)
        entries = make_lfn_entries(name, short_11) if needs_lfn else []
        entries.append(self._pack_83_raw(short_11, 0x10, dir_cluster, 0))
        self._append_dir_entries(parent_cluster, entries)
        return dir_cluster

    def add_file(self, dir_cluster: int, name: str, data: bytes) -> int:
        """Write file data clusters and register its directory entry."""
        num_clusters = max(1, (len(data) + SECTOR_SIZE - 1) // SECTOR_SIZE)
        start_cluster = self._alloc_chain(num_clusters)

        padded = data.ljust(num_clusters * SECTOR_SIZE, b"\x00")
        write_bytes(self.disk, self._cluster_offset(start_cluster), padded)

        short_11, needs_lfn = self._format_83(name)
        entries = make_lfn_entries(name, short_11) if needs_lfn else []
        entries.append(self._pack_83_raw(short_11, 0x20, start_cluster, len(data)))
        self._append_dir_entries(dir_cluster, entries)
        return start_cluster


def build_disk_image(output_path: str = "target/magma_disk.img") -> str:
    """Build the complete 3-partition GPT disk image."""
    print("Building 160 MiB GPT disk image with System Reserved tail space...")

    boot_efi_path = "boot/limine/BOOTX64.EFI"
    limine_conf_path = "boot/limine.conf"
    kernel_path = "target/x86_64-unknown-none/debug/magma_os"

    for path, desc in [
        (boot_efi_path, "BOOTX64.EFI"),
        (limine_conf_path, "limine.conf"),
        (kernel_path, "magma_os kernel"),
    ]:
        if not os.path.exists(path) or os.path.getsize(path) == 0:
            raise FileNotFoundError(f"Required file missing or empty: {path} ({desc})")

    with open(boot_efi_path, "rb") as f:
        boot_efi_content = f.read()
    with open(limine_conf_path, "rb") as f:
        limine_conf_content = f.read()
    with open(kernel_path, "rb") as f:
        kernel_content = f.read()

    print(f"  Found BOOTX64.EFI ({len(boot_efi_content)} bytes)")
    print(f"  Found limine.conf ({len(limine_conf_content)} bytes)")
    print(f"  Found magma_os kernel ({len(kernel_content)} bytes)")

    disk = bytearray(TOTAL_SECTORS * SECTOR_SIZE)

    # 1. Protective MBR at LBA 0
    print("  Creating protective MBR...")
    write_bytes(disk, 0, create_protective_mbr())

    # 2. Partition Entry Array (128 entries * 128 bytes = 16,384 bytes)
    print("  Creating partition entries...")
    partition_entries = bytearray(PRIMARY_ENTRIES_SECTORS * SECTOR_SIZE)
    write_bytes(
        partition_entries,
        0,
        create_partition_entry(
            EFI_SYSTEM_PARTITION_GUID,
            uuid.uuid4().bytes_le,
            PARTITION_1_START,
            PARTITION_1_END,
            0,
            "MAGMA_BOOT",
        ),
    )
    write_bytes(
        partition_entries,
        128,
        create_partition_entry(
            MAGMA_DATA_PARTITION_GUID,
            uuid.uuid4().bytes_le,
            PARTITION_2_START,
            PARTITION_2_END,
            0,
            "MAGMA_DATA",
        ),
    )
    write_bytes(
        partition_entries,
        256,
        create_partition_entry(
            RECOVERY_PARTITION_GUID,
            uuid.uuid4().bytes_le,
            PARTITION_3_START,
            PARTITION_3_END,
            ATTR_READ_ONLY,
            "MAGMA_RECOVERY",
        ),
    )
    pe_crc = zlib.crc32(bytes(partition_entries)) & 0xFFFFFFFF

    # 3. Primary & Backup GPT Headers and Entry Arrays
    print("  Creating primary and backup GPT structures...")
    primary_header = create_gpt_header(
        my_lba=PRIMARY_HEADER_LBA,
        alternate_lba=BACKUP_HEADER_LBA,
        partition_entry_lba=PRIMARY_ENTRIES_LBA,
        partition_entry_count=128,
        disk_guid=DISK_GUID,
        partition_array_crc=pe_crc,
    )
    write_bytes(disk, PRIMARY_HEADER_LBA * SECTOR_SIZE, primary_header.ljust(SECTOR_SIZE, b"\x00"))
    write_bytes(disk, PRIMARY_ENTRIES_LBA * SECTOR_SIZE, partition_entries)

    backup_header = create_gpt_header(
        my_lba=BACKUP_HEADER_LBA,
        alternate_lba=PRIMARY_HEADER_LBA,
        partition_entry_lba=BACKUP_ENTRIES_LBA,
        partition_entry_count=128,
        disk_guid=DISK_GUID,
        partition_array_crc=pe_crc,
    )
    write_bytes(disk, BACKUP_ENTRIES_LBA * SECTOR_SIZE, partition_entries)
    write_bytes(disk, BACKUP_HEADER_LBA * SECTOR_SIZE, backup_header.ljust(SECTOR_SIZE, b"\x00"))

    # 4. Format Partition 1 (MAGMA_BOOT / EFI System Partition)
    print("  Creating FAT32 filesystem on ESP partition...")
    esp_sectors = PARTITION_1_END - PARTITION_1_START + 1
    esp = Fat32Writer(disk, PARTITION_1_START, esp_sectors, "MAGMA_ESP")
    efi_dir = esp.mkdir(2, "EFI")
    efi_boot_dir = esp.mkdir(efi_dir, "BOOT")
    esp.add_file(efi_boot_dir, "BOOTX64.EFI", boot_efi_content)
    esp.add_file(efi_boot_dir, "limine.conf", limine_conf_content)
    esp.add_file(2, "limine.conf", limine_conf_content)
    boot_dir = esp.mkdir(2, "BOOT")
    esp.add_file(boot_dir, "MAGMA_OS", kernel_content)

    # 5. Format Partition 3 (MAGMA_RECOVERY Vault)
    print("  Creating FAT32 filesystem on MAGMA_RECOVERY partition...")
    rec_sectors = PARTITION_3_END - PARTITION_3_START + 1
    rec = Fat32Writer(disk, PARTITION_3_START, rec_sectors, "MAGMA_RECOV")

    system_cfg_path = "target/SYSTEM.CFG"
    if os.path.exists(system_cfg_path) and os.path.getsize(system_cfg_path) > 0:
        with open(system_cfg_path, "rb") as f:
            system_cfg_content = f.read()
    else:
        default_cfg = "[System]\nVersion=1.0\nTarget=magma_os\nBuild=x86_64-unknown-none\n"
        cfg_crc = zlib.crc32(default_cfg.encode()) & 0xFFFFFFFF
        default_cfg += f"CRC32={cfg_crc:08x}\n"
        system_cfg_content = default_cfg.encode()

    recovery_elf_path = "target/magma_recovery.elf"
    if os.path.exists(recovery_elf_path) and os.path.getsize(recovery_elf_path) > 0:
        with open(recovery_elf_path, "rb") as f:
            recovery_elf_content = f.read()
    else:
        recovery_elf_content = kernel_content

    rec.add_file(2, "SYSTEM.CFG", system_cfg_content)
    rec.add_file(2, "RECOVERY.ELF", recovery_elf_content)

    assert len(disk) == TOTAL_SECTORS * SECTOR_SIZE
    print(f"  Writing disk image to {output_path}...")
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with open(output_path, "wb") as f:
        f.write(disk)

    print("\nDisk image created successfully!")
    print(f"  File: {output_path}")
    print(f"  Size: {TOTAL_SECTORS} sectors ({BASE_SECTORS} base + {SYSTEM_RESERVED_SECTORS} reserved)")
    print("  Partitions:")
    print(f"    1. MAGMA_BOOT (ESP, FAT32): LBA {PARTITION_1_START}-{PARTITION_1_END} (40 MiB)")
    print(f"    2. MAGMA_DATA (Raw/CustomFS): LBA {PARTITION_2_START}-{PARTITION_2_END} (64 MiB)")
    print(f"    3. MAGMA_RECOVERY (Vault, Read-Only FAT32): LBA {PARTITION_3_START}-{PARTITION_3_END} (40 MiB)")
    return output_path


def _find_fat32_entry(blob: bytes, part_start_lba: int, dir_cluster: int, target_short_11: bytes) -> tuple[int, int] | None:
    """Look up a 32-byte 8.3 entry in a FAT32 directory cluster, returning (start_cluster, size)."""
    part_off = part_start_lba * SECTOR_SIZE
    vbr = blob[part_off : part_off + SECTOR_SIZE]
    reserved = struct.unpack("<H", vbr[14:16])[0]
    num_fats = vbr[16]
    fat_sz = struct.unpack("<I", vbr[36:40])[0]
    first_data = reserved + num_fats * fat_sz
    dir_off = part_off + (first_data + (dir_cluster - 2)) * SECTOR_SIZE

    for i in range(0, SECTOR_SIZE, 32):
        ent = blob[dir_off + i : dir_off + i + 32]
        if ent[0] == 0x00:
            break
        if ent[0] == 0xE5 or ent[11] == 0x0F:
            continue
        if ent[0:11] == target_short_11:
            clus = struct.unpack("<H", ent[26:28])[0] | (struct.unpack("<H", ent[20:22])[0] << 16)
            size = struct.unpack("<I", ent[28:32])[0]
            return clus, size
    return None


def verify_image(image_path: str) -> bool:
    """Verify GPT headers, partition CRCs, FAT32 BPBs, and staged boot/recovery files."""
    print(f"\n[VERIFY] Checking {image_path}...")
    if not os.path.exists(image_path):
        print("  ERROR: Image file does not exist")
        return False

    with open(image_path, "rb") as f:
        blob = f.read()

    blob_len = len(blob)
    if blob_len < BASE_SIZE or blob_len > MAX_SIZE or blob_len % SECTOR_SIZE != 0:
        print(f"  ERROR: Image size mismatch - expected {BASE_SIZE}..{MAX_SIZE} bytes, got {blob_len}")
        return False

    reserved_bytes = blob_len - BASE_SIZE
    print(f"  [OK] Image size: {blob_len} bytes (System Reserved: {reserved_bytes // SECTOR_SIZE} sectors / {reserved_bytes} bytes)")

    # Protective MBR
    mbr = blob[0:512]
    if mbr[510:512] != b"\x55\xAA" or mbr[450] != 0xEE:
        print("  ERROR: Invalid Protective MBR")
        return False
    print("  [OK] Protective MBR valid")

    # Primary & Backup GPT Headers
    for label, lba in [("Primary", PRIMARY_HEADER_LBA), ("Backup", BACKUP_HEADER_LBA)]:
        hdr = bytearray(blob[lba * SECTOR_SIZE : lba * SECTOR_SIZE + 92])
        if hdr[0:8] != b"EFI PART":
            print(f"  ERROR: Invalid {label} GPT signature")
            return False
        stored_crc = struct.unpack("<I", hdr[16:20])[0]
        hdr[16:20] = b"\x00\x00\x00\x00"
        calc_crc = zlib.crc32(bytes(hdr)) & 0xFFFFFFFF
        if calc_crc != stored_crc:
            print(f"  ERROR: {label} GPT header CRC mismatch ({calc_crc:08x} vs {stored_crc:08x})")
            return False
        print(f"  [OK] {label} GPT header valid")

    # Partition Entry Arrays
    pe_primary = blob[PRIMARY_ENTRIES_LBA * SECTOR_SIZE : PRIMARY_ENTRIES_LBA * SECTOR_SIZE + 16384]
    pe_backup = blob[BACKUP_ENTRIES_LBA * SECTOR_SIZE : BACKUP_ENTRIES_LBA * SECTOR_SIZE + 16384]
    stored_pe_crc = struct.unpack("<I", blob[512 + 88 : 512 + 92])[0]
    if (zlib.crc32(pe_primary) & 0xFFFFFFFF) != stored_pe_crc or pe_primary != pe_backup:
        print("  ERROR: GPT partition entry array CRC or mirror mismatch")
        return False
    print("  [OK] Primary and Backup GPT partition entries valid")

    # Verify FAT32 BPB and files on Partition 1 (ESP)
    esp_vbr = blob[PARTITION_1_START * SECTOR_SIZE : PARTITION_1_START * SECTOR_SIZE + 512]
    if esp_vbr[510:512] != b"\x55\xAA" or esp_vbr[82:90] != b"FAT32   ":
        print("  ERROR: Invalid FAT32 BPB on MAGMA_BOOT")
        return False

    efi_res = _find_fat32_entry(blob, PARTITION_1_START, 2, b"EFI        ")
    if not efi_res:
        print("  ERROR: /EFI not found on MAGMA_BOOT")
        return False
    efi_boot_res = _find_fat32_entry(blob, PARTITION_1_START, efi_res[0], b"BOOT       ")
    if not efi_boot_res:
        print("  ERROR: /EFI/BOOT not found on MAGMA_BOOT")
        return False
    bootx64_res = _find_fat32_entry(blob, PARTITION_1_START, efi_boot_res[0], b"BOOTX64 EFI")
    if not bootx64_res:
        print("  ERROR: /EFI/BOOT/BOOTX64.EFI not found on MAGMA_BOOT")
        return False
    expected_efi_size = os.path.getsize("boot/limine/BOOTX64.EFI")
    if bootx64_res[1] != expected_efi_size:
        print(f"  ERROR: BOOTX64.EFI size mismatch ({bootx64_res[1]} vs {expected_efi_size})")
        return False
    print(f"  [OK] /EFI/BOOT/BOOTX64.EFI verified ({bootx64_res[1]} bytes)")

    limine_res = _find_fat32_entry(blob, PARTITION_1_START, 2, b"LIMINE~1CON")
    expected_conf_size = os.path.getsize("boot/limine.conf")
    if not limine_res or limine_res[1] != expected_conf_size:
        print("  ERROR: /limine.conf missing or size mismatch on MAGMA_BOOT")
        return False
    print(f"  [OK] /limine.conf verified ({limine_res[1]} bytes)")

    boot_dir_res = _find_fat32_entry(blob, PARTITION_1_START, 2, b"BOOT       ")
    if not boot_dir_res:
        print("  ERROR: /boot not found on MAGMA_BOOT")
        return False
    kernel_res = _find_fat32_entry(blob, PARTITION_1_START, boot_dir_res[0], b"MAGMA_OS   ")
    expected_kernel_size = os.path.getsize("target/x86_64-unknown-none/debug/magma_os")
    if not kernel_res or kernel_res[1] != expected_kernel_size:
        print("  ERROR: /boot/magma_os missing or size mismatch on MAGMA_BOOT")
        return False
    print(f"  [OK] /boot/magma_os verified ({kernel_res[1]} bytes)")

    # Verify FAT32 BPB and files on Partition 3 (RECOVERY)
    rec_vbr = blob[PARTITION_3_START * SECTOR_SIZE : PARTITION_3_START * SECTOR_SIZE + 512]
    if rec_vbr[510:512] != b"\x55\xAA" or rec_vbr[82:90] != b"FAT32   ":
        print("  ERROR: Invalid FAT32 BPB on MAGMA_RECOVERY")
        return False
    syscfg_res = _find_fat32_entry(blob, PARTITION_3_START, 2, b"SYSTEM  CFG")
    recelf_res = _find_fat32_entry(blob, PARTITION_3_START, 2, b"RECOVERYELF")
    if not syscfg_res or not recelf_res:
        print("  ERROR: SYSTEM.CFG or RECOVERY.ELF missing on MAGMA_RECOVERY")
        return False
    print(f"  [OK] MAGMA_RECOVERY verified (SYSTEM.CFG={syscfg_res[1]} B, RECOVERY.ELF={recelf_res[1]} B)")

    return True


if __name__ == "__main__":
    output_file = sys.argv[1] if len(sys.argv) > 1 else "target/magma_disk.img"
    try:
        build_disk_image(output_file)
        if not verify_image(output_file):
            print("[VERIFY FAILED]")
            sys.exit(1)
        print("\n[VERIFY OK]")
        sys.exit(0)
    except Exception as e:
        print(f"ERROR: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)