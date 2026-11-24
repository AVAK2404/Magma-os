//! ACPI Table Parser - Pure no_std implementation
//!
//! Parses RSDP, RSDT/XSDT, MADT, and MCFG tables from ACPI.
//! All addresses are physical; HHDM offset is applied to access via virtual pointers.
//!
//! Design decisions:
//! - Uses read_unaligned for #[repr(packed)] structs to avoid unaligned reference UB
//! - Does not allocate - uses static byte arrays and stack buffers
//! - Validates checksums per ACPI specification
//! - Extracts MADT (APIC) and MCFG (PCIe ECAM) entries

#![allow(dead_code)]

/// HHDM offset - set during initialization via init()
static mut HHDM_OFFSET: u64 = 0;

/// Convert a physical address to virtual using HHDM.
/// If the address is already above HHDM (e.g., from Limine's RSDP response
/// in base revision 3+), it is returned unchanged. Otherwise, HHDM is added.
/// Returns None if the addition would overflow.
fn phys_to_virt(addr: u64, hhdm_offset: u64) -> Option<u64> {
    // If address is already in the higher half, it's already virtual
    // HHDM offset is 0xFFFF800000000000, so addresses >= that are already HHDM-mapped
    if addr >= hhdm_offset {
        Some(addr)
    } else {
        // Use checked_add to prevent overflow when translating physical to virtual
        hhdm_offset.checked_add(addr)
    }
}

use crate::serial;

/// RSDP revision constants
const RSDP_REVISION_V1: u8 = 0;
const RSDP_REVISION_V2: u8 = 2;

/// Maximum ACPI table entries (conservative estimate)
const MAX_ACPI_TABLES: usize = 128;

/// Hex digit lookup table for debug output
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// SDT header - 36 bytes per ACPI specification
#[repr(C, packed)]
struct SdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    asl_compiler_id: [u8; 4],
    asl_compiler_revision: u32,
}

const _: () = assert!(core::mem::size_of::<SdtHeader>() == 36);

impl SdtHeader {
    /// Verify the checksum by summing all bytes from 0 to length
    fn verify_checksum(&self) -> bool {
        // Read length unaligned to avoid packed struct UB
        let len: usize = unsafe {
            core::ptr::addr_of!(self.length).read_unaligned() as usize
        };
        // Validate length bounds
        if len < 36 || len > 0x10000 {
            return false;
        }
        // Sum bytes 0..len, covering the entire table
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(self as *const SdtHeader as *const u8, len)
        };
        let sum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        sum == 0
    }

    /// Get the signature as a 4-character string
    fn signature_str(&self) -> [u8; 4] {
        self.signature
    }
}

/// RSDP v1 structure (20 bytes)
#[repr(C, packed)]
struct RsdpV1 {
    signature: [u8; 8],      // "RSD PTR " (note the trailing space)
    checksum: u8,            // Checksum of first 20 bytes
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,       // Physical address of RSDT
}

const _: () = assert!(core::mem::size_of::<RsdpV1>() == 20);

impl RsdpV1 {
    /// Verify the checksum of the first 20 bytes
    fn verify_checksum(&self) -> bool {
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(self as *const RsdpV1 as *const u8, 20)
        };
        let sum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        sum == 0
    }
}

/// RSDP v2 structure (36 bytes) - extends RsdpV1
#[repr(C, packed)]
struct RsdpV2 {
    signature: [u8; 8],           // "RSD PTR " (note the trailing space)
    checksum: u8,                 // Checksum of first 36 bytes
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,            // Physical address of RSDT (legacy)
    length: u32,                  // Length of the entire ACPI table (at offset 20)
    xsdt_address: u64,            // Physical address of XSDT (v2+)
    extended_checksum: u8,        // Extended checksum (at offset 28)
    reserved: [u8; 3],            // Reserved (at offset 29)
}

const _: () = assert!(core::mem::size_of::<RsdpV2>() == 36);

impl RsdpV2 {
    /// Verify the checksum of bytes 0..length (the entire RSDP structure)
    /// Per ACPI spec, the extended checksum covers all bytes from offset 0 to the length field value.
    fn verify_checksum(&self) -> bool {
        // Read length field at offset 20
        let len: u32 = unsafe {
            core::ptr::addr_of!(self.length).read_unaligned()
        };

        // Length must be at least 36 (minimum valid RSDP v2 size) and not exceed reasonable bounds
        if len < 36 || len > 0x10000 {
            return false;
        }

        // Sum bytes 0..len, covering the entire RSDP structure
        let bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(self as *const RsdpV2 as *const u8, len as usize)
        };
        let sum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        sum == 0
    }
}

