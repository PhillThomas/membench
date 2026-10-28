//! RAM population.
//!
//! Three sources, best first (PLAN.md detection table):
//!
//! 1. **SMBIOS type 17** (`/sys/firmware/dmi/tables/DMI`) — slot, size, part
//!    number, speeds. Field offsets are taken from DMTF DSP0134 (7.18), not from
//!    folklore. Needs root on most kernels.
//! 2. **EDAC** (`/sys/devices/system/edac/mc*/dimm*`) — labels and sizes when the
//!    board has ECC and the driver is loaded.
//! 3. **SPD EEPROMs** (`/sys/bus/i2c/drivers/ee1004|spd5118/*/eeprom`) — the
//!    kernel tells us the generation by which driver is bound; the rated timings
//!    are decoded from the DDR4 layout (JESD400 offsets, as implemented by
//!    U-Boot's `ddr_spd.h`) and are always labelled *rated*, never "running".
//!
//! Running timings live in memory-controller registers that are undocumented per
//! vendor, so this module never guesses them: unavailable fields say Unknown.

use crate::result::{CpuInfo, Dimm, MemoryInfo, SpdTimings};
use crate::sysinfo::{dir_entries, exists, read_bytes, read_to_string, total_ram_bytes};

const DMI_PATH: &str = "/sys/firmware/dmi/tables/DMI";

pub fn detect(_cpu: &CpuInfo) -> MemoryInfo {
    let mut info = MemoryInfo {
        total_bytes: total_ram_bytes(),
        transparent_hugepages: super::transparent_hugepages_setting(),
        ..Default::default()
    };

    let (modules, note) = smbios_type17();
    info.modules = modules;
    if info.modules.is_empty() {
        info.modules = edac_modules();
    }
    info.populated_modules = (!info.modules.is_empty()).then_some(info.modules.len());
    info.access_note = note;

    info.spd = spd_modules();

    // Memory type: SMBIOS is authoritative, the bound SPD driver is next, and
    // nothing at all is reported as Unknown.
    if let Some(t) = info
        .modules
        .iter()
        .find_map(|d| d.device_type.clone())
        .filter(|t| !t.starts_with("Unknown"))
    {
        info.mem_type = Some(t.clone());
        info.mem_type_source = Some("SMBIOS type 17".to_string());
    } else if let Some(gen) = info.spd.iter().find_map(|s| s.generation.clone()) {
        info.mem_type = Some(gen.clone());
        info.mem_type_source = Some("SPD driver binding".to_string());
    }

    info.speed_mts = info.modules.iter().filter_map(|d| d.max_speed_mts).max();
    info.configured_speed_mts = info
        .modules
        .iter()
        .filter_map(|d| d.configured_speed_mts)
        .min();

    info.channels = None;
    info.channels_note = Some(channel_note(&info));

    info
}

/// Channel count is only knowable from the memory controller, which needs
/// vendor-specific register access. Say what we do know instead of guessing.
fn channel_note(info: &MemoryInfo) -> String {
    let modules = info.populated_modules.unwrap_or(0);
    if modules == 0 {
        return "module count unknown, so channel count is unknown".to_string();
    }
    format!(
        "{modules} module(s) populated; channel count needs memory-controller access (not \
         readable), so it is left Unknown — a dual-channel board usually shows 2 or 4 modules"
    )
}

// ---------------------------------------------------------------------------
// SMBIOS
// ---------------------------------------------------------------------------

/// One raw SMBIOS record: the formatted area plus its string table.
#[derive(Debug)]
struct SmbiosRecord {
    ty: u8,
    length: usize,
    data: Vec<u8>,
    strings: Vec<String>,
}

/// Walk the raw SMBIOS area. Every record is `type, length, handle(2)` followed
/// by `length - 4` more bytes and then a NUL-terminated string area ending in an
/// extra NUL.
fn smbios_records(blob: &[u8]) -> Vec<SmbiosRecord> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= blob.len() {
        let ty = blob[i];
        let length = blob[i + 1] as usize;
        if length < 4 || i + length > blob.len() {
            break; // truncated or corrupt: stop rather than read nonsense
        }
        let mut strings = Vec::new();
        let mut p = i + length;
        while p < blob.len() {
            if blob[p] == 0 {
                break; // the terminating double NUL
            }
            let start = p;
            while p < blob.len() && blob[p] != 0 {
                p += 1;
            }
            strings.push(String::from_utf8_lossy(&blob[start..p]).trim().to_string());
            p += 1;
        }
        // Advance past the string area and its final NUL.
        let next = p.saturating_add(1).max(i + 4);
        out.push(SmbiosRecord {
            ty,
            length,
            data: blob[i..i + length].to_vec(),
            strings,
        });
        if next <= i {
            break; // no progress: bail out
        }
        i = next;
    }
    out
}

