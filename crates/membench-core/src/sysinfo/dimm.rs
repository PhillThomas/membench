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

    let smb = smbios_tables();
    info.error_correction = smb.error_correction;
    info.modules = smb.modules;
    let note = smb.note;
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
/// Where the structures begin inside the DMI blob.
///
/// Two firmwares differ here, which is only visible on real machines. When the
/// blob carries an entry point — a 64-bit `_SM3_` one, then the 32-bit `_SM_`
/// one, per DSP0134 5.1 — the structures start after them. But the kernel's
/// `/sys/firmware/dmi/tables/DMI` file is the *structure area only*: the entry
/// point lives in a separate `smbios_entry_point` file, and `DMI` starts
/// straight at structure 0 (on the reporter's board: `00 1a 00 00 …`, type 0 of
/// 0x1A bytes). So an anchor is skipped when present and not demanded when
/// absent — insisting on one would have broken a table that parses fine.
fn structures_start(blob: &[u8]) -> usize {
    let mut off = 0usize;
    if blob.len() >= 7 && blob[0..5] == *b"_SM3_" {
        let len = blob[6] as usize; // Entry Point Length, 0x18 for the 64-bit EPS
        if len >= 5 && len <= blob.len() {
            off = len;
        }
    }
    if blob.len() >= off + 6 && blob[off..off + 4] == *b"_SM_" {
        // Entry Point Length, normally 0x1F; SMBIOS 2.1 firmware shipped both
        // 0x1E and 0x1F, which is why the spec calls it out.
        let len = blob[off + 5] as usize;
        if len >= 4 && off + len <= blob.len() {
            off += len;
        }
    }
    off
}