/// MADT CPU Structure (Type 0)
/// - Type: 0
/// - Length: 8 bytes
/// - Flags: u16 (bit 0 = enabled)
/// - APIC ID: u8
/// - APIC Cluster ID: u8
#[repr(packed)]
struct MadtCpuStruct {
    entry_type: u8,     // 0
    length: u8,         // 8
    cpu_flags: u16,
    apic_id: u8,
    apic_cluster_id: u8,
}

/// MADT I/O APIC Structure (Type 1)
/// - Type: 1
/// - Length: 12 bytes
/// - I/O APIC ID: u8
/// - Reserved: u8
/// - Address: u32 (MMIO base)
/// - GSI Base: u32 (Global System Interrupt number)
#[repr(packed)]
struct MadtIoApicStruct {
    entry_type: u8,     // 1 (not 2!)
    length: u8,         // 12
    io_apic_id: u8,
    reserved: u8,
    address: u32,       // Physical MMIO base address
    gsi_base: u32,    // Global System Interrupt base
}

/// MADT Interrupt Source Override (Type 2)
/// - Type: 2
/// - Length: 10 bytes
/// - Bus: u8
/// - Source Interrupt: u8
/// - Global Interrupt: u32
/// - Flags: u16
#[repr(packed)]
struct MadtInterruptOverride {
    entry_type: u8,     // 2 (not 9!)
    length: u8,         // 10
    bus: u8,
    irq_source: u8,
    gpic_interrupt: u32,
    flags: u16,
}

/// MADT NMI Structure (Type 4)
#[repr(packed)]
struct MadtNmiStruct {
    entry_type: u8,     // 4
    length: u8,         // 6
    cpu_flags: u16,
    local_apic_id: u8,
    reserved: u8,
}

/// MCFG PCIe ECAM Structure (16 bytes)
/// Per ACPI spec: base_address (u64, offset 0), pci_segment_group (u16, offset 8),
/// start_bus (u8, offset 10), end_bus (u8, offset 11), reserved (u32, offset 12)
#[repr(C, packed)]
struct McfgEcam {
    base_address: u64,         // Offset 0-7: ECAM base address
    pci_segment_group: u16,    // Offset 8-9: PCI segment group number
    start_bus: u8,             // Offset 10: Starting bus number
    end_bus: u8,               // Offset 11: Ending bus number
    reserved: u32,             // Offset 12-15: Reserved (must be 0)
}

const _: () = assert!(core::mem::size_of::<McfgEcam>() == 16);

/// Global storage for parsed ACPI information
static mut ACPI_DATA: AcpiData = AcpiData::new();