/// `None` = could not read the tables. The reason differs for root and non-root
/// and the panel must say which, because "run with sudo" is bad advice when the
/// kernel blocked root too.
fn smbios_type17() -> (Vec<Dimm>, Option<String>) {
    if !exists(DMI_PATH) {
        return (
            Vec::new(),
            Some("no SMBIOS access: this kernel exposes no DMI tables".into()),
        );
    }
    let blob = match read_bytes(DMI_PATH, 1 << 20) {
        Some(b) => b,
        None => {
            let note = if crate::sysinfo::is_root() {
                "no SMBIOS access: /sys/firmware/dmi/tables/DMI is unreadable even for root on \
                 this kernel"
            } else {
                "run with sudo for DIMM details (SMBIOS tables are root-only)"
            };
            return (Vec::new(), Some(note.to_string()));
        }
    };
    let records = smbios_records(&blob);
    let mut out = Vec::new();
    for r in records.iter().filter(|r| r.ty == 17) {
        if let Some(d) = decode_memory_device(r) {
            out.push(d);
        }
    }
    if out.is_empty() {
        return (
            Vec::new(),
            Some("SMBIOS readable but it lists no populated memory devices".to_string()),
        );
    }
    (out, None)
}

fn u16(r: &SmbiosRecord, off: usize) -> Option<u32> {
    if off + 2 > r.length {
        return None;
    }
    Some(u16::from_le_bytes([r.data[off], r.data[off + 1]]) as u32)
}

fn u32v(r: &SmbiosRecord, off: usize) -> Option<u64> {
    if off + 4 > r.length {
        return None;
    }
    Some(u32::from_le_bytes([
        r.data[off],
        r.data[off + 1],
        r.data[off + 2],
        r.data[off + 3],
    ]) as u64)
}

fn byte(r: &SmbiosRecord, off: usize) -> Option<u32> {
    if off >= r.length {
        return None;
    }
    Some(r.data[off] as u32)
}

/// String index → text. Index 0 means "not present".
fn string(r: &SmbiosRecord, off: usize) -> Option<String> {
    let idx = byte(r, off)? as usize;
    if idx == 0 {
        return None;
    }
    r.strings
        .get(idx - 1)
        .filter(|s| !s.is_empty() && !s.starts_with("NO"))
        .cloned()
}

fn decode_memory_device(r: &SmbiosRecord) -> Option<Dimm> {
    // DSP0134 7.18: Size at 0x0Ch. 0xFFFF = unknown, 0x7FFF = extended size
    // field, bit 15 set = value is in MB, otherwise KB.
    let size_field = u16(r, 0x0C)?;
    let size_bytes = match size_field {
        0xFFFF => None,
        0x7FFF => u32v(r, 0x1C).map(|mb| mb * 1024 * 1024), // Extended Size, MB
        v if v & 0x8000 != 0 => Some((v & 0x7FFF) as u64 * 1024 * 1024),
        v => Some(v as u64 * 1024),
    };
    // Empty slots are not interesting for a benchmark, and counting them as
    // modules would misreport the population.
    if size_field == 0 || size_bytes.unwrap_or(0) == 0 {
        return None;
    }

    let speed = u16(r, 0x15).filter(|v| *v != 0 && *v != 0xFFFF);
    let configured = u16(r, 0x20).filter(|v| *v != 0 && *v != 0xFFFF);
    let type_detail = u16(r, 0x13).unwrap_or(0);
    let mut device_type = memory_type_name(byte(r, 0x12).unwrap_or(0));

    // Registered / unbuffered is a bit in Type Detail, not a separate field.
    if type_detail & (1 << 13) != 0 {
        device_type = format!("{device_type} (registered)");
    } else if type_detail & (1 << 14) != 0 {
        device_type = format!("{device_type} (unbuffered)");
    }

    Some(Dimm {
        slot: string(r, 0x10),
        bank: string(r, 0x11),
        size_bytes,
        form_factor: form_factor_name(byte(r, 0x0E).unwrap_or(0)),
        device_type: Some(device_type),
        max_speed_mts: speed,
        configured_speed_mts: configured,
        part_number: string(r, 0x1A),
        manufacturer: string(r, 0x17),
        serial: string(r, 0x18),
        data_width_bits: u16(r, 0x0A).filter(|v| *v != 0xFFFF),
        total_width_bits: u16(r, 0x08).filter(|v| *v != 0xFFFF),
        // Attributes byte, bits 3-0 = rank number, 0 = unknown.
        ranks: byte(r, 0x1B).map(|v| v & 0x0F).filter(|v| *v != 0),
        source: "SMBIOS type 17".to_string(),
    })
}