fn smbios_records(blob: &[u8]) -> Vec<SmbiosRecord> {
    let mut out = Vec::new();
    let mut i = structures_start(blob);
    while i + 4 <= blob.len() {
        let ty = blob[i];
        let length = blob[i + 1] as usize;
        if length < 4 || i + length > blob.len() {
            break; // truncated or corrupt: stop rather than read nonsense
        }
        let mut strings = Vec::new();
        let mut p = i + length;
        // The unformatted area is a run of NUL-terminated strings closed by a
        // *double* NUL, and a record with no strings is nothing but that double
        // NUL. Stopping at the first NUL — as this did — is one byte short for
        // every string-less record, so each one misaligned everything after it.
        // On the reporter's B450 that meant the walk died after 746 of 1948
        // bytes: the type 17 records at 1277 and 1498 ("P0 CHANNEL A", part
        // CMK32GX4M2A2133C13) were never reached, and the panel said the table
        // listed no populated memory. `dmidecode` scans for the double NUL.
        while p + 1 < blob.len() && (blob[p] != 0 || blob[p + 1] != 0) {
            if blob[p] == 0 {
                p += 1; // a string boundary, not the end of the area
                continue;
            }
            let start = p;
            while p < blob.len() && blob[p] != 0 {
                p += 1;
            }
            strings.push(String::from_utf8_lossy(&blob[start..p]).trim().to_string());
        }
        // `p` is on the first byte of the terminating double NUL.
        let next = p.saturating_add(2).max(i + 4);
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

/// What one read of the SMBIOS tables is worth.
struct Smbios {
    modules: Vec<Dimm>,
    /// `None` = the tables were read. A failure's reason differs for root and
    /// non-root and the panel must say which, because "run with sudo" is bad
    /// advice when the kernel blocked root too.
    note: Option<String>,
    /// Type 16's answer for the physical memory array: what the board does
    /// about bad bits. That is a property of the machine rather than of any one
    /// module, so it sits beside the machine instead of inside a row.
    error_correction: Option<String>,
}

fn smbios_tables() -> Smbios {
    if !exists(DMI_PATH) {
        return Smbios {
            modules: Vec::new(),
            note: Some("no SMBIOS access: this kernel exposes no DMI tables".into()),
            error_correction: None,
        };
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
            return Smbios {
                modules: Vec::new(),
                note: Some(note.to_string()),
                error_correction: None,
            };
        }
    };
    let records = smbios_records(&blob);
    if records.is_empty() {
        return Smbios {
            modules: Vec::new(),
            note: Some(format!(
                "SMBIOS blob is readable ({} bytes) but carries no entry point anchor, so the                  structures could not be located",
                blob.len()
            )),
            error_correction: None,
        };
    }

    // The array is asked first because it answers even when no module does.
    let error_correction = array_error_correction(&records);

    let mut out = Vec::new();
    let mut type17 = 0usize;
    for r in records.iter().filter(|r| r.ty == 17) {
        type17 += 1;
        if let Some(d) = decode_memory_device(r) {
            out.push(d);
        }
    }
    if out.is_empty() {
        // Counts rather than a verdict: whether this is firmware that never
        // fills the table or a machine whose slots really are empty is the
        // reader's call, and the numbers are what makes it.
        return Smbios {
            modules: Vec::new(),
            note: Some(format!(
                "SMBIOS parsed {} structures, {} of them memory devices (type 17), none                  reporting a size — either the firmware leaves them unpopulated or every slot                  is empty",
                records.len(),
                type17
            )),
            error_correction,
        };
    }
    Smbios {
        modules: out,
        note: None,
        error_correction,
    }
}

/// DSP0134 Table 43 (Memory Error Correction Method). The three "None" codes
/// are a definite answer and are kept as one, because "this array corrects
/// nothing" is information. Reserved and unknown codes return `None` so the
/// panel says nothing rather than guessing at a value the firmware invented.
/// Memory Array — Error Correction Types, DSP0134 Table 75: `01h` Other, `02h`
/// Unknown, `03h` None, `04h` Parity, `05h` Single-bit ECC, `06h` Multi-bit
/// ECC, `07h` CRC.
///
/// Transcribed from the specification, which the first version of this function
/// was not: that one was written from memory, put `06h` at "Multi-bit parity"
/// and `03h` at "Multi-bit ECC", and every test written beside it agreed. A
/// Xeon with eight ECC RDIMMs is what found it — the firmware there says `06h`
/// and the invented table translated a machine with real ECC into parity, while
/// a desktop saying `03h`, which means None, came out claiming Multi-bit ECC.
/// A decoder's table is the one place where a confident guess is indistinguishable
/// from knowledge, because nothing downstream can tell that the words underneath
/// a label were never looked up.
///
/// `02h` Unknown and values outside `01h..=07h` are `None`: a field the firmware
/// never implemented is not a statement that nothing protects the array.
fn error_correction_name(v: u32) -> Option<String> {
    match v {
        0x01 => Some("Other".into()),
        0x03 => Some("None".into()),
        0x04 => Some("Parity".into()),
        0x05 => Some("Single-bit ECC".into()),
        0x06 => Some("Multi-bit ECC".into()),
        0x07 => Some("CRC".into()),
        _ => None,
    }
}

/// Type 16, offset 0x06, one entry per Physical Memory Array. The X99 rig
/// publishes two arrays and they agree, so that is one name; where a machine's
/// arrays disagree the answers are joined rather than resolved into one, because
/// choosing between them would be stating something no firmware said. An array
/// that gave no answer contributes nothing, and all-silent is `None`.
fn array_error_correction(records: &[SmbiosRecord]) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    for r in records.iter().filter(|r| r.ty == 16) {
        if let Some(name) = byte(r, 0x06).and_then(error_correction_name) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    (!names.is_empty()).then(|| names.join(" / "))
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
    // DSP0134 7.18: Size at 0x0Ch, 0 = empty slot, 0xFFFF = unknown, 0x7FFF =
    // look in the extended field. The unit bit runs the other way from the
    // intuitive reading, so it is worth quoting the spec's own example: "the
    // value 8100h identifies a 256 KB memory device and 0100h identifies a
    // 256 MB memory device" — bit 15 set means KB, clear means MB. An earlier
    // version of this decoder had it inverted, which would have called a 16 GB
    // module 16 MB, and its own test agreed with the mistake because the
    // fixture was built from the same misreading.
    let size_field = u16(r, 0x0C)?;
    let size_bytes = match size_field {
        0xFFFF => None,
        0x7FFF => u32v(r, 0x1C).map(|mb| mb * 1024 * 1024), // Extended Size, MB
        v if v & 0x8000 != 0 => Some((v & 0x7FFF) as u64 * 1024),
        v => Some(v as u64 * 1024 * 1024),
    };
    // Size 0 means the socket is empty: not interesting for a benchmark, and
    // counting it as a module would misreport the population. An *unknown*
    // size is the other code, 0xFFFF, and stays — empty sockets report 0, so a
    // record that merely cannot say how big it is is still a module, and the
    // row says Unknown rather than vanishing.
    if size_field == 0 {
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

    // ECC is arithmetic on two widths rather than a flag anyone sets: a module
    // 72 bits wide behind 64 bits of data is saying it carries eight check bits.
    // What the board does with those check bits is type 16's answer, kept
    // separately for exactly that reason.
    // Zero is filtered alongside 0xFFFF because firmware does write it, and a
    // width of nothing is a non-answer rather than a narrow bus — and because
    // `ilog2` below has no answer for zero.
    let data_width = u16(r, 0x0A).filter(|v| *v != 0 && *v != 0xFFFF);
    let total_width = u16(r, 0x08).filter(|v| *v != 0 && *v != 0xFFFF);
    let ecc_bits = match (total_width, data_width) {
        (Some(t), Some(d)) if t > d => Some(t - d),
        // Equal widths is usually "no checking" — and usually 64 over 64, which
        // stays zero as it should. But there is no such thing as a 72-bit data
        // path: memory data buses are 8, 16, 32 or 64 bits wide, so firmware
        // that reports 72 over 72 has written the module's total width into
        // both fields instead of answering the question. The Xeon rig this
        // project tests against does exactly that with Samsung ECC RDIMMs, and
        // the plain subtraction called the machine non-ECC. A width that is not
        // one of those buses is carrying check bits, and the gap between it and
        // the widest bus it could have been is how many: 72 means 8, 80 means
        // 16. `dmidecode` prints these fields as they arrive, 72 and 72, and
        // agrees that the arithmetic — not the firmware — is what was missing.
        (Some(t), Some(d)) if t == d && !d.is_power_of_two() => Some(d - (1 << d.ilog2())),
        (Some(_), Some(_)) => Some(0),
        _ => None,
    };

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
        data_width_bits: data_width,
        total_width_bits: total_width,
        ecc_bits,
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
/// DDR4 byte numbers are the SPD byte numbers of JESD403-1, as used by
/// `decode-dimms` and by U-Boot; they are *not* file offsets, and the values
/// below are the ones that matter:
///
/// ```text
///   1  SPD revision, major.minor in BCD — 0x10 is revision 1.0, 0x12 is 1.2
///   2  key byte / fundamental memory type: 12 (0x0C) = DDR4, 14 = DDR4E
///   3  module type, low nibble
///  17  time bases: bits 1:0 and 7:6 must both be zero, which leaves the only
///      combination any shipping module uses: MTB = 125 ps, FTB = 1 ps
///  18  tCKAVGmin in MTB units, with a signed FTB correction at byte 125
///  20..23 supported CAS latencies as a 32-bit little-endian bitmask; the
///      base CL is 23 if byte 23 has bit 7 set, otherwise 7
///  24/25/26 tAAmin/tRCDmin/tRPmin in MTB units, signed FTB corrections at
///      123/122/121
///  27/28 tRASmin as one 12-bit MTB value
/// ```
///
/// Two earlier assumptions here were wrong in a way that hid each other. The
/// key byte is byte 2 and DDR4's value is 12, not `0x12` at byte 1 — byte 1
/// holds the SPD revision, so a revision-1.0 module was refused as corrupt
/// while this machine's own revision-1.2 module passed the check by accident.
/// And the DDR4 time base is 125 ps, not the 1000 ps of DDR2/DDR3: with the
/// old base a 13.75 ns tAAmin decoded as 110 ns, tripped the plausibility
/// gate, and this panel reported a healthy DDR4-3200 module as blank.
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
    // An EEPROM of all zeros is a different failure from one that disagrees
    // with the binding: nothing was ever written to it, or the driver is bound
    // to an address with no module behind it.
    if b[..32].iter().all(|&v| v == 0) {
        t.note = "SPD EEPROM is present but reads as all zeros, so nothing could be decoded \
                  (no table behind the driver binding)"
            .to_string();
        return t;
    }
    if b[2] != SPD_DDR4 && b[2] != SPD_DDR4E {
        t.note = format!(
            "SPD key byte (byte 2) says {} ({:#04X}), not DDR4, although the driver binding says \
             {generation}; nothing is decoded. Byte 1 ({:#04X}) is the SPD revision, not a key \
             byte.",
            spd_memory_type_name(b[2]),
            b[2],
            b[1]
        );
        return t;
    }

    let module_type = match b[3] & 0x0F {
        0x00 => Some("extended module type"),
        0x01 => Some("RDIMM"),
        0x02 => Some("UDIMM"),
        0x03 => Some("SO-DIMM"),
        0x04 => Some("LRDIMM"),
        0x05 => Some("Mini-RDIMM"),
        0x06 => Some("Mini-UDIMM"),
        0x08 => Some("72-bit SO-RDIMM"),
        0x09 => Some("72-bit SO-UDIMM"),
        0x0C => Some("16-bit SO-DIMM"),
        0x0D => Some("32-bit SO-DIMM"),
        0x0F => Some("no base memory"),
        _ => None,
    };
    let prefix = module_type
        .map(|m| format!("{m} module, "))
        .unwrap_or_default();

    // The module's own part number, from the vendor block at bytes 329..348
    // (JESD403-5). SMBIOS usually carries the same string, but SPD is worth
    // reading anyway: it reaches places SMBIOS cannot — a container, for one —
    // and it describes the module rather than the socket. These are the
    // reporter's bytes: they read "CMK32GX4M2A2133C13", what `decode-dimms`
    // prints for the same EEPROM.
    t.part_number = spd_part_number(b);

    // Time bases. There is exactly one valid combination in DDR4, so anything
    // else means the table is not what the layout claims.
    if (b[17] & 0x03) != 0 || (b[17] & 0xC0) != 0 {
        t.note = format!(
            "{prefix}unsupported SPD time base {:#04X}, timings Unknown",
            b[17]
        );
        return t;
    }
    let mtb_ns = MTB_NS_DDR4;
    // The signed 1 ps corrections are bytes 121 to 125. A read that stops short
    // of them loses at most 127 ps per timing, and says so rather than
    // silently treating them as zero.
    let has_fine = b.len() > 125;
    let signed = |v: u8| -> f64 {
        if v & 0x80 != 0 {
            f64::from(v as i8)
        } else {
            f64::from(v)
        }
    };
    let ns = |byte: usize, fine: usize| -> f64 {
        let correction = if has_fine { signed(b[fine]) } else { 0.0 };
        f64::from(b[byte]) * mtb_ns + correction * FTB_NS
    };

    // The rated speed comes from tCKAVGmin alone, so a module whose other
    // fields are blank can still be identified by it.
    //
    // Vendors round that field, so the raw value lands a fraction of a ps off
    // the bin the module is actually rated for: the reporter's kit computes
    // 0.938 ns, which divides to 2132 MT/s, while the modules are labelled and
    // sold as DDR4-2133 (tCKAVGmin = 7.5/8 = 0.9375 ns). Snap to the JEDEC bin
    // when the value is within one fine-time step of it, as `decode-dimms`
    // does, so the panel agrees with the sticker.
    let bin = |mut ctime: f64| -> f64 {
        for n in 7..15u32 {
            let standard = 7.5 / f64::from(n);
            if (ctime - standard).abs() < FTB_NS {
                ctime = standard;
                break;
            }
        }
        ctime
    };
    let tck = plausible(bin(ns(18, 125)), 0.25, 3.2);
    if let Some(tck) = tck {
        t.max_mts = Some((2000.0 / tck) as u32);
    }

    let taa = plausible(ns(24, 123), 5.0, 40.0);
    let trcd = plausible(ns(25, 122), 5.0, 40.0);
    let trp = plausible(ns(26, 121), 5.0, 40.0);
    // tRASmin is a single 12-bit value spread over two bytes, in MTB units.
    let tras_raw = u16::from(b[27] & 0x0F) << 8 | u16::from(b[28]);
    let tras = plausible(f64::from(tras_raw) * mtb_ns, 15.0, 120.0);

    t.tAA_ns = taa.map(|v| v as f32);
    t.tRCD_ns = trcd.map(|v| v as f32);
    t.tRP_ns = trp.map(|v| v as f32);
    t.tRAS_ns = tras.map(|v| v as f32);

    // Supported CAS latencies, independent of the minima above: the bitmask
    // says what the module can do, the minima say what it was tested at.
    let cas_mask =
        (b[20] as u32) | ((b[21] as u32) << 8) | ((b[22] as u32) << 16) | ((b[23] as u32) << 24);
    let cas_base = if b[23] & 0x80 != 0 { 23 } else { 7 };
    let max_cl = (0..30u32)
        .rev()
        .find(|ii| (cas_mask >> ii) & 1 != 0)
        .map(|ii| cas_base + ii);

    match (taa, trcd, trp, tras, tck) {
        (Some(taa), Some(trcd), Some(trp), Some(tras), Some(tck)) => {
            // JEDEC rounds a timing up to the next whole cycle once 25 ps of
            // slack is allowed for, which is what `decode-dimms` does; plain
            // rounding would sometimes print a cycle count the module cannot
            // actually meet.
            let cycles = |ns: f64| ((ns / tck) - 0.025).ceil() as u32;
            // tAA says how many cycles are needed; the bitmask says which
            // latencies the module offers. The CAS number is the smallest
            // offered one that meets the need, so the panel never prints a
            // latency the module does not actually support.
            let cl = |need: u32| -> u32 {
                (0..30u32)
                    .filter(|ii| (cas_mask >> ii) & 1 != 0)
                    .map(|ii| cas_base + ii)
                    .find(|cl| *cl >= need)
                    .unwrap_or(need)
            };
            t.cycles = Some(format!(
                "{}-{}-{}-{}",
                cl(cycles(taa)),
                cycles(trcd),
                cycles(trp),
                cycles(tras)
            ));
            let note = format!(
                "{prefix}rated SPD timings at tCKAVGmin = {:.3} ns ({} MT/s){cl}; these are the \
                 module's published minima, not the values the controller is running{fine}",
                tck,
                (2000.0 / tck) as u32,
                cl = max_cl
                    .map(|c| format!(", supported CAS latencies up to CL{c}"))
                    .unwrap_or_default(),
                fine = if has_fine {
                    String::new()
                } else {
                    "; the read stopped before byte 125, so the 1 ps fine-time corrections are \
                     missing"
                        .to_string()
                }
            );
            t.note = note;
        }
        _ => {
            // Say what was read, not just that it failed: a table that is all
            // zeros and a table with one implausible field are different
            // problems, and the hex is the only way to tell them apart
            // remotely.
            t.note = format!(
                "{prefix}SPD EEPROM is present but its timing fields are blank or implausible \
                 (tAA/tRCD/tRP bytes read {:#04X}/{:#04X}/{:#04X}, time base byte {:#04X}), so \
                 timings are Unknown",
                b[24], b[25], b[26], b[17]
            );
        }
    }
    t
}

/// The one time base DDR4 modules ship with, in ns.
const MTB_NS_DDR4: f64 = 0.125;
/// Fine time base, in ns (1 ps).
const FTB_NS: f64 = 0.001;
/// Key byte values (SPD byte 2), from the JEDEC memory-type list that
/// `decode-dimms` follows: 12 = DDR4 SDRAM, 14 = DDR4E SDRAM.
const SPD_DDR4: u8 = 12;
const SPD_DDR4E: u8 = 14;

/// The part number from the SPD vendor block: printable ASCII, ended by the
/// first byte that is not. The field is written that way rather than padded by
/// rule, so a module that left it blank reads as nothing, not as spaces.
fn spd_part_number(b: &[u8]) -> Option<String> {
    if b.len() < 349 {
        return None;
    }
    let text: String = b[329..349]
        .iter()
        .map(|&c| char::from(c))
        .take_while(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let text = text.trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

/// Names for the SPD byte-2 memory-type codes, used to say *what* was found
/// rather than only that it disagreed.
fn spd_memory_type_name(key: u8) -> &'static str {
    match key {
        4 => "SDR SDRAM",
        6 => "DDR SGRAM",
        7 => "DDR SDRAM",
        8 => "DDR2 SDRAM",
        11 => "DDR3 SDRAM",
        12 => "DDR4 SDRAM",
        14 => "DDR4E SDRAM",
        15 => "LPDDR3 SDRAM",
        16 => "LPDDR4 SDRAM",
        17 => "LPDDR4X SDRAM",
        18 => "DDR5 SDRAM",
        19 => "LPDDR5 SDRAM",
        _ => "an unlisted memory type",
    }
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
    /// One SMBIOS structure: four header bytes, the formatted body, the
    /// NUL-terminated strings and the terminating double NUL.
    fn smbios_record(ty: u8, body: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut b = vec![ty, (4 + body.len()) as u8, 0x00, 0x04];
        b.extend_from_slice(body);
        for s in strings {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        b.push(0);
        if strings.is_empty() {
            // With no strings the area is nothing but its terminating double
            // NUL, which is the case that used to desync the walk.
            b.push(0);
        }
        b
    }

    fn fixture_type17(size_field: u16, part: &str, slot: &str) -> Vec<u8> {
        smbios_record(
            17,
            &type17_body(size_field),
            &["Bank 0", slot, part], // string 1 = bank, 2 = locator, 3 = part
        )
    }

    /// The formatted area of a type 17 record, minus the four header bytes.
    fn type17_body(size_field: u16) -> Vec<u8> {
        let mut b = Vec::new();
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
        b
    }

    #[test]
    fn type17_record_decodes_with_spec_offsets() {
        // 8 GiB as 8192 megabytes with the unit bit clear (bit 15 set would
        // mean kilobytes — see `size_field_units_follow_the_spec_example`).
        let blob = fixture_type17(8192, "CMP32GX4M2B3200C16", "DIMM_A1");
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
        // 192 bits of module behind 160 bits of data: 32 bits of checking, by
        // subtraction, which is all any decoder can honestly say.
        assert_eq!(d.ecc_bits, Some(32));
    }

    /// The same record with different widths, so the arithmetic that calls a
    /// module ECC is tested rather than asserted in a comment.
    fn type17_with_widths(total: u16, data: u16) -> Dimm {
        let mut body = type17_body(8192);
        body[4..6].copy_from_slice(&total.to_le_bytes()); // 0x08 total width
        body[6..8].copy_from_slice(&data.to_le_bytes()); // 0x0A data width
        let blob = smbios_record(17, &body, &["Bank 0", "DIMM_A1", "PART"]);
        let records = smbios_records(&blob);
        decode_memory_device(&records[0]).expect("decode")
    }

    #[test]
    fn check_bits_are_the_difference_between_the_two_widths() {
        // 72 over 64 is the ordinary ECC module; 80 over 72 is what a wide
        // server rank reports for the same eight bits of checking.
        assert_eq!(type17_with_widths(72, 64).ecc_bits, Some(8));
        assert_eq!(type17_with_widths(80, 72).ecc_bits, Some(8));
        // Equal widths is a reported answer, not an absence of one.
        assert_eq!(type17_with_widths(64, 64).ecc_bits, Some(0));
        // 0xFFFF means the firmware could not say, which must not be read as
        // a module with no checking.
        assert_eq!(type17_with_widths(0xFFFF, 64).ecc_bits, None);
        assert_eq!(type17_with_widths(72, 0xFFFF).ecc_bits, None);

        // The Xeon rig's firmware, as `dmidecode` reads it: 72 bits in both
        // fields. There is no 72-bit data path, so the module is carrying eight
        // check bits and the subtraction that ignores that called a machine with
        // eight ECC RDIMMs clean.
        assert_eq!(type17_with_widths(72, 72).ecc_bits, Some(8));
        assert_eq!(type17_with_widths(80, 80).ecc_bits, Some(16));
        // A power of two is a real data path and stays zero, and a zero width is
        // a non-answer rather than a 0-bit bus (it also has no ilog2).
        assert_eq!(type17_with_widths(32, 32).ecc_bits, Some(0));
        assert_eq!(type17_with_widths(0, 0).ecc_bits, None);
    }

    #[test]
    fn the_array_names_its_correction_and_refuses_to_guess() {
        let record = |code: u8| {
            let mut b = vec![0u8; 0x13];
            b[2] = code; // offset 0x06 in the record is two bytes into the body
            smbios_records(&smbios_record(16, &b, &[]))
        };
        let named = |code: u8| array_error_correction(&record(code));
        // Table 75, not a recollection of it. These four values are the ones
        // the invented table got wrong, so they are the ones worth pinning.
        assert_eq!(named(0x06).as_deref(), Some("Multi-bit ECC"));
        assert_eq!(named(0x05).as_deref(), Some("Single-bit ECC"));
        assert_eq!(named(0x03).as_deref(), Some("None"));
        assert_eq!(named(0x04).as_deref(), Some("Parity"));
        assert_eq!(named(0x07).as_deref(), Some("CRC"));
        assert_eq!(named(0x01).as_deref(), Some("Other"));
        // 0x02 is "Unknown" and 0x20 is reserved: both say nothing at all,
        // rather than the panel printing a confident "None" for a board that
        // never answered the question.
        assert_eq!(named(0x02), None);
        assert_eq!(named(0x00), None);
        assert_eq!(named(0x20), None);

        // Two arrays that agree collapse to one name (the X99 rig); two that
        // disagree are both printed rather than arbitrated.
        let pair = |a: u8, b: u8| {
            let mk = |code: u8| {
                let mut b = vec![0u8; 0x13];
                b[2] = code;
                smbios_record(16, &b, &[])
            };
            let mut blob = mk(a);
            blob.extend_from_slice(&mk(b));
            array_error_correction(&smbios_records(&blob))
        };
        assert_eq!(pair(0x06, 0x06).as_deref(), Some("Multi-bit ECC"));
        assert_eq!(pair(0x06, 0x03).as_deref(), Some("Multi-bit ECC / None"));
        // One array answering and the other not is still an answer, not a tie.
        assert_eq!(pair(0x06, 0x02).as_deref(), Some("Multi-bit ECC"));
        assert_eq!(pair(0x02, 0x02), None);
    }

    /// Decode one Size field on its own, so the unit convention can be tested
    /// against the spec's own worked example rather than against a fixture
    /// built from the same reading.
    fn size_of(size_field: u16) -> Option<u64> {
        let blob = smbios_record(17, &type17_body(size_field), &["Bank 0", "DIMM 0", "X"]);
        let records = smbios_records(&blob);
        decode_memory_device(records.first()?).map(|d| d.size_bytes.unwrap_or(0))
    }

    #[test]
    fn size_field_units_follow_the_spec_example() {
        // DSP0134 7.18, quoted: "the value 8100h identifies a 256 KB memory
        // device and 0100h identifies a 256 MB memory device". Bit 15 set means
        // kilobytes and clear means megabytes — the opposite of what looks
        // intuitive, and this decoder had it the other way round once, which
        // would have called a 16 GiB stick 16 MB. Its own fixture agreed with
        // the mistake, which is the lesson: the expectations below are the
        // spec's numbers, not derived from the code.
        assert_eq!(size_of(0x8100), Some(256 * 1024));
        assert_eq!(size_of(0x0100), Some(256 * 1024 * 1024));
        // 16384 in the low bits, unit bit clear: a 16 GiB stick, which is what
        // the reporter's board reports for each of its two modules.
        assert_eq!(size_of(0x4000), Some(16 * 1024 * 1024 * 1024));
    }

    #[test]
    fn an_empty_slot_is_skipped() {
        // Size 0 = no device installed in the socket (DSP0134 7.18).
        assert_eq!(size_of(0), None);
        // Unknown size keeps the module but says so, rather than dropping it.
        let blob = smbios_record(17, &type17_body(0xFFFF), &["Bank 0", "DIMM 0", "X"]);
        let d = decode_memory_device(&smbios_records(&blob)[0]).expect("decode");
        assert_eq!(d.size_bytes, None);
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

    /// A record with no strings is nothing but the terminating double NUL after
    /// its formatted area. The old walker stopped at the first of those two
    /// bytes, so it resumed one byte early and read every later structure out
    /// of place: on the reporter's B450 the walk died after 746 of 1948 bytes
    /// and never reached the type 17 records at 1277 and 1498, which made a
    /// board with 32 GiB fitted report no populated memory.
    #[test]
    fn a_record_without_strings_does_not_desync_the_walk() {
        let mut blob = smbios_record(32, &[0u8; 7], &[]); // System Boot, no strings
        blob.extend(smbios_record(
            17,
            &type17_body(0x4000), // 16384 MB
            &["Bank 0", "P0 CHANNEL A", "CMK32GX4M2A2133C13"],
        ));
        let records = smbios_records(&blob);
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[1].ty, 17);
        let d = decode_memory_device(&records[1]).expect("decode");
        assert_eq!(d.size_bytes, Some(16 * 1024 * 1024 * 1024));
        assert_eq!(d.slot.as_deref(), Some("P0 CHANNEL A"));
        assert_eq!(d.part_number.as_deref(), Some("CMK32GX4M2A2133C13"));
    }

    /// The entry point structure and the structures live in separate files on a
    /// modern kernel, but a blob that does carry an anchor must still parse.
    #[test]
    fn an_entry_point_is_skipped_when_present() {
        let mut blob = vec![0u8; 0x18];
        blob[0..5].copy_from_slice(b"_SM3_");
        blob[6] = 0x18; // 64-bit entry point length
        let mut inner = vec![0u8; 0x1f];
        inner[0..4].copy_from_slice(b"_SM_");
        inner[5] = 0x1f; // 32-bit entry point length
        blob.extend(inner);
        blob.extend(smbios_record(
            17,
            &type17_body(8192),
            &["Bank 0", "DIMM 0", "X"],
        ));
        let records = smbios_records(&blob);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].ty, 17);
    }

    /// The two modules from the ASRock B450 Pro4 R2.0 in issue #1, byte for
    /// byte as read from `/sys/bus/i2c/devices/5-005x/eeprom` on that machine:
    /// the first 32 bytes plus the fine-time byte at 125. `decode-dimms`, run
    /// there against the same EEPROMs, reports DDR4 SDRAM, SPD revision 1.0,
    /// UDIMM, 2133 MT/s (PC4-17000), 16384 MB, AA-RCD-RP-RAS 15-15-15-36 and
    /// CAS latencies 9 to 16, with both block CRCs valid — so the assertions
    /// below are the reference decoder's own answers, not this one's.
    fn reporters_modules() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[..32].copy_from_slice(&[
            0x23, 0x10, 0x0c, 0x02, 0x85, 0x21, 0x00, 0x08, 0x00, 0x00, 0x00, 0x03, 0x09, 0x03,
            0x00, 0x00, 0x00, 0x00, 0x08, 0x0c, 0xfc, 0x03, 0x00, 0x00, 0x6c, 0x6c, 0x6c, 0x11,
            0x08, 0x74, 0xf0, 0x0a,
        ]);
        b[118] = 0xb5;
        b[119] = 0xcf;
        b[125] = 0xc2; // tCKAVGmin fine time: -62 ps, signed
        b[126] = 0x7d; // block 0 CRC, low byte
        b[127] = 0x54;
        // The vendor block, also as read: module JEDEC code 0x02/0x9E, which
        // `decode-dimms` resolves to Corsair, DRAM code 0x80/0x2C for Micron,
        // and the part number. The date, location and revision bytes are
        // unwritten on these modules, which the decoder has to live with.
        b[320] = 0x02;
        b[321] = 0x9e;
        b[329..349].copy_from_slice(b"CMK32GX4M2A2133C13  ");
        b[350] = 0x80;
        b[351] = 0x2c;
        b
    }

    #[test]
    fn the_reporters_ddr4_2133_kit_decodes() {
        let t = decode_spd("DDR4", "5-0050", &reporters_modules());
        assert_eq!(t.max_mts, Some(2133), "{t:?}");
        assert_eq!(t.tAA_ns, Some(13.5), "{t:?}");
        assert_eq!(t.tRCD_ns, Some(13.5), "{t:?}");
        assert_eq!(t.tRP_ns, Some(13.5), "{t:?}");
        assert_eq!(t.tRAS_ns, Some(33.0), "{t:?}");
        assert_eq!(t.cycles.as_deref(), Some("15-15-15-36"), "{t:?}");
        assert!(t.note.contains("UDIMM"), "{}", t.note);
        assert!(t.note.contains("up to CL16"), "{}", t.note);
        assert!(t.note.contains("not the values"), "{}", t.note);
    }

    #[test]
    fn the_reporters_module_names_itself() {
        // Model straight off the EEPROM, no SMBIOS involved: the string SMBIOS
        // carries for these modules, minus its two trailing spaces.
        let t = decode_spd("DDR4", "5-0050", &reporters_modules());
        assert_eq!(t.part_number.as_deref(), Some("CMK32GX4M2A2133C13"));
    }

    #[test]
    fn a_part_number_field_left_blank_is_not_an_empty_string() {
        let mut b = reporters_modules();
        b[329..349].fill(0);
        assert_eq!(decode_spd("DDR4", "5-0050", &b).part_number, None);
        // A read that stops before the vendor block says nothing rather than
        // guessing at a label.
        let short = &reporters_modules()[..256];
        assert_eq!(decode_spd("DDR4", "5-0050", short).part_number, None);
    }

    #[test]
    fn a_revision_1_0_module_is_not_a_bad_key_byte() {
        // Issue #1 in one byte: SPD byte 1 is 0x10, which is revision 1.0. The
        // released decoder looked for the key byte there and refused both
        // modules; the key byte is byte 2, and theirs says 0x0C = DDR4.
        let b = reporters_modules();
        assert_eq!(b[1], 0x10);
        assert_eq!(b[2], 0x0c);
        let t = decode_spd("DDR4", "5-0050", &b);
        assert!(!t.note.contains("key byte"), "{}", t.note);
        assert!(t.cycles.is_some(), "nothing was decoded: {}", t.note);
    }

    #[test]
    fn a_disagreeing_key_byte_is_named_not_just_hex_dumped() {
        let mut b = reporters_modules();
        b[2] = 11; // DDR3 SDRAM, while the driver binding says DDR4
        let t = decode_spd("DDR4", "0-0052", &b);
        assert_eq!(t.cycles, None);
        assert!(t.note.contains("DDR3 SDRAM"), "{}", t.note);
        // The revision byte is called a revision, so the message cannot send a
        // reader looking for 0x12 in the wrong place again.
        assert!(t.note.contains("revision"), "{}", t.note);
    }

    #[test]
    fn a_blank_spd_is_reported_as_blank() {
        let b = vec![0u8; 512];
        let t = decode_spd("DDR4", "0-0052", &b);
        assert_eq!(t.cycles, None);
        assert_eq!(t.max_mts, None);
        assert!(t.note.contains("all zeros"), "{}", t.note);
        assert_eq!(t.generation.as_deref(), Some("DDR4"));
    }

    #[test]
    fn an_unsupported_time_base_stops_the_decode() {
        let mut b = reporters_modules();
        b[17] = 0x01; // bits 1:0 must be zero; no shipping module does this
        let t = decode_spd("DDR4", "5-0050", &b);
        assert_eq!(t.cycles, None);
        assert!(t.note.contains("time base"), "{}", t.note);
    }

    #[test]
    fn the_fine_time_correction_is_signed() {
        // -62 ps is what the reporter's module carries on tCKAVGmin; a decoder
        // that reads it unsigned would claim 125 ps extra per cycle and land on
        // 1976 MT/s instead of 2133.
        let full = reporters_modules();
        assert_eq!(full[125], 0xc2);
        let t = decode_spd("DDR4", "5-0050", &full);
        assert_eq!(t.max_mts, Some(2133));

        let mut b = reporters_modules();
        b[123] = 0xe0; // tAAmin fine: -32 ps
        let t = decode_spd("DDR4", "5-0050", &b);
        let taa = t.tAA_ns.expect("tAA");
        assert!((taa - 13.468).abs() < 0.001, "tAA {taa}");
    }

    #[test]
    fn a_short_spd_read_says_the_fine_bytes_are_missing() {
        // The corrections are bytes 121..125, so a read that stops before them
        // still decodes — it must not pretend they were zero.
        let short = &reporters_modules()[..120];
        let t = decode_spd("DDR4", "5-0050", short);
        assert!(t.note.contains("fine-time"), "{}", t.note);
        assert_eq!(t.tAA_ns, Some(13.5));
        // Without the -62 ps correction tCKAVGmin reads as a round 1.0 ns, so
        // the module looks like a 2000 MT/s part rather than the DDR4-2133 it
        // is. That drift is the reason the note has to say so.
        assert_eq!(t.max_mts, Some(2000), "{}", t.note);
    }

    #[test]
    fn ddr5_spd_is_reported_without_invented_timings() {
        let mut b = vec![0u8; 256];
        b[2] = 18; // 18 = DDR5 SDRAM
        let t = decode_spd("DDR5", "0-0051", &b);
        assert_eq!(t.generation.as_deref(), Some("DDR5"));
        assert_eq!(t.cycles, None);
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