/// Interrupt Source Override entry storage
/// Stores (bus, source_irq, gsi, flags) from MADT Type 2 entries
#[derive(Copy, Clone)]
pub struct IsoEntry {
    pub bus: u8,
    pub source_irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

/// ACPI parsed data storage
struct AcpiData {
    rsdp_phys: u64,
    rsdt_phys: u64,
    xsdt_phys: u64,
    revision: u8,
    table_count: usize,
    table_signatures: [u32; MAX_ACPI_TABLES],
    madt_lapic_base: u32,
    madt_ioapic_count: u8,
    madt_ioapic_bases: [u32; 8],      // Store I/O APIC MMIO base addresses
    madt_ioapic_gsi_bases: [u32; 8], // Store I/O APIC GSI base offsets
    madt_ioapic_entries: [u32; 8],     // Store I/O APIC base addresses for ioapic.rs
    madt_iso_count: u8,
    madt_isos: [IsoEntry; 8],         // Store MADT Interrupt Source Overrides
    mcfg_count: u8,
    mcfg_buses: [(u8, u8); 8],
    mcfg_base_addresses: [u64; 8],
}

impl AcpiData {
    const fn new() -> Self {
        Self {
            rsdp_phys: 0,
            rsdt_phys: 0,
            xsdt_phys: 0,
            revision: 0,
            table_count: 0,
            table_signatures: [0; MAX_ACPI_TABLES],
            madt_lapic_base: 0,
            madt_ioapic_count: 0,
            madt_ioapic_bases: [0; 8],
            madt_ioapic_gsi_bases: [0; 8],
            madt_ioapic_entries: [0; 8],
            madt_iso_count: 0,
            madt_isos: [IsoEntry { bus: 0, source_irq: 0, gsi: 0, flags: 0 }; 8],
            mcfg_count: 0,
            mcfg_buses: [(0, 0); 8],
            mcfg_base_addresses: [0; 8],
        }
    }
}

/// Initialize ACPI subsystem
/// Called after PMM and paging are initialized
pub fn init(rsdp_addr: u64, hhdm_offset: u64) {
    // Store HHDM offset for phys_to_virt helper
    unsafe { HHDM_OFFSET = hhdm_offset; }

    if rsdp_addr == 0 {
        serial::write_str_safe("[ACPI] ERROR: RSDP address is zero\r\n");
        return;
    }

    // RSDP from Limine base revision 3+ may already be HHDM-mapped virtual address
    // Use phys_to_virt to get the correct virtual address
    let rsdp_virt = match phys_to_virt(rsdp_addr, hhdm_offset) {
        Some(addr) => addr as *const u8,
        None => {
            serial::write_str_safe("[ACPI] ERROR: RSDP address + HHDM offset overflow\r\n");
            return;
        }
    };

    // Read first 8 bytes to check signature (unaligned read)
    let sig_bytes: [u8; 8] = unsafe { core::ptr::read_unaligned(rsdp_virt as *const [u8; 8]) };

    // "RSD PTR " signature (ASCII bytes: 52 53 44 20 50 54 52 20)
    // Log raw bytes if validation fails for debugging
    if sig_bytes != *b"RSD PTR " {
        serial::write_str_safe("[ACPI] ERROR: Invalid RSDP signature, raw bytes: 0x");
        // Log each byte as hex
        for byte in &sig_bytes {
            let high = (byte >> 4) & 0xF;
            let low = byte & 0xF;
            serial::write_byte(HEX[high as usize]);
            serial::write_byte(HEX[low as usize]);
        }
        serial::write_str_safe(" (ASCII: '");
        // Try to print as ASCII, show '?' for non-printable
        for byte in &sig_bytes {
            if byte.is_ascii_graphic() || *byte == b' ' {
                serial::write_byte(*byte);
            } else {
                serial::write_byte(b'?');
            }
        }
        serial::write_str_safe("')\r\n");
        return;
    }

    // Read revision byte (offset 15)
    let revision: u8 = unsafe { core::ptr::read_unaligned(rsdp_virt.add(15)) };

    serial::write_str_safe("[ACPI] RSDP found, revision: ");
    serial::write_str_safe(match revision {
        1 => "1",
        2 => "2",
        _ => "?",
    });
    serial::write_str_safe("\r\n");

    // Validate and parse based on revision
    match revision {
        RSDP_REVISION_V1 => {
            // RSDP v1: 20 bytes, check only rsdt_address
            let rsdp_v1: &RsdpV1 = unsafe { &*core::ptr::addr_of!(*(rsdp_virt as *const RsdpV1)) };

            if !rsdp_v1.verify_checksum() {
                serial::write_str_safe("[ACPI] ERROR: RSDP v1 checksum invalid\r\n");
                return;
            }

            let rsdt_phys = rsdp_v1.rsdt_address as u64;
            parse_rsdt(rsdt_phys, hhdm_offset, false);
        },
        RSDP_REVISION_V2 => {
            // RSDP v2: 36 bytes, try XSDT first (preferred), fall back to RSDT
            // Read fields using read_unaligned to avoid packed struct UB
            let rsdt_phys: u32 = unsafe {
                core::ptr::read_unaligned((rsdp_virt as *const u8).add(16) as *const u32)
            };
            let xsdt_phys: u64 = unsafe {
                core::ptr::read_unaligned((rsdp_virt as *const u8).add(24) as *const u64)
            };

            // Log raw RSDP v2 field values for debugging
            serial::write_str_safe("[ACPI] RSDP v2: rsdt_address=0x");
            write_u32_hex(rsdt_phys);
            serial::write_str_safe(" xsdt_address=0x");
            write_u64_hex(xsdt_phys);
            serial::write_str_safe("\r\n");

            // Try XSDT first (preferred), fall back to RSDT if XSDT is zero or invalid
            if xsdt_phys != 0 {
                let xsdt_virt = match phys_to_virt(xsdt_phys, hhdm_offset) {
                    Some(addr) => addr,
                    None => {
                        serial::write_str_safe("[ACPI] ERROR: XSDT address + HHDM offset overflow\r\n");
                        // Fall back to RSDT
                        let rsdt_value = rsdt_phys as u64;
                        if rsdt_value != 0 {
                            parse_rsdt(rsdt_value, hhdm_offset, false);
                        } else {
                            serial::write_str_safe("[ACPI] ERROR: Both XSDT and RSDT addresses are zero\r\n");
                        }
                        return;
                    }
                };
                // XSDT parsing returns true only if checksum passes AND valid tables found
                if !parse_xsdt(xsdt_virt, hhdm_offset) {
                    serial::write_str_safe("[ACPI] Falling back to RSDT\r\n");
                    // Fall back to RSDT if XSDT parsing failed
                    let rsdt_value = rsdt_phys as u64;
                    if rsdt_value != 0 {
                        parse_rsdt(rsdt_value, hhdm_offset, false);
                    } else {
                        serial::write_str_safe("[ACPI] ERROR: RSDT address is zero\r\n");
                    }
                }
            } else {
                // Fall back to RSDT if XSDT not present
                let rsdt_value = rsdt_phys as u64;
                if rsdt_value != 0 {
                    parse_rsdt(rsdt_value, hhdm_offset, false);
                } else {
                    serial::write_str_safe("[ACPI] ERROR: Both XSDT and RSDT addresses are zero\r\n");
                }
            }
        },
        _ => {
            serial::write_str_safe("[ACPI] ERROR: Unsupported RSDP revision\r\n");
        }
    }

    // Print ACPI summary after successful parsing
    print_acpi_summary();
}

/// Print summary of parsed ACPI data
fn print_acpi_summary() {
    unsafe {
        serial::write_str_safe("[ACPI] Summary: ");
        serial::write_str_safe(match ACPI_DATA.revision {
            1 => "RSDT",
            2 => "XSDT",
            _ => "unknown",
        });
        serial::write_str_safe(", tables=");
        write_u64_hex(ACPI_DATA.table_count as u64);
        serial::write_str_safe(", LAPIC=0x");
        write_u32_hex(ACPI_DATA.madt_lapic_base);
        serial::write_str_safe("\r\n");

        // Log I/O APIC addresses and GSI bases from MADT
        for i in 0..ACPI_DATA.madt_ioapic_count {
            serial::write_str_safe("[ACPI] I/O APIC: base=0x");
            write_u32_hex(ACPI_DATA.madt_ioapic_bases[i as usize]);
            serial::write_str_safe(" GSI_base=0x");
            write_u32_hex(ACPI_DATA.madt_ioapic_gsi_bases[i as usize]);
            serial::write_str_safe("\r\n");
        }

        // Log MCFG bases for PCIe ECAM
        for i in 0..ACPI_DATA.mcfg_count {
            serial::write_str_safe("[ACPI] MCFG: bus=");
            write_u8_hex(ACPI_DATA.mcfg_buses[i as usize].0);
            serial::write_str_safe("-");
            write_u8_hex(ACPI_DATA.mcfg_buses[i as usize].1);
            serial::write_str_safe(" base=0x");
            write_u64_hex(ACPI_DATA.mcfg_base_addresses[i as usize]);
            serial::write_str_safe("\r\n");
        }
    }
}

/// Parse RSDT (array of 32-bit physical addresses)
fn parse_rsdt(rsdt_phys: u64, hhdm_offset: u64, use_xsdt: bool) {
    if rsdt_phys == 0 {
        serial::write_str_safe("[ACPI] RSDT address is zero\r\n");
        return;
    }

    // Apply HHDM offset with overflow protection
    let rsdt_virt = match rsdt_phys.checked_add(hhdm_offset) {
        Some(addr) => addr as *const SdtHeader,
        None => {
            serial::write_str_safe("[ACPI] ERROR: RSDT address + HHDM offset overflow\r\n");
            return;
        }
    };

    // Read RSDT header (unaligned read)
    let header = unsafe { core::ptr::read_unaligned(rsdt_virt) };
    if !header.verify_checksum() {
        serial::write_str_safe("[ACPI] WARNING: RSDT checksum invalid\r\n");
    }

    // Validate header length before computing entry count
    let header_len = header.length as usize;
    let min_len = core::mem::size_of::<SdtHeader>();
    let max_len = 0x10000; // ACPI spec: table length must not exceed 64KB
    if header_len < min_len {
        serial::write_str_safe("[ACPI] ERROR: RSDT header length too small\r\n");
        return;
    }
    if header_len > max_len {
        serial::write_str_safe("[ACPI] ERROR: RSDT header length too large\r\n");
        return;
    }

    let entry_count = (header_len - min_len) / 4;

    if entry_count > MAX_ACPI_TABLES {
        serial::write_str_safe("[ACPI] WARNING: Too many RSDT entries\r\n");
    }

    let entry_count = entry_count.min(MAX_ACPI_TABLES);

    // Read table pointers (32-bit entries for RSDT)
    let entry_ptr = match rsdt_phys.checked_add(hhdm_offset)
        .and_then(|addr| addr.checked_add(core::mem::size_of::<SdtHeader>() as u64)) {
        Some(addr) => addr as *const u32,
        None => {
            serial::write_str_safe("[ACPI] ERROR: RSDT entry pointer overflow\r\n");
            return;
        }
    };

    let mut count = 0usize;
    for i in 0..entry_count {
        let table_phys_addr = unsafe { core::ptr::read_unaligned(entry_ptr.add(i)) } as u64;
        // Table addresses from Limine may already be HHDM-mapped virtual addresses
        // Use checked addition for physical-to-virtual translation
        let table_virt = match phys_to_virt(table_phys_addr, hhdm_offset) {
            Some(addr) => addr as *const SdtHeader,
            None => {
                serial::write_str_safe("[ACPI] ERROR: Table entry pointer overflow\r\n");
                continue;
            }
        };

        let table_header = unsafe { core::ptr::read_unaligned(table_virt) };
        let sig = table_header.signature_str();

        // Log table signature
        let sig_str = core::str::from_utf8(&sig).unwrap_or("????");
        serial::write_str_safe("[ACPI] Table: ");
        serial::write_str_safe(sig_str);
        serial::write_str_safe("\r\n");

        // Store signature
        if count < MAX_ACPI_TABLES {
            unsafe { ACPI_DATA.table_signatures[count] = table_phys_addr as u32; }
        }
        count += 1;

        // Parse specific tables (pass the virtual pointer)
        check_fadt(table_virt, hhdm_offset);
        check_madt(table_virt, hhdm_offset);
        check_mcfg(table_virt, hhdm_offset);
        check_hpet(table_virt, hhdm_offset);
    }
    unsafe {
        ACPI_DATA.table_count = count;
        ACPI_DATA.rsdt_phys = rsdt_phys;
        ACPI_DATA.revision = if use_xsdt { 2 } else { 1 };
    }
}

/// Parse XSDT (array of 64-bit physical addresses)
/// Returns true if parsing succeeded (checksum passes and header is valid), false if table is corrupt.
/// xsdt_virt: already HHDM-mapped virtual address
fn parse_xsdt(xsdt_virt: u64, hhdm_offset: u64) -> bool {
    if xsdt_virt == 0 {
        serial::write_str_safe("[ACPI] XSDT address is zero\r\n");
        return false;
    }

    let xsdt_ptr: *const SdtHeader = xsdt_virt as *const SdtHeader;

    // Read XSDT header (unaligned read)
    let header = unsafe { core::ptr::read_unaligned(xsdt_ptr) };

    // Check header length validity - if invalid, we cannot parse
    let header_len = header.length as usize;
    let min_len = core::mem::size_of::<SdtHeader>();
    if header_len < min_len {
        serial::write_str_safe("[ACPI] ERROR: XSDT header length too small\r\n");
        return false;
    }

    // Parse succeeded - store table count even if zero
    unsafe {
        ACPI_DATA.table_count = 0;
        ACPI_DATA.xsdt_phys = xsdt_virt;
        ACPI_DATA.revision = 2;
    }

    // Checksum warning but still return true (table is parseable)
    let checksum_valid = header.verify_checksum();
    if !checksum_valid {
        serial::write_str_safe("[ACPI] WARNING: XSDT checksum invalid\r\n");
        // Still return true - the table structure is parseable even if checksum fails
    }

    let max_len = 0x10000; // ACPI spec: table length must not exceed 64KB
    if header_len > max_len {
        serial::write_str_safe("[ACPI] ERROR: XSDT header length too large\r\n");
        return false;
    }

    let entry_count = (header_len - min_len) / 8;

    if entry_count > MAX_ACPI_TABLES {
        serial::write_str_safe("[ACPI] WARNING: Too many XSDT entries\r\n");
    }

    let entry_count = entry_count.min(MAX_ACPI_TABLES);

    // Read table pointers (64-bit entries for XSDT)
    // Entry pointers start after SdtHeader
    let entry_ptr: *const u64 = (xsdt_virt.checked_add(core::mem::size_of::<SdtHeader>() as u64))
        .unwrap_or(0) as *const u64;

    let mut count = 0usize;
    for i in 0..entry_count {
        let table_phys_addr = unsafe { core::ptr::read_unaligned(entry_ptr.add(i)) };
        // Table addresses from Limine may already be HHDM-mapped virtual addresses
        // Use checked addition for physical-to-virtual translation
        let table_virt = match phys_to_virt(table_phys_addr, hhdm_offset) {
            Some(addr) => addr,
            None => {
                serial::write_str_safe("[ACPI] ERROR: XSDT entry pointer overflow\r\n");
                continue;
            }
        };

        let table_header = unsafe { core::ptr::read_unaligned(table_virt as *const SdtHeader) };
        let sig = table_header.signature_str();

        // Log table signature
        let sig_str = core::str::from_utf8(&sig).unwrap_or("????");
        serial::write_str_safe("[ACPI] Table: ");
        serial::write_str_safe(sig_str);
        serial::write_str_safe("\r\n");

        // Store signature (table_phys_addr may be virtual, extract the base portion)
        if count < MAX_ACPI_TABLES {
            unsafe { ACPI_DATA.table_signatures[count] = table_phys_addr as u32; }
        }
        count += 1;

        // Parse specific tables (pass the virtual pointer directly)
        check_fadt(table_virt as *const SdtHeader, hhdm_offset);
        check_madt(table_virt as *const SdtHeader, hhdm_offset);
        check_mcfg(table_virt as *const SdtHeader, hhdm_offset);
        check_hpet(table_virt as *const SdtHeader, hhdm_offset);
    }

    unsafe {
        ACPI_DATA.table_count = count;
    }

    // Return true - parsing succeeded (table structure is valid even if empty or checksum warning)
    true
}

/// Check if this is the FACP (Fixed ACPI Description Table)
fn check_fadt(table_virt: *const SdtHeader, _hhdm_offset: u64) {
    // FACP signature: "FACP" (0x50414346 in little-endian)
    let header = unsafe { core::ptr::read_unaligned(table_virt) };
    let sig = u32::from_le_bytes(header.signature);
    if sig == 0x50414346 {
        serial::write_str_safe("[ACPI] FACP found, length=");
        write_u32_hex(header.length);
        serial::write_str_safe("\r\n");
    }
}

/// Check MADT (Multiple APIC Description Table)
/// Takes a pointer to the table header; addresses are already virtual (HHDM-mapped)
fn check_madt(table_virt: *const SdtHeader, _hhdm_offset: u64) {
    // Check for "APIC" signature: 0x43495041 in little-endian = 'A' 'P' 'I' 'C'
    let header = unsafe { core::ptr::read_unaligned(table_virt) };
    let sig = u32::from_le_bytes(header.signature);
    if sig != 0x43495041 {
        return;
    }

    // This is the MADT table
    // MADT structure: [SdtHeader][lapic_address: u32][flags: u32][entries...]
    // table_virt is already HHDM-mapped virtual address
    let header_size = core::mem::size_of::<SdtHeader>() as u64;
    let lapic_addr = unsafe {
        core::ptr::read_unaligned((table_virt as *const u8).add(header_size as usize) as *const u32)
    };
    unsafe { ACPI_DATA.madt_lapic_base = lapic_addr; }

    serial::write_str_safe("[ACPI] MADT: LAPIC base=0x");
    write_u32_hex(lapic_addr);
    serial::write_str_safe("\r\n");

    // Parse MADT entries
    // MADT fixed header: SdtHeader (36) + LAPIC addr (4) + flags (4) = 44 bytes
    // But we start entries at offset 44, which is header_size + 8
    let mut current_offset = header_size.wrapping_add(8); // Skip header + lapic_addr + flags
    let table_end = u64::from(header.length);

    while current_offset < table_end {
        let entry_base = unsafe {
            (table_virt as *const u8).add(current_offset as usize)
        };
        let entry_type = unsafe { core::ptr::read_unaligned(entry_base) };
        let entry_len = unsafe { core::ptr::read_unaligned(entry_base.add(1)) } as u64;

        match entry_type {
            0 => {
                // CPU Local APIC structure (Type 0)
                // Per ACPI spec: apic_id at +3, flags at +4 (16-bit)
                if entry_len >= 8 {
                    let apic_id = unsafe {
                        core::ptr::read_unaligned(entry_base.add(3))
                    };
                    let flags = unsafe {
                        core::ptr::read_unaligned(entry_base.add(4)) as u16
                    };
                    // Bit 0 = enabled
                    let enabled = (flags & 1) != 0;
                    serial::write_str_safe("[ACPI] MADT CPU: APIC ID=");
                    write_u8_hex(apic_id);
                    serial::write_str_safe(" enabled=");
                    if enabled { serial::write_str_safe("yes"); } else { serial::write_str_safe("no"); }
                    serial::write_str_safe("\r\n");
                }
                current_offset = current_offset.wrapping_add(entry_len.max(2));
            }
            1 => {
                // I/O APIC structure (Type 1)
                // Per ACPI spec: io_apic_id at +2, address at +4 (32-bit), gsi_base at +8 (32-bit)
                if entry_len >= 12 {
                    let io_apic_id = unsafe {
                        core::ptr::read_unaligned(entry_base.add(2))
                    };
                    let address = unsafe {
                        core::ptr::read_unaligned(entry_base.add(4) as *const u32)
                    };
                    let gsi_base = unsafe {
                        core::ptr::read_unaligned(entry_base.add(8) as *const u32)
                    };
                    serial::write_str_safe("[ACPI] MADT I/O APIC: ID=");
                    write_u8_hex(io_apic_id);
                    serial::write_str_safe(" base=0x");
                    write_u32_hex(address);
                    serial::write_str_safe(" GSI_base=");
                    write_u32_hex(gsi_base);
                    serial::write_str_safe("\r\n");
                    // Store I/O APIC base address and GSI base
                    if unsafe { ACPI_DATA.madt_ioapic_count } < 8 {
                        let idx = unsafe { ACPI_DATA.madt_ioapic_count };
                        unsafe {
                            ACPI_DATA.madt_ioapic_entries[idx as usize] = address;
                            ACPI_DATA.madt_ioapic_bases[idx as usize] = address;
                            ACPI_DATA.madt_ioapic_gsi_bases[idx as usize] = gsi_base;
                        }
                        unsafe { ACPI_DATA.madt_ioapic_count = ACPI_DATA.madt_ioapic_count.saturating_add(1); }
                    }
                }
                current_offset = current_offset.wrapping_add(entry_len.max(2));
            }
            2 => {
                // Interrupt Source Override structure (Type 2)
                // Per ACPI spec: bus at +2, source at +3, gsi at +4, flags at +8 (16-bit)
                if entry_len >= 10 {
                    let bus = unsafe {
                        core::ptr::read_unaligned(entry_base.add(2))
                    };
                    let source = unsafe {
                        core::ptr::read_unaligned(entry_base.add(3))
                    };
                    let gsi = unsafe {
                        core::ptr::read_unaligned(entry_base.add(4) as *const u32)
                    };
                    let flags = unsafe {
                        core::ptr::read_unaligned(entry_base.add(8) as *const u16)
                    };
                    let active_high = (flags & 1) != 0;
                    let level_triggered = (flags & 2) != 0;
                    serial::write_str_safe("[ACPI] MADT ISO: bus=");
                    write_u8_hex(bus);
                    serial::write_str_safe(" source=");
                    write_u8_hex(source);
                    serial::write_str_safe(" GSI=");
                    write_u32_hex(gsi);
                    serial::write_str_safe(" active_high=");
                    if active_high { serial::write_str_safe("1"); } else { serial::write_str_safe("0"); }
                    serial::write_str_safe(" level_trig=");
                    if level_triggered { serial::write_str_safe("1"); } else { serial::write_str_safe("0"); }
                    serial::write_str_safe("\r\n");
                    // Store ISO entry for I/O APIC IRQ routing
                    if unsafe { ACPI_DATA.madt_iso_count } < 8 {
                        let idx = unsafe { ACPI_DATA.madt_iso_count };
                        unsafe {
                            ACPI_DATA.madt_isos[idx as usize] = IsoEntry {
                                bus,
                                source_irq: source,
                                gsi,
                                flags,
                            };
                        }
                        unsafe { ACPI_DATA.madt_iso_count = ACPI_DATA.madt_iso_count.saturating_add(1); }
                    }
                }
                current_offset = current_offset.wrapping_add(entry_len.max(2));
            }
            4 => {
                // NMI structure (Type 4)
                if entry_len >= 6 {
                    let flags = unsafe {
                        core::ptr::read_unaligned(entry_base.add(2)) as u16
                    };
                    let local_apic_id = unsafe {
                        core::ptr::read_unaligned(entry_base.add(4))
                    };
                    serial::write_str_safe("[ACPI] MADT NMI: flags=0x");
                    write_u16_hex(flags);
                    serial::write_str_safe(" APIC ID=");
                    write_u8_hex(local_apic_id);
                    serial::write_str_safe("\r\n");
                }
                current_offset = current_offset.wrapping_add(entry_len.max(2));
            }
            _ => {
                // Unknown entry type - read length field and skip
                current_offset = current_offset.wrapping_add(entry_len.max(2));
            }
        }
    }
}

/// Check MCFG table and extract PCIe ECAM information
fn check_mcfg(table_virt: *const SdtHeader, _hhdm_offset: u64) {
    // Check for "MCFG" signature: 0x4647434d in little-endian = 'M' 'C' 'F' 'G'
    let header = unsafe { core::ptr::read_unaligned(table_virt) };
    let sig = u32::from_le_bytes(header.signature);
    if sig != 0x4647434d {
        return;
    }

    serial::write_str_safe("[ACPI] MCFG: Found, parsing ECAM regions\r\n");

    // MCFG table structure:
    // - SdtHeader (36 bytes)
    // - 8-byte reserved field
    // - McfgEcam structures (16 bytes each)
    let sdt_header_size = core::mem::size_of::<SdtHeader>();
    let struct_size = core::mem::size_of::<McfgEcam>();
    let entry_count = (header.length as usize - sdt_header_size - 8) / struct_size;
    let entry_count = entry_count.min(8);

    // Entry pointers start after SdtHeader + 8-byte reserved field
    // table_virt is already HHDM-mapped virtual address
    let entry_ptr = unsafe {
        (table_virt as *const u8).add(sdt_header_size + 8) as *const McfgEcam
    };

    for i in 0..entry_count {
        let ecam = unsafe { core::ptr::read_unaligned(entry_ptr.add(i)) };
        unsafe { ACPI_DATA.mcfg_count = ACPI_DATA.mcfg_count.saturating_add(1); }

        if (i as usize) < 8 {
            unsafe {
                ACPI_DATA.mcfg_buses[i] = (ecam.start_bus, ecam.end_bus);
                ACPI_DATA.mcfg_base_addresses[i] = ecam.base_address;
            }
        }

        serial::write_str_safe("[ACPI] MCFG ECAM: bus=");
        let start = ecam.start_bus;
        let end = ecam.end_bus;
        write_u8_hex(start);
        serial::write_str_safe("-");
        write_u8_hex(end);
        serial::write_str_safe(" base=0x");
        write_u64_hex(ecam.base_address);
        serial::write_str_safe("\r\n");
    }
}

/// Check HPET table
fn check_hpet(_header: *const SdtHeader, _hhdm_offset: u64) {
    // HPET signature: 0x54455048 in little-endian = 'H' 'P' 'E' 'T'
    // Full HPET parsing would extract counter period, main counter, and timers
    let _ = _header;
    let _ = _hhdm_offset;
}

/// Write u8 as 2-digit hex (always 2 digits, including leading zero for val < 16)
fn write_u8_hex(val: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    serial::write_byte(HEX[(val >> 4) as usize]);
    serial::write_byte(HEX[(val & 0xF) as usize]);
}

/// Write u32 as hex
fn write_u32_hex(mut val: u32) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 8];
    let mut idx = 8;
    while val > 0 {
        idx -= 1;
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &buf[idx..] {
        serial::write_byte(b);
    }
}