/// DSP0134 Table 78 (Memory Device: Type). Anything outside this list is
/// reported as unknown with its raw value rather than being mapped to the
/// nearest guess.
fn memory_type_name(v: u32) -> String {
    let name = match v {
        0x01 => "Other",
        0x02 => "Unknown",
        0x03 => "DRAM",
        0x07 => "RAM",
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile device",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return format!("Unknown (code {v:#04X})"),
    };
    name.to_string()
}

/// DSP0134 7.18.1. Only the codes this project has verified against the spec
/// text are named; the rest keep their raw value visible.
fn form_factor_name(v: u32) -> Option<String> {
    let name = match v {
        0x01 => "Other",
        0x02 => "Unknown",
        0x09 => "DIMM",
        0x0D => "SODIMM",
        0x0F => "FB-DIMM",
        _ => return None,
    };
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// EDAC
// ---------------------------------------------------------------------------

/// `dimm*` directories under `/sys/devices/system/edac/mc*`.
fn edac_modules() -> Vec<Dimm> {
    let mut out = Vec::new();
    for mc in dir_entries("/sys/devices/system/edac/mc") {
        if !mc.starts_with("mc") || !mc[2..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let base = format!("/sys/devices/system/edac/mc/{mc}");
        for dimm in dir_entries(&base) {
            if !dimm.starts_with("dimm") {
                continue;
            }
            let dir = format!("{base}/{dimm}");
            let label = read_to_string(&format!("{dir}/dimm_label"));
            let size_mb = read_to_string(&format!("{dir}/dimm_size"))
                .and_then(|s| s.trim_end_matches(" MB").trim().parse::<u64>().ok());
            if size_mb.is_none() && label.is_none() {
                continue;
            }
            out.push(Dimm {
                slot: label.clone().or_else(|| Some(format!("{mc}/{dimm}"))),
                bank: Some(format!("{mc} controller")),
                size_bytes: size_mb.map(|mb| mb * 1024 * 1024),
                source: "EDAC".to_string(),
                ..Default::default()
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SPD
// ---------------------------------------------------------------------------

/// Kernel drivers that mean "an SPD EEPROM is wired up and readable".
const SPD_DRIVERS: [(&str, &str); 2] = [("ee1004", "DDR4"), ("spd5118", "DDR5")];

fn spd_modules() -> Vec<SpdTimings> {
    let mut out = Vec::new();
    for (driver, generation) in SPD_DRIVERS {
        let base = format!("/sys/bus/i2c/drivers/{driver}");
        if !exists(&base) {
            continue;
        }
        for dev in dir_entries(&base) {
            // Device directories look like `0-0052`; skip `bind`, `uevent`, …
            if !dev.contains('-')
                || !dev[..dev.find('-').unwrap_or(0)]
                    .chars()
                    .all(|c| c.is_ascii_digit())
            {
                continue;
            }
            let path = format!("{base}/{dev}/eeprom");
            let Some(bytes) = read_bytes(&path, 1024) else {
                continue;
            };
            out.push(decode_spd(generation, &dev, &bytes));
        }
    }
    out
}

/// Decode what the SPD layout can honestly give us.
///
/// DDR4 byte numbers follow JESD400 as implemented by U-Boot's `ddr_spd.h`:
/// byte 1 is the key byte (0x12 = DDR4 SDRAM), byte 3 the module type, byte 17
/// the time bases, 18 `tCKAVGmin`, 24/25/26 tAA/tRCD/tRP minima, 27/28 tRAS.
/// Boards whose firmware never wrote the EEPROM (common on soldered LPDDR and on
/// VMs) are full of zeros: every value then fails a plausibility gate and the
/// result is Unknown, which is the correct answer.
fn decode_spd(generation: &str, device: &str, b: &[u8]) -> SpdTimings {
    let mut t = SpdTimings {
        generation: Some(generation.to_string()),
        device: format!("SPD EEPROM {device} ({generation})"),
        note: "rated values from the SPD EEPROM, not running timings; running timings need \
               memory-controller register access"
            .to_string(),
        ..Default::default()
    };

    if generation != "DDR4" {
        t.note = format!(
            "SPD present; timings for {generation} are not decoded by this build, so they are \
             reported as Unknown"
        );
        return t;
    }
    if b.len() < 46 {
        t.note = "SPD EEPROM read was too short to decode".to_string();
        return t;
    }
    if b[1] != 0x12 {
        t.note = format!(
            "SPD key byte is {:#04X}, not 0x12 (DDR4): the driver binding says {generation} but \
             the contents disagree, so nothing is decoded",
            b[1]
        );
        return t;
    }

    let module_type = match b[3] & 0x0F {
        0x00 => Some("extended module type"),
        0x01 => Some("RDIMM"),
        0x02 => Some("UDIMM"),
        0x03 => Some("SODIMM"),
        0x04 => Some("LRDIMM"),
        0x05 => Some("Mini-RDIMM"),
        0x06 => Some("Mini-UDIMM"),
        0x08 => Some("72b SODIMM"),
        0x09 => Some("72b RDIMM"),
        _ => None,
    };

    // Time bases: byte 17 low nibble is MTB (0 = 1000 ps, 1 = 2500 ps,
    // 2 = 500 ps), high nibble the fine time base. Every DDR4 module in the wild
    // uses MTB = 1000 ps; anything else is treated as unreadable rather than
    // assumed.
    let mtb_ns = match b[17] & 0x0F {
        0 => Some(1.0f64),
        other => {
            let _ = other;
            None
        }
    };
    let Some(mtb) = mtb_ns else {
        if let Some(m) = module_type {
            t.note = format!(
                "{m} module; unsupported SPD time base {:#04X}, timings Unknown",
                b[17]
            );
        }
        return t;
    };

    let taa = plausible(b[24] as f64 * mtb, 5.0, 40.0);
    let trcd = plausible(b[25] as f64 * mtb, 5.0, 40.0);
    let trp = plausible(b[26] as f64 * mtb, 5.0, 40.0);
    let tras = plausible(
        (b[28] as f64 + ((b[27] & 0x0F) as f64) * 256.0) * mtb,
        15.0,
        120.0,
    );
    let tck = plausible(b[18] as f64 * mtb, 0.3, 3.2);

    t.tAA_ns = taa.map(|v| v as u32);
    t.tRCD_ns = trcd.map(|v| v as u32);
    t.tRP_ns = trp.map(|v| v as u32);
    t.tRAS_ns = tras.map(|v| v as u32);

    if let (Some(taa), Some(trcd), Some(trp), Some(tras), Some(tck)) = (taa, trcd, trp, tras, tck) {
        let cycles = |ns: f64| (ns / tck).round() as u32;
        t.cas_string = Some(format!(
            "{}-{}-{}-{}",
            cycles(taa),
            cycles(trcd),
            cycles(trp),
            cycles(tras)
        ));
        let mut note = format!(
            "rated SPD timings at tCKAVGmin = {:.3} ns ({} MT/s); these are the module's \
             published minima, not the values the controller is running",
            tck,
            (2000.0 / tck).round() as u32
        );
        if let Some(m) = module_type {
            note = format!("{m} module, {note}");
        }
        t.note = note;
    } else {
        let mut note = "SPD EEPROM is present but its timing fields are blank or implausible \
                        (firmware did not populate them), so timings are Unknown"
            .to_string();
        if let Some(m) = module_type {
            note = format!("{m} module, {note}");
        }
        t.note = note;
    }
    t
}

/// Reject values outside `lo..=hi` — the gate that keeps a zeroed-out EEPROM
/// from producing a confident-looking wrong answer.
fn plausible(v: f64, lo: f64, hi: f64) -> Option<f64> {
    if v.is_finite() && (lo..=hi).contains(&v) {
        Some(v)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built SMBIOS area with one type 17 record, using only offsets the
    /// spec confirms: this is the only way to exercise the decoder on a machine
    /// whose DMI table is unreadable.
    /// `size_field` is the raw 16-bit Size field: KB, MB with bit 15 set,
    /// 0 for an empty slot, or 0x7FFF to point at the extended field.
    fn fixture_type17(size_field: u16, part: &str, slot: &str) -> Vec<u8> {
        let mut b = Vec::new();
        // header: type, length, handle
        b.push(17u8);
        b.push(0x22); // 34 bytes of formatted area (SMBIOS 2.7)
        b.extend_from_slice(&[0x00, 0x04]); // handle
        b.extend_from_slice(&[0xFF, 0xFF]); // 0x04 physical array handle
        b.extend_from_slice(&[0xFE, 0xFF]); // 0x06 error info handle
        b.extend_from_slice(&[0xC0, 0x00]); // 0x08 total width 192
        b.extend_from_slice(&[0xA0, 0x00]); // 0x0A data width 160
        b.extend_from_slice(&size_field.to_le_bytes()); // 0x0C size
        b.push(0x09); // 0x0E form factor DIMM
        b.push(0x00); // 0x0F device set
        b.push(0x02); // 0x10 device locator -> string 2
        b.push(0x01); // 0x11 bank locator -> string 1
        b.push(0x1A); // 0x12 memory type DDR4
        b.extend_from_slice(&[0x00, 0x40]); // 0x13 type detail: bit 14 unbuffered
        b.extend_from_slice(&[0x00, 0x0D]); // 0x15 speed 3333? (0x0D00) -> see below
        b.push(0x00); // 0x17 manufacturer: absent
        b.push(0x00); // 0x18 serial: absent
        b.push(0x00); // 0x19 asset tag: absent
        b.push(0x03); // 0x1A part number -> string 3
        b.push(0x01); // 0x1B attributes: rank 1
                      // 0x1C..0x21: extended size (4) + configured speed (2). That is the end
                      // of the formatted area: length is 0x22, so anything further here would
                      // be read as the string area and swallow the strings below.
        b.extend_from_slice(&[0x00; 6]);
        // string area
        b.extend_from_slice(b"Bank 0\0");
        b.extend_from_slice(slot.as_bytes());
        b.push(0);
        b.extend_from_slice(part.as_bytes());
        b.push(0);
        b.push(0); // terminating NUL
        b
    }

    #[test]
    fn type17_record_decodes_with_spec_offsets() {
        // 8 GiB expressed in megabytes (bit 15 set), which is the only way a
        // 16-bit field can say it.
        let blob = fixture_type17(0x8000 | 8192, "CMP32GX4M2B3200C16", "DIMM_A1");
        let records = smbios_records(&blob);
        assert_eq!(records.len(), 1, "{records:?}");
        let d = decode_memory_device(&records[0]).expect("decode");
        assert_eq!(d.size_bytes, Some(8 * 1024 * 1024 * 1024));
        assert_eq!(d.part_number.as_deref(), Some("CMP32GX4M2B3200C16"));
        assert_eq!(d.slot.as_deref(), Some("DIMM_A1"));
        assert_eq!(d.bank.as_deref(), Some("Bank 0"));
        assert_eq!(d.form_factor.as_deref(), Some("DIMM"));
        assert_eq!(d.device_type.as_deref(), Some("DDR4 (unbuffered)"));
        assert_eq!(d.total_width_bits, Some(192));
        assert_eq!(d.data_width_bits, Some(160));
        assert_eq!(d.ranks, Some(1));
        assert_eq!(d.source, "SMBIOS type 17");
    }

    #[test]
    fn empty_slots_are_skipped_and_mb_flag_is_honoured() {
        // Size 0 = empty slot.
        let blob = fixture_type17(0, "X", "DIMM_A2");
        assert!(decode_memory_device(&smbios_records(&blob)[0]).is_none());

        // Size with bit 15 set = megabytes (the flag is the high byte, which
        // `fixture_type17` already encodes little-endian).
        let blob = fixture_type17(0x8000 | 16, "X", "DIMM_A2");
        let d = decode_memory_device(&smbios_records(&blob)[0]).expect("decode");
        assert_eq!(d.size_bytes, Some(16 * 1024 * 1024));
    }

    #[test]
    fn truncated_and_garbage_tables_are_survivable() {
        assert!(smbios_records(&[]).is_empty());
        assert!(smbios_records(&[17]).is_empty());
        assert!(smbios_records(&[17, 200, 0, 0]).is_empty()); // length past the end
                                                              // A record claiming length 0 must not spin forever.
        assert!(smbios_records(&[17, 0, 0, 0, 0]).is_empty());
        // Random bytes: must terminate and must not panic.
        let junk: Vec<u8> = (0..512u32).map(|i| (i * 7 % 251) as u8).collect();
        let _ = smbios_records(&junk);
    }

    #[test]
    fn memory_type_names_follow_the_spec_table() {
        assert_eq!(memory_type_name(0x1A), "DDR4");
        assert_eq!(memory_type_name(0x22), "DDR5");
        assert_eq!(memory_type_name(0x18), "DDR3");
        assert_eq!(memory_type_name(0x02), "Unknown");
        assert_eq!(memory_type_name(0x77), "Unknown (code 0x77)");
        assert_eq!(form_factor_name(0x09).as_deref(), Some("DIMM"));
        assert_eq!(
            form_factor_name(0x03),
            None,
            "unverified code stays unnamed"
        );
    }

    #[test]
    fn spd_decoder_accepts_a_realistic_ddr4_blob() {
        // 16 GiB DDR4-3200 UDIMM: tCKAVGmin 0.625 ns is below the 1 ns MTB, so
        // firmware reports 1 ns rounded minima: tAA 14, tRCD 15, tRP 15, tRAS 32.
        let mut b = vec![0u8; 256];
        b[1] = 0x12; // key byte: DDR4
        b[3] = 0x02; // UDIMM
        b[17] = 0x00; // MTB = 1000 ps
        b[18] = 1; // tCKAVGmin = 1 ns with MTB granularity
        b[24] = 14;
        b[25] = 15;
        b[26] = 15;
        b[27] = 0x00; // tRAS high nibble
        b[28] = 32; // tRAS low byte
        let t = decode_spd("DDR4", "0-0052", &b);
        assert_eq!(t.tAA_ns, Some(14));
        assert_eq!(t.tRAS_ns, Some(32));
        assert_eq!(t.cas_string.as_deref(), Some("14-15-15-32"));
        assert!(t.note.contains("UDIMM"), "{}", t.note);
        assert!(t.note.contains("not the values"), "{}", t.note);
    }

    #[test]
    fn zeroed_spd_is_reported_as_unknown_not_as_zero_timings() {
        // Exactly what this test box's soldered LPDDR reports: a DDR4 key byte
        // and nothing else.
        let mut b = vec![0u8; 256];
        b[1] = 0x12;
        let t = decode_spd("DDR4", "0-0052", &b);
        assert_eq!(t.tAA_ns, None);
        assert_eq!(t.cas_string, None);
        assert!(
            t.note.contains("Unknown") || t.note.contains("implausible"),
            "{}",
            t.note
        );
        assert_eq!(t.generation.as_deref(), Some("DDR4"));
    }

    #[test]
    fn spd_decoder_refuses_a_mismatched_key_byte() {
        let b = vec![0xAAu8; 256];
        let t = decode_spd("DDR4", "0-0052", &b);
        assert_eq!(t.cas_string, None);
        assert!(t.note.contains("disagree"), "{}", t.note);
    }

    #[test]
    fn ddr5_spd_is_reported_without_invented_timings() {
        let b = vec![0x12u8; 256];
        let t = decode_spd("DDR5", "0-0051", &b);
        assert_eq!(t.generation.as_deref(), Some("DDR5"));
        assert_eq!(t.cas_string, None);
        assert!(t.note.contains("not decoded"), "{}", t.note);
    }

    #[test]
    fn detect_on_this_machine_is_honest() {
        let cpu = crate::sysinfo::cpu::detect();
        let m = detect(&cpu);
        assert!(
            m.total_bytes.is_some() || crate::sysinfo::read_to_string("/proc/meminfo").is_none()
        );
        assert!(m.populated_modules.unwrap_or(0) == m.modules.len());
        // Whatever we found, the channel count is never invented.
        assert_eq!(m.channels, None);
        assert!(m.channels_note.is_some());
        for s in &m.spd {
            assert!(!s.device.is_empty());
            assert!(!s.note.is_empty());
        }
    }
}