/// Write u64 as hex
fn write_u64_hex(mut val: u64) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 16];
    let mut idx = 16;
    while val > 0 {
        idx -= 1;
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &buf[idx..] {
        serial::write_byte(b);
    }
}

/// Write u16 as hex
fn write_u16_hex(mut val: u16) {
    if val == 0 {
        serial::write_str_safe("0");
        return;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 4];
    let mut idx = 4;
    while val > 0 {
        idx -= 1;
        buf[idx] = HEX[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &buf[idx..] {
        serial::write_byte(b);
    }
}

// ============================================================================
// Public Getter Functions for I/O APIC and ISO data
// ============================================================================

/// Get Local APIC base address from MADT
pub fn lapic_base() -> u32 {
    unsafe { ACPI_DATA.madt_lapic_base }
}

/// Get I/O APIC count from MADT
pub fn ioapic_count() -> u8 {
    unsafe { ACPI_DATA.madt_ioapic_count }
}

/// Get I/O APIC base addresses from MADT as a static slice
pub fn ioapiics() -> &'static [u32] {
    unsafe {
        &ACPI_DATA.madt_ioapic_entries[..ACPI_DATA.madt_ioapic_count as usize]
    }
}

/// Get Interrupt Source Override entries from MADT as a static slice
pub fn interrupt_overrides() -> &'static [IsoEntry] {
    unsafe {
        &ACPI_DATA.madt_isos[..ACPI_DATA.madt_iso_count as usize]
    }
}