use crate::bail;
use crate::elf::get_page_mask;
use crate::ensure;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::split_buffers_by_alignment;
use crate::file_writer::split_output_by_group;
use crate::file_writer::split_output_into_sections;
use crate::incremental::PreparedState;
use crate::layout::FileLayout;
use crate::layout::HeaderInfo;
use crate::layout::Layout;
use crate::layout::ObjectLayout;
use crate::layout::OutputRecordLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::layout::SymbolCopyInfo;
use crate::macho::BuildVersionCommand;
use crate::macho::CS_ADHOC;
use crate::macho::CS_BLOB_HEADERS_SIZE;
use crate::macho::CS_BLOCK_SIZE;
use crate::macho::CS_BLOCK_SIZE_EXP;
use crate::macho::CS_EXECSEG_MAIN_BINARY;
use crate::macho::CS_HASH_SIZE;
use crate::macho::CS_HASHTYPE_SHA256;
use crate::macho::CS_IDENTIFIER_STRING;
use crate::macho::CS_LINKER_SIGNED;
use crate::macho::CS_PADDED_FILENAME_SIZE;
use crate::macho::CS_SUPPORTSEXECSEG;
use crate::macho::CSMAGIC_CODEDIRECTORY;
use crate::macho::CSMAGIC_EMBEDDED_SIGNATURE;
use crate::macho::CSSLOT_CODEDIRECTORY;
use crate::macho::CodeSignatureBlobIndex;
use crate::macho::CodeSignatureCodeDirectory;
use crate::macho::CodeSignatureCommand;
use crate::macho::CodeSignatureSuperBlob;
use crate::macho::DEFAULT_SEGMENT_COUNT;
use crate::macho::DYLINKER_PATH;
use crate::macho::DyldChainedFixupsCommand;
use crate::macho::DylibCommand;
use crate::macho::DylibLoadKind;
use crate::macho::DylinkerCommand;
use crate::macho::EntryPointCommand;
use crate::macho::FileHeader;
use crate::macho::MACHO_COMMAND_ALIGNMENT;
use crate::macho::MACHO_COMPACT_UNWIND_ENTRY_SIZE;
use crate::macho::MACHO_PAGE_SIZE;
use crate::macho::MACHO_START_MEM_ADDRESS;
use crate::macho::MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT;
use crate::macho::MACHO_UNWIND_SECOND_LEVEL_REGULAR;
use crate::macho::MachO;
use crate::macho::SectionEntry;
use crate::macho::SegmentCommand;
use crate::macho::SegmentSectionsInfo;
use crate::macho::SegmentType;
use crate::macho::SymtabCommand;
use crate::macho::UuidCommand;
use crate::macho::get_segment_sections;
use crate::macho::id_dylib_command_size;
use crate::macho::id_dylib_path;
use crate::macho::load_dylib_command_count;
use crate::macho::load_dylib_command_size;
use crate::macho::load_dylib_commands;
use crate::macho::load_dylib_paths;
use crate::macho::macho_live_eh_frame_cies;
use crate::output_section_id;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::HexU64;
use crate::output_trace::TraceOutput;
use crate::part_id;
use crate::platform::Arch;
use crate::platform::Args;
use crate::platform::ObjectFile;
use crate::platform::RelocationList;
use crate::platform::SectionHeader;
use crate::platform::Symbol;
use crate::resolution::SectionSlot;
use crate::sharding::ShardKey;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use itertools::Itertools;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::RelocationSize;
use linker_utils::relaxation::SectionRelaxDeltas;
use linker_utils::relaxation::opt_input_to_output;
use object::BigEndian;
use object::Endianness;
use object::SymbolIndex;
use object::U32;
use object::from_bytes_mut;
use object::macho;
use object::macho::CPU_TYPE_ARM64;
use object::macho::LC_BUILD_VERSION;
use object::macho::LC_CODE_SIGNATURE;
use object::macho::LC_DYLD_CHAINED_FIXUPS;
use object::macho::LC_ID_DYLIB;
use object::macho::LC_LOAD_DYLIB;
use object::macho::LC_LOAD_DYLINKER;
use object::macho::LC_LOAD_WEAK_DYLIB;
use object::macho::LC_MAIN;
use object::macho::LC_SEGMENT_64;
use object::macho::LC_SYMTAB;
use object::macho::LC_UUID;
use object::macho::MH_CIGAM_64;
use object::macho::MH_DYLIB;
use object::macho::MH_EXECUTE;
use object::macho::N_ABS;
use object::macho::N_EXT;
use object::macho::N_PEXT;
use object::macho::N_SECT;
use object::macho::N_UNDF;
use object::macho::RelocationInfo;
use object::macho::SEG_DATA;
use object::macho::SEG_LINKEDIT;
use object::macho::SEG_PAGEZERO;
use object::macho::SEG_TEXT;
use object::read::macho::Section as MachOSection;
use object::slice_from_bytes_mut;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use rayon::slice::ParallelSlice;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::ops::BitAnd;
use tracing::debug_span;
use zerocopy::FromBytes;
use zerocopy::FromZeros;

const LE: Endianness = Endianness::Little;

type MachOLayout<'data> = Layout<'data, MachO>;
type SymtabEntry = object::macho::Nlist64<Endianness>;
const MACHO_UUID: [u8; 16] = *b"SLD-MACHO-UUID!!";
const DYLD_CHAINED_PTR_64_OFFSET: u16 = 6;
const DYLD_CHAINED_PTR_START_NONE: u16 = 0xffff;
const DATA_SEGMENT_CHAIN_INDEX: usize = 2;

#[derive(Debug, Default)]
struct ChainedRebases {
    slots: Vec<u64>,
    bind_slots: Vec<ChainedBindSlot>,
    got_entries: Vec<ChainedGotEntry>,
    imports: Vec<ChainedImport>,
}

#[derive(Debug)]
struct ChainedBindSlot {
    address: u64,
    import_index: usize,
}

#[derive(Debug)]
struct ChainedGotEntry {
    address: u64,
    target: u64,
    import_index: Option<usize>,
    name: String,
}

#[derive(Debug)]
struct ChainedImport {
    name: Vec<u8>,
    lib_ordinal: u8,
    got_address: Option<u64>,
    stub_address: Option<u64>,
}

impl ChainedRebases {
    fn collect<'data, A: Arch<Platform = MachO>>(layout: &MachOLayout<'data>) -> Result<Self> {
        let Some(data_segment) = get_segment_sections(layout, SegmentType::DataSections) else {
            return Ok(Self::default());
        };
        let data_start = data_segment.segment_size.mem_offset;
        let data_end = data_start + data_segment.segment_size.mem_size;
        let mut slots = Vec::new();
        let mut bind_slots = Vec::new();
        let mut imports = Vec::new();

        for group in &layout.group_layouts {
            for file in &group.files {
                let FileLayout::Object(object) = file else {
                    continue;
                };
                for (section_index, section) in object.sections.iter().enumerate() {
                    if !matches!(section, SectionSlot::Loaded(_)) {
                        continue;
                    }
                    let section_index = object::SectionIndex(section_index);
                    let relax_deltas = object.section_relax_deltas.get(section_index.0);
                    let Some(section_address) =
                        object.section_resolutions[section_index.0].address()
                    else {
                        continue;
                    };
                    let live_unwind_ranges =
                        macho_writer_live_unwind_relocation_ranges(object, layout, section_index)?;
                    let live_subsection_ranges = macho_writer_live_subsection_relocation_ranges(
                        object,
                        layout,
                        section_index,
                    )?;
                    let mut skip_subtractor_pair = false;
                    for rel in object.relocations(section_index)?.relocations {
                        let rel = rel.info(LE);
                        let input_offset = rel.r_address as usize;
                        if relax_deltas
                            .is_some_and(|deltas| deltas.deletes_input_offset(input_offset as u64))
                        {
                            skip_subtractor_pair = false;
                            continue;
                        }
                        if live_unwind_ranges.as_ref().is_some_and(|ranges| {
                            !ranges.iter().any(|range| range.contains(&input_offset))
                        }) {
                            skip_subtractor_pair = false;
                            continue;
                        }
                        if live_subsection_ranges.as_ref().is_some_and(|ranges| {
                            !ranges.iter().any(|range| range.contains(&input_offset))
                        }) {
                            skip_subtractor_pair = false;
                            continue;
                        }
                        if rel.r_type == macho::ARM64_RELOC_ADDEND {
                            continue;
                        }
                        if rel.r_type == macho::ARM64_RELOC_SUBTRACTOR {
                            skip_subtractor_pair = true;
                            continue;
                        }
                        if skip_subtractor_pair {
                            ensure!(
                                rel.r_type == macho::ARM64_RELOC_UNSIGNED,
                                "Mach-O ARM64_RELOC_SUBTRACTOR must be followed by ARM64_RELOC_UNSIGNED"
                            );
                            skip_subtractor_pair = false;
                            continue;
                        }
                        if is_tlv_init_relocation(object.object.section(section_index)?, rel) {
                            continue;
                        }
                        let rel_info = A::relocation_from_raw(rel)?;
                        if !is_chained_rebase_relocation(&rel_info) {
                            continue;
                        }
                        let output_offset = relax_deltas.map_or(input_offset as u64, |deltas| {
                            deltas.input_to_output_offset(input_offset as u64)
                        });
                        let place = section_address + output_offset;
                        if place >= data_start && place + 8 <= data_end {
                            let (resolution, local_symbol_id) =
                                get_resolution(rel, object, layout)?;
                            if is_import_relocation(layout, local_symbol_id, &resolution) {
                                let import_index = push_import(
                                    layout,
                                    &mut imports,
                                    local_symbol_id.context(
                                        "Mach-O import relocation must reference a symbol",
                                    )?,
                                    None,
                                    None,
                                )?;
                                bind_slots.push(ChainedBindSlot {
                                    address: place,
                                    import_index,
                                });
                                slots.push(place);
                            } else if resolution.raw_value >= MACHO_START_MEM_ADDRESS {
                                slots.push(place);
                            }
                        }
                    }
                    ensure!(
                        !skip_subtractor_pair,
                        "Mach-O ARM64_RELOC_SUBTRACTOR missing paired ARM64_RELOC_UNSIGNED"
                    );
                }
            }
        }

        let mut got_entries = Vec::new();
        for group in &layout.group_layouts {
            for file in &group.files {
                let FileLayout::Object(object) = file else {
                    continue;
                };
                for (symbol_id, resolution) in layout.resolutions_in_range(object.symbol_id_range) {
                    let Some(resolution) = resolution else {
                        continue;
                    };
                    let Some(got_address) = resolution.format_specific.got_address else {
                        continue;
                    };
                    let got_address = got_address.get();
                    let import_index = if is_import_symbol(layout, symbol_id)
                        && resolution.raw_value < MACHO_START_MEM_ADDRESS
                    {
                        Some(push_import(
                            layout,
                            &mut imports,
                            symbol_id,
                            Some(got_address),
                            resolution
                                .format_specific
                                .stub_address
                                .map(|address| address.get()),
                        )?)
                    } else {
                        None
                    };
                    got_entries.push(ChainedGotEntry {
                        address: got_address,
                        target: resolution.raw_value,
                        import_index,
                        name: layout
                            .symbol_db
                            .symbol_name_for_display(symbol_id)
                            .to_string(),
                    });
                    slots.push(got_address);
                }
            }
        }

        slots.sort_unstable();
        slots.dedup();
        bind_slots.sort_by_key(|entry| entry.address);
        got_entries.sort_by_key(|entry| entry.address);
        got_entries.dedup_by_key(|entry| entry.address);
        Ok(Self {
            slots,
            bind_slots,
            got_entries,
            imports,
        })
    }

    fn contains(&self, place: u64) -> bool {
        self.slots.binary_search(&place).is_ok()
    }

    fn next_stride(&self, place: u64) -> Result<u64> {
        let Ok(index) = self.slots.binary_search(&place) else {
            return Ok(0);
        };
        let page_start = place / MACHO_PAGE_SIZE * MACHO_PAGE_SIZE;
        let Some(next) = self.slots.get(index + 1).copied() else {
            return Ok(0);
        };
        if next >= page_start + MACHO_PAGE_SIZE {
            return Ok(0);
        }
        let distance = next - place;
        ensure!(
            distance.is_multiple_of(4),
            "Mach-O chained fixup distance {distance:#x} is not 4-byte aligned"
        );
        let stride = distance / 4;
        ensure!(
            stride < 0x1000,
            "Mach-O chained fixup distance {distance:#x} is too large"
        );
        Ok(stride)
    }

    fn bind_import_index(&self, place: u64) -> Option<usize> {
        self.bind_slots
            .binary_search_by_key(&place, |slot| slot.address)
            .ok()
            .map(|index| self.bind_slots[index].import_index)
    }
}

fn is_import_relocation(
    layout: &MachOLayout<'_>,
    local_symbol_id: Option<SymbolId>,
    resolution: &Resolution<MachO>,
) -> bool {
    resolution.raw_value < MACHO_START_MEM_ADDRESS
        && local_symbol_id.is_some_and(|symbol_id| is_import_symbol(layout, symbol_id))
}

fn is_import_symbol(layout: &MachOLayout<'_>, symbol_id: SymbolId) -> bool {
    layout
        .symbol_db
        .is_undefined(layout.symbol_db.definition(symbol_id))
}

fn push_import(
    layout: &MachOLayout<'_>,
    imports: &mut Vec<ChainedImport>,
    symbol_id: SymbolId,
    got_address: Option<u64>,
    stub_address: Option<u64>,
) -> Result<usize> {
    let raw_name = layout.symbol_db.symbol_name(symbol_id)?;
    let name = import_symbol_name(raw_name.bytes()).to_vec();
    let lib_ordinal = import_library_ordinal(layout, &name)?;
    if let Some((import_index, import)) = imports
        .iter_mut()
        .enumerate()
        .find(|(_, import)| import.name == name && import.lib_ordinal == lib_ordinal)
    {
        if let Some(got_address) = got_address {
            match import.got_address {
                Some(existing) if existing != got_address => {}
                _ => {
                    import.got_address = Some(got_address);
                    import.stub_address = stub_address;
                    return Ok(import_index);
                }
            }
        } else {
            return Ok(import_index);
        }
    }
    let import_index = imports.len();
    imports.push(ChainedImport {
        name,
        lib_ordinal,
        got_address,
        stub_address,
    });
    Ok(import_index)
}

fn import_symbol_name(name: &[u8]) -> &[u8] {
    if name.starts_with(b"_objc_msgSend$") {
        return b"_objc_msgSend";
    }
    if name.starts_with(b"_objc_msgSendSuper2$") {
        return b"_objc_msgSendSuper2";
    }
    if name.starts_with(b"_objc_msgSendSuper$") {
        return b"_objc_msgSendSuper";
    }
    name
}

fn import_library_ordinal(layout: &MachOLayout<'_>, name: &[u8]) -> Result<u8> {
    let Some(library) = import_library_name(name) else {
        return Ok(layout
            .args()
            .dylib_symbol_ordinals
            .get(name)
            .copied()
            .unwrap_or(1));
    };
    for (index, path) in load_dylib_paths(layout.args()).enumerate() {
        if path_matches_library(path, library) {
            return u8::try_from(index + 1).context("Mach-O dylib ordinal exceeds u8");
        }
    }
    Ok(1)
}

fn import_library_name(name: &[u8]) -> Option<&'static [u8]> {
    if let Some(library) = objc_class_library_name(name) {
        return Some(library);
    }
    if name.starts_with(b"_objc_")
        || name.starts_with(b"__objc")
        || name.starts_with(b"_OBJC_")
        || name.starts_with(b"_class_")
        || name.starts_with(b"_imp_")
        || name.starts_with(b"_ivar_")
        || name.starts_with(b"_method_")
        || name.starts_with(b"_object_")
        || name.starts_with(b"_property_")
        || name.starts_with(b"_protocol_")
        || name.starts_with(b"_sel_")
    {
        return Some(b"libobjc");
    }
    if name.starts_with(b"_CF") || name.starts_with(b"_kCF") || name.starts_with(b"___CF") {
        return Some(b"CoreFoundation.framework");
    }
    if matches!(name, b"___NSArray0__" | b"___NSDictionary0__") {
        return Some(b"CoreFoundation.framework");
    }
    if name.starts_with(b"_AudioComponent")
        || name.starts_with(b"_AudioConverter")
        || name.starts_with(b"_AudioOutputUnit")
        || name.starts_with(b"_AudioUnit")
    {
        return Some(b"AudioToolbox.framework");
    }
    if name.starts_with(b"_AudioConvertHostTime")
        || name.starts_with(b"_AudioDevice")
        || name.starts_with(b"_AudioGetCurrentHostTime")
        || name.starts_with(b"_AudioHardware")
        || name.starts_with(b"_AudioObject")
    {
        return Some(b"CoreAudio.framework");
    }
    if name.starts_with(b"_CG") || name.starts_with(b"_kCG") {
        return Some(b"CoreGraphics.framework");
    }
    if is_security_cms_symbol(name) {
        return Some(b"Security.framework");
    }
    if name.starts_with(b"_CM") || name.starts_with(b"_kCM") {
        return Some(b"CoreMedia.framework");
    }
    if name.starts_with(b"_CV") || name.starts_with(b"_kCV") {
        return Some(b"CoreVideo.framework");
    }
    if name.starts_with(b"_VT") || name.starts_with(b"_kVT") {
        return Some(b"VideoToolbox.framework");
    }
    if name.starts_with(b"_IOSurface") || name.starts_with(b"_kIOSurface") {
        return Some(b"IOSurface.framework");
    }
    if name.starts_with(b"_IO") || name.starts_with(b"_kIO") {
        return Some(b"IOKit.framework");
    }
    if name.starts_with(b"_FSEvent") || name.starts_with(b"_LS") || name.starts_with(b"_UT") {
        return Some(b"CoreServices.framework");
    }
    if name.starts_with(b"_Authorization")
        || name.starts_with(b"_CMS")
        || name.starts_with(b"_CSSM")
        || name.starts_with(b"_Sec")
        || name.starts_with(b"_SSL")
        || name.starts_with(b"_TLS")
        || name.starts_with(b"_errAuthorization")
        || name.starts_with(b"_kCMS")
        || name.starts_with(b"_kSec")
    {
        return Some(b"Security.framework");
    }
    if name.starts_with(b"_SCContent")
        || name.starts_with(b"_SCShareable")
        || name.starts_with(b"_SCStream")
        || name.starts_with(b"_kSCContent")
        || name.starts_with(b"_kSCShareable")
        || name.starts_with(b"_kSCStream")
    {
        return Some(b"ScreenCaptureKit.framework");
    }
    if name.starts_with(b"_SC") || name.starts_with(b"_kSC") {
        return Some(b"SystemConfiguration.framework");
    }
    if is_appkit_ns_symbol(name) {
        return Some(b"AppKit.framework");
    }
    if !is_libsystem_ns_symbol(name) && (name.starts_with(b"_NS") || name.starts_with(b"__NS")) {
        return Some(b"Foundation.framework");
    }
    if is_zlib_symbol(name) {
        return Some(b"libz");
    }
    if name.starts_with(b"_iconv") || name.starts_with(b"_libiconv") {
        return Some(b"libiconv");
    }
    if is_libcxx_symbol(name) {
        return Some(b"libc++");
    }
    None
}

fn objc_class_library_name(name: &[u8]) -> Option<&'static [u8]> {
    let class_name = name
        .strip_prefix(b"_OBJC_CLASS_$_")
        .or_else(|| name.strip_prefix(b"_OBJC_METACLASS_$_"))?;

    if matches!(
        class_name,
        b"NSArray" | b"NSData" | b"NSDictionary" | b"NSMutableArray" | b"NSSet"
    ) {
        return Some(b"CoreFoundation.framework");
    }
    if matches!(
        class_name,
        b"NSApplication"
            | b"NSButton"
            | b"NSColor"
            | b"NSEvent"
            | b"NSImage"
            | b"NSMenu"
            | b"NSPanel"
            | b"NSPasteboard"
            | b"NSRunningApplication"
            | b"NSScreen"
            | b"NSTableView"
            | b"NSTextView"
            | b"NSView"
            | b"NSWindow"
            | b"NSWorkspace"
    ) {
        return Some(b"AppKit.framework");
    }
    if class_name.starts_with(b"SC") {
        return Some(b"ScreenCaptureKit.framework");
    }
    if class_name.starts_with(b"NS") && class_name != b"NSObject" {
        return Some(b"Foundation.framework");
    }
    None
}

fn is_libcxx_symbol(name: &[u8]) -> bool {
    if name == b"___cxa_atexit" {
        return false;
    }
    name.starts_with(b"__Z") || name == b"___gxx_personality_v0" || name.starts_with(b"___cxa_")
}

fn is_libsystem_ns_symbol(name: &[u8]) -> bool {
    matches!(
        name,
        b"__NSConcreteGlobalBlock"
            | b"__NSConcreteStackBlock"
            | b"_NSGetExecutablePath"
            | b"__NSGetExecutablePath"
            | b"__NSGetArgc"
            | b"__NSGetArgv"
            | b"__NSGetEnviron"
    )
}

fn is_zlib_symbol(name: &[u8]) -> bool {
    name.starts_with(b"_adler32")
        || name.starts_with(b"_compress")
        || name.starts_with(b"_crc32")
        || name.starts_with(b"_deflate")
        || name.starts_with(b"_gz")
        || name.starts_with(b"_inflate")
        || name.starts_with(b"_uncompress")
        || name == b"_get_crc_table"
        || name == b"_zlibVersion"
}

fn is_security_cms_symbol(name: &[u8]) -> bool {
    name.starts_with(b"_CMSDecoder")
        || name.starts_with(b"_CMSEncode")
        || name.starts_with(b"_CMSEncoder")
}

fn is_appkit_ns_symbol(name: &[u8]) -> bool {
    name.starts_with(b"_NSApplication")
        || name.starts_with(b"_NSButton")
        || name.starts_with(b"_NSColor")
        || name.starts_with(b"_NSEvent")
        || name.starts_with(b"_NSImage")
        || name.starts_with(b"_NSMenu")
        || name.starts_with(b"_NSPanel")
        || name.starts_with(b"_NSPasteboard")
        || name.starts_with(b"_NSRunningApplication")
        || name.starts_with(b"_NSScreen")
        || name.starts_with(b"_NSTableView")
        || name.starts_with(b"_NSTextView")
        || name.starts_with(b"_NSView")
        || name.starts_with(b"_NSWindow")
        || name.starts_with(b"_NSWorkspace")
}

fn path_matches_library(path: &[u8], library: &[u8]) -> bool {
    if library.ends_with(b".framework") {
        return path
            .split(|byte| *byte == b'/')
            .any(|component| component == library);
    }

    let basename = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    if basename == library {
        return true;
    }
    let Some(suffix) = basename.strip_prefix(library) else {
        return false;
    };
    matches!(suffix.first(), Some(b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::compact_unwind_dwarf_offset_hint;
    use super::path_matches_library;
    use super::rewrite_compacted_macho_eh_frame_cie_pointers;
    use linker_utils::relaxation::SectionRelaxDeltas;

    #[test]
    fn framework_matching_uses_path_components() {
        assert!(path_matches_library(
            b"/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation",
            b"Foundation.framework",
        ));
        assert!(path_matches_library(
            b"/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
            b"CoreFoundation.framework",
        ));
        assert!(!path_matches_library(
            b"/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
            b"Foundation.framework",
        ));
    }

    #[test]
    fn dylib_matching_uses_basename_prefix() {
        assert!(path_matches_library(
            b"/usr/lib/libobjc.A.dylib",
            b"libobjc"
        ));
        assert!(path_matches_library(b"/usr/lib/libc++.1.dylib", b"libc++"));
        assert!(!path_matches_library(
            b"/usr/lib/libcompression.dylib",
            b"libc",
        ));
    }

    #[test]
    fn compact_unwind_dwarf_offset_hint_falls_back_to_section_start() {
        assert_eq!(compact_unwind_dwarf_offset_hint(0), 0);
        assert_eq!(compact_unwind_dwarf_offset_hint(0x00ff_ffff), 0x00ff_ffff);
        assert_eq!(compact_unwind_dwarf_offset_hint(0x0100_0000), 0);
    }

    #[test]
    fn compacted_eh_frame_rewrites_fde_cie_pointer() {
        let input = [
            4, 0, 0, 0, // Live CIE length.
            0, 0, 0, 0, // Live CIE marker.
            4, 0, 0, 0, // Dead CIE length.
            0, 0, 0, 0, // Dead CIE marker.
            4, 0, 0, 0, // Live FDE length.
            20, 0, 0, 0, // Input pointer back to the live CIE.
        ];
        let deltas = SectionRelaxDeltas::new(vec![(8, 8)]);
        let mut out = [
            4, 0, 0, 0, // Live CIE length.
            0, 0, 0, 0, // Live CIE marker.
            4, 0, 0, 0, // Live FDE length.
            20, 0, 0, 0, // Stale pointer before rewriting.
        ];

        rewrite_compacted_macho_eh_frame_cie_pointers(&input, &deltas, &mut out).unwrap();

        assert_eq!(&out[12..16], &[12, 0, 0, 0]);
    }
}

fn is_chained_rebase_relocation(rel_info: &linker_utils::elf::RelocationKindInfo) -> bool {
    rel_info.kind == RelocationKind::Absolute
        && matches!(rel_info.size, RelocationSize::ByteSize(8))
}

fn macho_addend(rel: RelocationInfo) -> i64 {
    i64::from(rel.r_symbolnum)
        .wrapping_shl(64 - 24)
        .wrapping_shr(64 - 24)
}

pub(crate) fn write<'data, A: Arch<Platform = MachO>>(
    sized_output: &mut SizedOutput,
    layout: &MachOLayout<'data>,
    incremental: &PreparedState,
) -> Result {
    timing_phase!("Write data to file");
    let existing_output_bytes_available = sized_output.existing_data_available();
    let chained_rebases = ChainedRebases::collect::<A>(layout)?;
    let (mut section_buffers, mut padding) =
        split_output_into_sections(layout, &mut sized_output.out);
    padding.fill_zero();
    zero_file_backed_zero_fill_sections(&mut section_buffers, layout);

    write_stubs(
        section_buffers.get_mut(output_section_id::PLT_GOT),
        layout,
        &chained_rebases,
    )?;
    write_got(
        section_buffers.get_mut(output_section_id::GOT),
        layout,
        &chained_rebases,
    )?;
    write_unwind_info(
        section_buffers.get_mut(output_section_id::MACHO_UNWIND_INFO),
        layout,
    )?;

    let mut writable_buckets = split_buffers_by_alignment(&mut section_buffers, layout);
    let groups_and_buffers = split_output_by_group(layout, &mut writable_buckets);
    groups_and_buffers.into_par_iter().try_for_each(
        |(group, mut buffers, group_file_offsets)| -> Result {
            verbose_timing_phase!("Write group");

            let mut symbol_writer = MachOSymbolTableWriter {
                next_strtab_offset: group.strtab_start_offset,
            };
            for file in &group.files {
                write_file::<A>(
                    file,
                    &mut buffers,
                    layout,
                    &sized_output.trace,
                    &mut symbol_writer,
                    &chained_rebases,
                    incremental,
                    existing_output_bytes_available,
                    &group_file_offsets,
                    &group.file_sizes,
                )
                .with_context(|| format!("Failed copying from {file} to output file"))?;
            }
            Ok(())
        },
    )?;

    write_code_signature(layout, sized_output)?;

    Ok(())
}

fn zero_file_backed_zero_fill_sections(
    section_buffers: &mut OutputSectionMap<&mut [u8]>,
    layout: &MachOLayout<'_>,
) {
    layout
        .output_sections
        .ids_with_info()
        .for_each(|(id, info)| {
            if is_zero_fill_section(info.section_attributes.flags) {
                section_buffers.get_mut(id).fill(0);
            }
        });
}

fn write_file<'data, A: Arch<Platform = MachO>>(
    file: &FileLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    _trace: &TraceOutput,
    symbol_writer: &mut MachOSymbolTableWriter,
    chained_rebases: &ChainedRebases,
    incremental: &PreparedState,
    existing_output_bytes_available: bool,
    group_file_offsets: &OutputSectionPartMap<usize>,
    group_file_sizes: &OutputSectionPartMap<usize>,
) -> Result {
    match file {
        FileLayout::Object(s) => {
            write_object::<A>(
                s,
                buffers,
                layout,
                symbol_writer,
                chained_rebases,
                incremental,
                existing_output_bytes_available,
                group_file_offsets,
                group_file_sizes,
            )?;
        }
        FileLayout::Prelude(s) => {
            write_prelude::<A>(s, buffers, layout, symbol_writer, chained_rebases)?;
        }
        _ => {
            // TODO
        }
    }
    Ok(())
}

fn write_prelude<'data, A: Arch<Platform = MachO>>(
    prelude: &PreludeLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
    chained_rebases: &ChainedRebases,
) -> Result {
    verbose_timing_phase!("Write prelude");

    let header: &mut FileHeader = from_bytes_mut(buffers.get_mut(part_id::FILE_HEADER))
        .map_err(|_| error!("Invalid file header allocation"))?
        .0;
    populate_file_header::<A>(layout, &prelude.header_info, header)?;

    write_segment_commands::<A>(layout, buffers)?;

    if !layout.args().is_dynamiclib {
        let entry_point_command: &mut EntryPointCommand =
            from_bytes_mut(buffers.get_mut(part_id::ENTRY_POINT))
                .map_err(|_| error!("Invalid ENTRY_POINT command allocation"))?
                .0;
        write_entry_point_command::<A>(layout, entry_point_command)?;
    }

    let build_version_command: &mut BuildVersionCommand =
        from_bytes_mut(buffers.get_mut(part_id::BUILD_VERSION))
            .map_err(|_| error!("Invalid BUILD_VERSION command allocation"))?
            .0;
    write_build_version_command(layout, build_version_command);

    let uuid_command: &mut UuidCommand = from_bytes_mut(buffers.get_mut(part_id::UUID_COMMAND))
        .map_err(|_| error!("Invalid UUID_COMMAND allocation"))?
        .0;
    write_uuid_command(uuid_command);

    write_load_dylib_commands(layout, buffers.get_mut(part_id::LIBSYSTEM))?;

    if layout.args().is_dynamiclib {
        let (id_dylib_command, id_dylib_path_buffer): (&mut DylibCommand, &mut [u8]) =
            from_bytes_mut(buffers.get_mut(part_id::ID_DYLIB))
                .map_err(|_| error!("Invalid ID_DYLIB command allocation"))?;
        write_id_dylib_command(layout, id_dylib_command, id_dylib_path_buffer);
    }

    if !layout.args().is_dynamiclib {
        let (dylinker_command, dylinker_path_buffer): (&mut DylinkerCommand, &mut [u8]) =
            from_bytes_mut(buffers.get_mut(part_id::INTERP))
                .map_err(|_| error!("Invalid INTERP command allocation"))?;
        write_dylinker_command::<A>(dylinker_command, dylinker_path_buffer);
    }

    let chained_fixups_command: &mut DyldChainedFixupsCommand =
        from_bytes_mut(buffers.get_mut(part_id::DYLD_CHAINED_FIXUPS))
            .map_err(|_| error!("Invalid DYLD_CHAINED_FIXUPS command allocation"))?
            .0;
    write_dyld_chained_fixups_command::<A>(layout, chained_fixups_command);

    let (symtab_command, _) = from_bytes_mut(buffers.get_mut(part_id::SYMTAB_COMMAND))
        .map_err(|_| error!("Invalid SYMTAB_COMMAND allocation"))?;
    write_symtab_command::<A>(layout, symtab_command);

    let code_signature_command: &mut CodeSignatureCommand =
        from_bytes_mut(buffers.get_mut(part_id::CODE_SIGNATURE_COMMAND))
            .map_err(|_| error!("Invalid CODE_SIGNATURE_COMMAND allocation"))?
            .0;
    write_code_signature_command::<A>(layout, code_signature_command);

    let chained_fixup_table = buffers.get_mut(part_id::CHAINED_FIXUP_TABLE);
    write_chained_fixup_table(chained_fixup_table, layout, chained_rebases)?;

    // Fill up one extra character as n_strx == 0 is treated as unnamed.
    buffers.get_mut(part_id::STRTAB).fill(0);
    write_internal_symbols(prelude, buffers, layout, symbol_writer)?;

    Ok(())
}

fn populate_file_header<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    _header_info: &HeaderInfo,
    header: &mut FileHeader,
) -> Result {
    let load_commands_info = get_segment_sections(layout, SegmentType::LoadCommands)
        .ok_or_else(|| error!("LoadCommands segment is mandatory"))?;

    header.magic = U32::new(BigEndian, MH_CIGAM_64);
    header.cputype = U32::new(LE, CPU_TYPE_ARM64);
    header.cpusubtype = U32::new(LE, 0);
    header.filetype = U32::new(
        LE,
        if layout.args().is_dynamiclib {
            MH_DYLIB
        } else {
            MH_EXECUTE
        },
    );
    // TODO: a cleaner way how to filter out sections being part of the final output?
    let command_sizes = load_commands_info
        .segment_sections
        .iter()
        .filter(|s| s.0.mem_size > 0)
        .map(|s| s.0.mem_size)
        .sum::<u64>();
    let output_load_command_count = load_commands_info
        .segment_sections
        .iter()
        .filter(|s| s.0.mem_size > 0)
        .count();
    let extra_load_dylib_commands = load_dylib_command_count(layout.args()).saturating_sub(1);
    header.ncmds = U32::new(
        LE,
        (output_load_command_count + extra_load_dylib_commands) as u32,
    );
    header.sizeofcmds = U32::new(LE, command_sizes as u32);
    let mut flags = macho::MH_DYLDLINK | macho::MH_NOUNDEFS | macho::MH_TWOLEVEL;
    if layout.args().is_dynamiclib {
        flags |= macho::MH_NO_REEXPORTED_DYLIBS;
    } else {
        flags |= macho::MH_PIE;
    }
    if layout
        .section_layouts
        .get(output_section_id::MACHO_THREAD_VARS)
        .mem_size
        > 0
    {
        flags |= macho::MH_HAS_TLV_DESCRIPTORS;
    }
    header.flags = U32::new(LE, flags);
    header.reserved = U32::new(LE, 0);
    Ok(())
}

fn split_segment_command_buffer<'out>(
    bytes: &'out mut [u8],
    segment_name: &str,
    section_count: usize,
) -> Result<(&'out mut SegmentCommand, &'out mut [SectionEntry])> {
    let (command, rest) =
        from_bytes_mut(bytes).map_err(|_| error!("Invalid segment command allocation"))?;
    let (sections, rest) = slice_from_bytes_mut(rest, section_count)
        .map_err(|_| error!("Invalid segment section allocation"))?;
    ensure!(
        rest.is_empty(),
        "Trailing bytes in {segment_name} segment command allocation for {section_count} sections: {} bytes",
        rest.len()
    );
    Ok((command, sections))
}

fn write_segment_commands<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let pagezero_segment =
        split_segment_command_buffer(buffers.get_mut(part_id::PAGEZERO_SEGMENT), SEG_PAGEZERO, 0)?
            .0;
    write_segment(
        layout,
        part_id::PAGEZERO_SEGMENT,
        SEG_PAGEZERO,
        pagezero_segment,
        0,
        0,
        0,
        MACHO_START_MEM_ADDRESS,
        0,
    );

    let text_segment_sections = get_segment_sections(layout, SegmentType::TextSections)
        .ok_or_else(|| error!("TextSections segment is mandatory"))?
        .segment_sections;
    // The __TEXT segment in the layout includes also all the commands!
    let text_segment_size = get_segment_sections(layout, SegmentType::Text)
        .ok_or_else(|| error!("Text segment is mandatory"))?
        .segment_size;
    let (text_segment, text_sections) = split_segment_command_buffer(
        buffers.get_mut(part_id::TEXT_SEGMENT),
        SEG_TEXT,
        text_segment_sections.len(),
    )?;
    write_segment(
        layout,
        part_id::TEXT_SEGMENT,
        SEG_TEXT,
        text_segment,
        text_segment_size.file_offset as u64,
        text_segment_size.file_size as u64,
        text_segment_size.mem_offset,
        text_segment_size.mem_size,
        text_segment_sections.len(),
    );
    write_sections(SEG_TEXT, text_sections, &text_segment_sections)?;

    if let Some(data_segment_info) = get_segment_sections(layout, SegmentType::DataSections) {
        let data_segment_sections = data_segment_info.segment_sections;
        let data_segment_size = data_segment_info.segment_size;
        let (data_segment, data_sections) = split_segment_command_buffer(
            buffers.get_mut(part_id::DATA_SEGMENT),
            SEG_DATA,
            data_segment_sections.len(),
        )?;
        write_segment(
            layout,
            part_id::DATA_SEGMENT,
            SEG_DATA,
            data_segment,
            data_segment_size.file_offset as u64,
            data_segment_size.file_size as u64,
            data_segment_size.mem_offset,
            data_segment_size.mem_size,
            data_segment_sections.len(),
        );
        write_sections(SEG_DATA, data_sections, &data_segment_sections)?;
    }

    let linkedit_segment_size = get_segment_sections(layout, SegmentType::LinkeditSections)
        .ok_or_else(|| error!("LinkeditSections segment is mandatory"))?
        .segment_size;
    let linkedit_file_size = total_file_size(layout)
        .checked_sub(linkedit_segment_size.file_offset as u64)
        .ok_or_else(|| error!("Invalid __LINKEDIT file offset"))?;
    let linkedit_segment =
        split_segment_command_buffer(buffers.get_mut(part_id::LINK_EDIT_SEGMENT), SEG_LINKEDIT, 0)?
            .0;
    write_segment(
        layout,
        part_id::LINK_EDIT_SEGMENT,
        SEG_LINKEDIT,
        linkedit_segment,
        linkedit_segment_size.file_offset as u64,
        linkedit_file_size,
        linkedit_segment_size.mem_offset,
        linkedit_file_size,
        // The sections in the __LINKEDIT are "hidden".
        0,
    );

    Ok(())
}

fn total_file_size(layout: &MachOLayout<'_>) -> u64 {
    let mut file_size = 0;
    layout
        .section_layouts
        .for_each(|_, section| file_size = file_size.max(section.file_offset + section.file_size));
    file_size as u64
}

fn write_segment(
    layout: &MachOLayout,
    part_id: part_id::PartId,
    seg_name: &str,
    segment_cmd: &mut SegmentCommand,
    file_offset: u64,
    file_size: u64,
    mem_offset: u64,
    mem_size: u64,
    section_count: usize,
) {
    let prot_flags = layout
        .output_sections
        .section_flags(part_id.output_section_id())
        .raw();

    segment_cmd.cmd.set(LE, LC_SEGMENT_64);
    segment_cmd.cmdsize.set(
        LE,
        (size_of::<SegmentCommand>() + size_of::<SectionEntry>() * section_count) as u32,
    );
    segment_cmd.segname[..seg_name.len()].copy_from_slice(seg_name.as_bytes());
    segment_cmd.segname[seg_name.len()..].zero();
    segment_cmd.fileoff.set(LE, file_offset);
    segment_cmd.filesize.set(LE, file_size);
    segment_cmd.vmaddr.set(LE, mem_offset);
    segment_cmd.vmsize.set(LE, mem_size);
    segment_cmd.maxprot.set(LE, prot_flags);
    segment_cmd.initprot.set(LE, prot_flags);
    segment_cmd.nsects.set(LE, section_count as u32);
    segment_cmd.flags.set(LE, 0);
}

fn write_sections(
    seg_name: &str,
    sections: &mut [SectionEntry],
    segment_sections: &[(
        OutputRecordLayout,
        Option<SectionName<'_>>,
        crate::macho::SectionFlags,
    )],
) -> Result {
    for (section, (size, section_name, section_flags)) in sections.iter_mut().zip(segment_sections)
    {
        let section_name = section_name
            .ok_or_else(|| error!("section name must be known"))?
            .0;

        section.segname[..seg_name.len()].copy_from_slice(seg_name.as_bytes());
        section.segname[seg_name.len()..].zero();
        section.sectname[..section_name.len()].copy_from_slice(section_name);
        section.sectname[section_name.len()..].zero();
        section.addr.set(LE, size.mem_offset);
        section.size.set(LE, size.mem_size);
        section.offset.set(
            LE,
            if is_zero_fill_section(*section_flags) {
                0
            } else {
                size.file_offset as u32
            },
        );
        section.align.set(LE, u32::from(size.alignment.exponent));
        section.reloff.set(LE, 0);
        section.nreloc.set(LE, 0);
        section.flags.set(LE, section_flags.raw());
        section.reserved1.set(LE, 0);
        section.reserved2.set(
            LE,
            if section_name == b"__stubs" {
                crate::macho::MACHO_STUB_SIZE as u32
            } else {
                0
            },
        );
        section.reserved3.set(LE, 0);
    }

    Ok(())
}

fn write_object<'data, A: Arch<Platform = MachO>>(
    object: &ObjectLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
    chained_rebases: &ChainedRebases,
    incremental: &PreparedState,
    existing_output_bytes_available: bool,
    group_file_offsets: &OutputSectionPartMap<usize>,
    group_file_sizes: &OutputSectionPartMap<usize>,
) -> Result {
    verbose_timing_phase!("Write object", file_id = object.file_id.as_u32());

    let _span = debug_span!("write_file", filename = %object.input).entered();
    let _file_span = layout.args().common().trace_span_for_file(object.file_id);
    for (i, sec) in object.sections.iter().enumerate() {
        match sec {
            SectionSlot::Loaded(sec) => {
                write_object_section::<A>(
                    object,
                    layout,
                    sec,
                    object::SectionIndex(i),
                    buffers,
                    chained_rebases,
                    incremental,
                    existing_output_bytes_available,
                    group_file_offsets,
                    group_file_sizes,
                )?;
            }
            _ => (),
        }
    }

    write_symbols(object, buffers, layout, symbol_writer)?;
    if object.owns_thunk_block
        && let Some(addresses) = layout
            .thunk_block_addresses
            .get(object.thunk_block_id.as_usize())
    {
        let part_id = layout.thunk_block_part_ids[object.thunk_block_id.as_usize()];
        write_thunks::<A>(addresses, part_id, buffers, layout)?;
    }
    for &block_id in &object.extra_thunk_block_ids {
        if let Some(addresses) = layout.thunk_block_addresses.get(block_id.as_usize()) {
            let part_id = layout.thunk_block_part_ids[block_id.as_usize()];
            write_thunks::<A>(addresses, part_id, buffers, layout)?;
        }
    }

    Ok(())
}

fn write_thunks<A: Arch<Platform = MachO>>(
    thunk_addresses: &std::collections::BTreeMap<SymbolId, u64>,
    part_id: crate::part_id::PartId,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'_>,
) -> Result {
    let Some(config) = A::thunk_config() else {
        return Ok(());
    };
    let thunk_size = config.thunk_size as usize;
    if thunk_addresses.is_empty() {
        return Ok(());
    }

    let buffer = buffers.get_mut(part_id);
    let raw_size = thunk_addresses.len() * thunk_size;
    let allocation_size = part_id
        .alignment(&layout.output_sections)
        .align_up_usize(raw_size);
    for (&symbol_id, &thunk_address) in thunk_addresses {
        ensure!(
            thunk_address != 0,
            "Mach-O thunk address was not allocated for {}",
            layout.symbol_db.symbol_name_for_display(symbol_id)
        );
        let resolution = layout
            .merged_symbol_resolution(symbol_id)
            .with_context(|| {
                format!(
                    "No resolution for Mach-O thunk target {}",
                    layout.symbol_db.symbol_name_for_display(symbol_id)
                )
            })?;
        let target_address = resolution
            .format_specific
            .stub_address
            .map_or(resolution.raw_value, |address| address.get());
        let thunk_buf = buffer
            .split_off_mut(..thunk_size)
            .ok_or_else(|| crate::file_writer::insufficient_allocation("Mach-O thunk space"))?;
        A::write_thunk(thunk_address, target_address, thunk_buf);
    }
    let padding_size = allocation_size - raw_size;
    if padding_size > 0 {
        buffer
            .split_off_mut(..padding_size)
            .ok_or_else(|| crate::file_writer::insufficient_allocation("Mach-O thunk space"))?;
    }
    Ok(())
}

fn write_object_section<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
    section: &Section,
    section_index: object::SectionIndex,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    chained_rebases: &ChainedRebases,
    incremental: &PreparedState,
    existing_output_bytes_available: bool,
    group_file_offsets: &OutputSectionPartMap<usize>,
    group_file_sizes: &OutputSectionPartMap<usize>,
) -> Result {
    let relocations = object_layout.relocations(section_index)?;
    let record_for_reuse = !layout.args().should_output_partial_object()
        && relocations.num_relocations() == 0
        && object_layout
            .section_relax_deltas
            .get(section_index.0)
            .is_none()
        && !section.flags.needs_got()
        && !section.flags.needs_plt();
    let can_reuse_existing_bytes = existing_output_bytes_available && record_for_reuse;

    let written = write_section_raw(
        object_layout,
        layout,
        section,
        section_index,
        buffers,
        incremental,
        record_for_reuse,
        can_reuse_existing_bytes,
        group_file_offsets,
        group_file_sizes,
    )?;
    if written.reused {
        return Ok(());
    }
    let out = written.bytes;

    let section_address = object_layout.section_resolutions[section_index.0]
        .address()
        .context("Attempted to apply relocations to a section that we didn't load")?;
    let section_part_id =
        object_layout.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    let relax_deltas = object_layout.section_relax_deltas.get(section_index.0);
    let mut paired_addend = 0;
    let mut paired_subtractor = None;
    let section_header = object_layout.object.section(section_index)?;
    let is_eh_frame = object_layout.object.section_name(section_header)? == b"__eh_frame";
    let live_unwind_ranges =
        macho_writer_live_unwind_relocation_ranges(object_layout, layout, section_index)?;
    let live_subsection_ranges =
        macho_writer_live_subsection_relocation_ranges(object_layout, layout, section_index)?;
    for rel in relocations.relocations {
        let mut rel = rel.info(LE);
        let input_offset = u64::from(rel.r_address);
        if relax_deltas.is_some_and(|deltas| deltas.deletes_input_offset(input_offset)) {
            continue;
        }
        if live_unwind_ranges.as_ref().is_some_and(|ranges| {
            !ranges
                .iter()
                .any(|range| range.contains(&(input_offset as usize)))
        }) {
            paired_addend = 0;
            paired_subtractor = None;
            continue;
        }
        if live_subsection_ranges.as_ref().is_some_and(|ranges| {
            !ranges
                .iter()
                .any(|range| range.contains(&(input_offset as usize)))
        }) {
            paired_addend = 0;
            paired_subtractor = None;
            continue;
        }
        let output_offset = opt_input_to_output(relax_deltas, input_offset);
        rel.r_address = output_offset
            .try_into()
            .context("Compacted Mach-O relocation offset exceeds u32")?;
        if rel.r_type == macho::ARM64_RELOC_ADDEND {
            paired_addend = macho_addend(rel);
            continue;
        }
        if rel.r_type == macho::ARM64_RELOC_SUBTRACTOR {
            ensure!(
                paired_subtractor.replace(rel).is_none(),
                "Mach-O ARM64_RELOC_SUBTRACTOR must be followed by ARM64_RELOC_UNSIGNED"
            );
            continue;
        }
        if is_tlv_init_relocation(section_header, rel) {
            apply_tlv_init_relocation::<A>(object_layout, rel, paired_addend, layout, out)?;
            paired_addend = 0;
            continue;
        }
        if let Some(subtractor) = paired_subtractor.take() {
            let eh_frame_addend_adjustment = if is_eh_frame {
                macho_eh_frame_subtractor_addend_adjustment(
                    object_layout,
                    section_index,
                    relax_deltas,
                    subtractor,
                    input_offset,
                    output_offset,
                )?
            } else {
                0
            };
            apply_subtractor_relocation::<A>(
                object_layout,
                subtractor,
                rel,
                paired_addend,
                eh_frame_addend_adjustment,
                layout,
                out,
            )?;
            paired_addend = 0;
            continue;
        }
        apply_relocation::<A>(
            object_layout,
            section_address,
            section_part_id,
            rel,
            paired_addend,
            layout,
            out,
            chained_rebases,
        )?;
        paired_addend = 0;
    }
    ensure!(
        paired_subtractor.is_none(),
        "Mach-O ARM64_RELOC_SUBTRACTOR missing paired ARM64_RELOC_UNSIGNED"
    );

    Ok(())
}

fn macho_eh_frame_subtractor_addend_adjustment<'data>(
    object_layout: &ObjectLayout<'data, MachO>,
    section_index: object::SectionIndex,
    relax_deltas: Option<&SectionRelaxDeltas>,
    subtractor: RelocationInfo,
    input_offset: u64,
    output_offset: u64,
) -> Result<i64> {
    // __eh_frame pcrel fields use subtractor addends based on their input field position. Keep the
    // same target when compaction moves the field differently than the subtractor symbol.
    let field_deleted = input_offset - output_offset;
    let subtractor_input_offset = if subtractor.r_extern {
        let symbol_index = SymbolIndex(subtractor.r_symbolnum as usize);
        let symbol = object_layout.object.symbol(symbol_index)?;
        if object_layout.object.symbol_section(symbol, symbol_index)? == Some(section_index) {
            Some(
                object_layout
                    .object
                    .symbol_offset_in_section(symbol, section_index)?,
            )
        } else {
            None
        }
    } else if subtractor.r_symbolnum == section_index.0 as u32 + 1 {
        Some(0)
    } else {
        None
    };
    let subtractor_deleted = subtractor_input_offset.map_or(0, |input_offset| {
        input_offset - opt_input_to_output(relax_deltas, input_offset)
    });
    (i128::from(field_deleted) - i128::from(subtractor_deleted))
        .try_into()
        .context("Compacted Mach-O __eh_frame subtractor addend adjustment exceeds i64")
}

fn is_tlv_init_relocation(section: &SectionEntry, rel: RelocationInfo) -> bool {
    section.name() == b"__thread_vars"
        && rel.r_type == macho::ARM64_RELOC_UNSIGNED
        && rel.r_address % 24 == 16
}

fn is_zero_fill_section(section_flags: crate::macho::SectionFlags) -> bool {
    matches!(
        section_flags.raw() & macho::SECTION_TYPE,
        macho::S_ZEROFILL | macho::S_GB_ZEROFILL | macho::S_THREAD_LOCAL_ZEROFILL
    )
}

#[inline(always)]
fn apply_tlv_init_relocation<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    rel: RelocationInfo,
    paired_addend: i64,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
) -> Result {
    let offset_in_section = rel.r_address as usize;
    ensure!(
        offset_in_section + size_of::<u64>() <= out.len(),
        "Mach-O TLV descriptor relocation is outside __thread_vars"
    );

    let rel_info = A::relocation_from_raw(rel)?;
    ensure!(
        rel_info.kind == RelocationKind::Absolute
            && matches!(rel_info.size, RelocationSize::ByteSize(8)),
        "Mach-O TLV descriptor initializer must use an 8-byte absolute relocation"
    );

    let (mut resolution, _) = get_resolution(rel, object_layout, layout)?;
    resolution.raw_value = resolution.raw_value.wrapping_add(paired_addend as u64);
    let thread_data_start = layout
        .section_layouts
        .get(output_section_id::TDATA)
        .mem_offset;
    let offset = resolution
        .raw_value
        .checked_sub(thread_data_start)
        .with_context(|| {
            format!(
                "Mach-O TLV initializer target {:#x} precedes __thread_data at {:#x}",
                resolution.raw_value, thread_data_start
            )
        })?;
    write_u64(out, offset_in_section, offset)
}

fn apply_subtractor_relocation<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    subtractor: RelocationInfo,
    rel: RelocationInfo,
    paired_addend: i64,
    addend_adjustment: i64,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
) -> Result {
    ensure!(
        rel.r_type == macho::ARM64_RELOC_UNSIGNED,
        "Mach-O ARM64_RELOC_SUBTRACTOR must be followed by ARM64_RELOC_UNSIGNED"
    );
    ensure!(
        subtractor.r_address == rel.r_address,
        "Mach-O ARM64_RELOC_SUBTRACTOR pair addresses differ"
    );

    let rel_info = A::relocation_from_raw(rel)?;
    let offset_in_section = rel.r_address as usize;
    let addend = read_relocation_addend(out, offset_in_section, rel_info.size)?;
    let (positive, _) = get_resolution(rel, object_layout, layout)?;
    let (negative, _) = get_resolution(subtractor, object_layout, layout)?;
    let value = positive
        .raw_value
        .wrapping_add(addend)
        .wrapping_add(paired_addend as u64)
        .wrapping_add(addend_adjustment as u64)
        .wrapping_sub(negative.raw_value);

    rel_info.write_to_buffer(value, &mut out[offset_in_section..])?;
    Ok(())
}

fn read_relocation_addend(out: &[u8], offset: usize, size: RelocationSize) -> Result<u64> {
    let bytes = match size {
        RelocationSize::ByteSize(bytes @ (1 | 2 | 4 | 8)) => bytes,
        _ => bail!("Unsupported Mach-O subtractor relocation size {size}"),
    };
    let input = out
        .get(offset..offset + bytes)
        .ok_or_else(|| error!("Read past end of Mach-O subtractor relocation"))?;
    let mut value = [0u8; size_of::<u64>()];
    value[..bytes].copy_from_slice(input);
    Ok(u64::from_le_bytes(value))
}

#[inline(always)]
fn apply_relocation<'data, A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'data, MachO>,
    section_address: u64,
    section_part_id: part_id::PartId,
    rel: RelocationInfo,
    paired_addend: i64,
    layout: &MachOLayout<'data>,
    out: &mut [u8],
    chained_rebases: &ChainedRebases,
) -> Result {
    let offset_in_section = u64::from(rel.r_address);
    let place = section_address + offset_in_section;

    let _span = tracing::trace_span!(
        "relocation",
        address = place,
        address_hex = %HexU64::new(place)
    )
    .entered();

    let rel_info = A::relocation_from_raw(rel)?;
    let (mut resolution, local_symbol_id) = get_resolution(rel, object_layout, layout)?;
    let implicit_addend = if rel.r_type == macho::ARM64_RELOC_UNSIGNED {
        read_relocation_addend(out, offset_in_section as usize, rel_info.size)?
    } else {
        0
    };
    resolution.raw_value = resolution
        .raw_value
        .wrapping_add(implicit_addend)
        .wrapping_add(paired_addend as u64);

    let raw_value = resolution.raw_value;
    let got_load_relaxed_to_direct = matches!(
        rel.r_type,
        macho::ARM64_RELOC_GOT_LOAD_PAGE21 | macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12
    ) && resolution.format_specific.got_address.is_none();
    let uses_tlv_got = matches!(
        rel.r_type,
        macho::ARM64_RELOC_TLVP_LOAD_PAGE21 | macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12
    ) && resolution.format_specific.got_address.is_some();
    let target_value = match rel_info.kind {
        RelocationKind::Got | RelocationKind::GotRelative => resolution
            .format_specific
            .got_address
            .map_or(raw_value, |address| address.get())
            .wrapping_add(paired_addend as u64),
        RelocationKind::Relative if uses_tlv_got => resolution
            .format_specific
            .got_address
            .map_or(raw_value, |address| address.get()),
        RelocationKind::PltRelative => resolution
            .format_specific
            .stub_address
            .map_or(raw_value, |address| address.get()),
        RelocationKind::Relative
            if rel.r_type == macho::ARM64_RELOC_BRANCH26
                && resolution.format_specific.stub_address.is_some() =>
        {
            resolution.format_specific.stub_address.unwrap().get()
        }
        _ => raw_value,
    };

    let target_name = local_symbol_id.map_or_else(
        || format!("section ordinal {}", rel.r_symbolnum),
        |symbol_id| {
            layout
                .symbol_db
                .symbol_name_for_display(symbol_id)
                .to_string()
        },
    );

    let mask = get_page_mask(rel_info.mask);
    let mut value = match rel_info.kind {
        RelocationKind::Absolute | RelocationKind::Got => {
            target_value.bitand(mask.symbol_plus_addend)
        }
        RelocationKind::AbsoluteLowPart if uses_tlv_got => resolution
            .format_specific
            .got_address
            .map_or(target_value, |address| address.get())
            .bitand(mask.symbol_plus_addend),
        RelocationKind::AbsoluteLowPart => target_value.bitand(mask.symbol_plus_addend),
        RelocationKind::Relative | RelocationKind::GotRelative | RelocationKind::PltRelative => {
            target_value
                .bitand(mask.symbol_plus_addend)
                .wrapping_sub(place.bitand(mask.place))
        }
        _ => todo!(),
    };
    if let Some(local_symbol_id) = local_symbol_id
        && let Some(thunked_value) = maybe_get_thunk_for_relocation::<A>(
            object_layout,
            layout,
            section_part_id,
            rel_info,
            local_symbol_id,
            place,
            value,
        )?
    {
        value = thunked_value;
    }
    if let Some(import_index) = chained_rebases.bind_import_index(place) {
        value = encode_chained_bind(import_index, chained_rebases.next_stride(place)?)?;
    } else if chained_rebases.contains(place) {
        value = encode_chained_rebase(value, chained_rebases.next_stride(place)?).with_context(
            || format!("Failed to encode Mach-O chained rebase at {place:#x} for {target_name}"),
        )?;
    }

    if (rel.r_type == macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 && !uses_tlv_got)
        || (rel.r_type == macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 && got_load_relaxed_to_direct)
    {
        rewrite_pageoff_load_to_add(&mut out[offset_in_section as usize..])?;
    }

    tracing::trace!(
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %target_name,
            "relocation applied");

    rel_info
        .write_to_buffer(value, &mut out[offset_in_section as usize..])
        .with_context(|| {
            format!(
                "failed to apply Mach-O relocation type {} at offset {:#x} against {target_name}",
                rel.r_type, offset_in_section
            )
        })?;

    Ok(())
}

fn maybe_get_thunk_for_relocation<A: Arch<Platform = MachO>>(
    object_layout: &ObjectLayout<'_, MachO>,
    layout: &MachOLayout<'_>,
    section_part_id: part_id::PartId,
    rel_info: linker_utils::elf::RelocationKindInfo,
    local_symbol_id: SymbolId,
    place: u64,
    value: u64,
) -> Result<Option<u64>> {
    let Some(config) = A::thunk_config() else {
        return Ok(None);
    };

    if !rel_info.thunkable || rel_info.range.contains(value as i64) {
        return Ok(None);
    }

    let canonical_id = layout.symbol_db.definition(local_symbol_id);
    let thunk_id = crate::thunks::block_id_for_source_part(
        object_layout,
        &layout.thunk_block_part_ids,
        section_part_id,
        config.primary_function_part_id,
    );

    let thunk_address = layout
        .thunk_block_addresses
        .get(thunk_id.as_usize())
        .and_then(|addresses| addresses.get(&canonical_id))
        .copied()
        .with_context(|| {
            format!(
                "Mach-O branch relocation out of range by {} for symbol {}, but no thunk was allocated",
                rel_info.range.overrun(value as i64),
                layout.symbol_db.symbol_name_for_display(local_symbol_id),
            )
        })?;

    ensure!(
        thunk_address != 0,
        "Mach-O thunk address was not allocated for {}",
        layout.symbol_db.symbol_name_for_display(local_symbol_id)
    );

    let mask = get_page_mask(rel_info.mask);
    let new_value = thunk_address
        .wrapping_add(rel_info.bias)
        .bitand(mask.symbol_plus_addend)
        .wrapping_sub(place.bitand(mask.place));

    tracing::trace!(
        old_value = value,
        new_value,
        thunk_address,
        "Using Mach-O thunk instead of out-of-range branch"
    );

    Ok(Some(new_value))
}

fn rewrite_pageoff_load_to_add(out: &mut [u8]) -> Result {
    ensure!(
        out.len() >= 4,
        "Mach-O pageoff relocation must have a 4-byte instruction"
    );
    let insn = read_u32(out, 0)?;
    // ld64 relaxes pageoff loads from:
    //     ldr Xt, [Xn, #imm]
    // to:
    //     add Xt, Xn, #imm
    // so the register receives the target address instead of loading through an indirection slot.
    let rewritten = 0x9100_0000 | (insn & 0x003f_ffff);
    write_u32(out, 0, rewritten)
}

fn encode_chained_rebase(value: u64, next_stride: u64) -> Result<u64> {
    let runtime_offset = value
        .checked_sub(MACHO_START_MEM_ADDRESS)
        .with_context(|| format!("Cannot encode Mach-O chained rebase target {value:#x}"))?;
    ensure!(
        runtime_offset < (1 << 36),
        "Mach-O chained rebase target {runtime_offset:#x} exceeds 36 bits"
    );
    ensure!(
        next_stride < 0x1000,
        "Mach-O chained rebase next stride {next_stride:#x} exceeds 12 bits"
    );
    Ok(runtime_offset | (next_stride << 51))
}

fn encode_chained_bind(import_index: usize, next_stride: u64) -> Result<u64> {
    ensure!(
        import_index < (1 << 24),
        "Mach-O chained bind import index {import_index} exceeds 24 bits"
    );
    ensure!(
        next_stride < 0x1000,
        "Mach-O chained bind next stride {next_stride:#x} exceeds 12 bits"
    );
    Ok(import_index as u64 | (next_stride << 51) | (1 << 63))
}

fn write_stubs(out: &mut [u8], layout: &MachOLayout, chained_rebases: &ChainedRebases) -> Result {
    out.fill(0);
    if out.is_empty() {
        return Ok(());
    }

    let stubs_start = layout
        .section_layouts
        .get(output_section_id::PLT_GOT)
        .mem_offset;
    let mut imports = chained_rebases
        .imports
        .iter()
        .filter_map(|import| import.stub_address.zip(import.got_address))
        .collect::<Vec<_>>();
    imports.sort_by_key(|(stub_address, _)| *stub_address);

    for (stub_address, got_address) in imports {
        let offset = stub_address
            .checked_sub(stubs_start)
            .with_context(|| format!("Mach-O stub address {stub_address:#x} precedes __stubs"))?
            as usize;
        ensure!(
            offset + crate::macho::MACHO_STUB_SIZE as usize <= out.len(),
            "Mach-O stub at {stub_address:#x} is outside __stubs"
        );

        write_u32(out, offset, encode_adrp_x16(stub_address, got_address)?)?;
        write_u32(out, offset + 4, encode_ldr_x16_from_x16(got_address)?)?;
        write_u32(out, offset + 8, 0xd61f0200)?;
    }

    Ok(())
}

fn encode_adrp_x16(place: u64, target: u64) -> Result<u32> {
    let place_page = place & !0xfff;
    let target_page = target & !0xfff;
    let page_delta = (target_page as i64 - place_page as i64) >> 12;
    ensure!(
        (-(1 << 20)..(1 << 20)).contains(&page_delta),
        "Mach-O stub target {target:#x} is out of ADRP range from {place:#x}"
    );
    let imm = (page_delta as u64) & 0x1f_ffff;
    let immlo = imm & 0x3;
    let immhi = (imm >> 2) & 0x7_ffff;
    Ok(0x9000_0000 | 16 | ((immlo as u32) << 29) | ((immhi as u32) << 5))
}

fn encode_ldr_x16_from_x16(target: u64) -> Result<u32> {
    let page_offset = target & 0xfff;
    ensure!(
        page_offset.is_multiple_of(8),
        "Mach-O GOT address {target:#x} is not 8-byte aligned"
    );
    let imm12 = page_offset / 8;
    Ok(0xf940_0000 | ((imm12 as u32) << 10) | (16 << 5) | 16)
}

fn write_got(out: &mut [u8], layout: &MachOLayout, chained_rebases: &ChainedRebases) -> Result {
    out.fill(0);
    if out.is_empty() {
        return Ok(());
    }

    let got_start = layout
        .section_layouts
        .get(output_section_id::GOT)
        .mem_offset;
    for entry in &chained_rebases.got_entries {
        let offset =
            entry.address.checked_sub(got_start).with_context(|| {
                format!("Mach-O GOT address {:#x} precedes __got", entry.address)
            })? as usize;
        ensure!(
            offset + crate::macho::MACHO_GOT_ENTRY_SIZE as usize <= out.len(),
            "Mach-O GOT entry at {:#x} is outside __got",
            entry.address
        );
        let next_stride = chained_rebases.next_stride(entry.address)?;
        let value = if let Some(import_index) = entry.import_index {
            encode_chained_bind(import_index, next_stride)?
        } else {
            encode_chained_rebase(entry.target, next_stride).with_context(|| {
                format!(
                    "Failed to encode Mach-O chained rebase for GOT entry at {:#x} for {}",
                    entry.address, entry.name
                )
            })?
        };
        write_u64(out, offset, value)?;
    }

    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct CompactUnwindEntry {
    function_address: u64,
    length: u32,
    encoding: u32,
    personality: u64,
    lsda: u64,
}

#[derive(Clone, Copy, Debug)]
struct UnwindInfoEntry {
    function_offset: u32,
    length: u32,
    encoding: u32,
    personality_offset: Option<u32>,
    lsda_offset: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
struct FdeInfo {
    output_offset: u64,
    length: u32,
    personality_offset: Option<u32>,
    lsda_offset: Option<u32>,
}

const UNWIND_ARM64_MODE_MASK: u32 = 0x0f00_0000;
const UNWIND_ARM64_MODE_DWARF: u32 = 0x0300_0000;
const UNWIND_DWARF_OFFSET_MASK: u32 = 0x00ff_ffff;
const UNWIND_PERSONALITY_MASK: u32 = 0x3000_0000;
const UNWIND_HAS_LSDA: u32 = 0x4000_0000;
const UNWIND_COMMON_ENCODINGS: [u32; 3] = [0x0200_0000, 0x0400_0000, 0];
const MAX_UNWIND_PERSONALITIES: usize = 3;

fn write_unwind_info(out: &mut [u8], layout: &MachOLayout<'_>) -> Result {
    out.fill(0);
    if out.is_empty() {
        return Ok(());
    }

    let mut entries = collect_unwind_info_entries(layout)?;
    if entries.is_empty() {
        return Ok(());
    }

    entries.sort_by_key(|entry| entry.function_offset);
    entries.dedup_by_key(|entry| entry.function_offset);

    let text = layout.section_layouts.get(output_section_id::TEXT);
    let text_end = macho_image_offset(text.mem_offset + text.mem_size)?;
    add_unwind_info_gap_entries(&mut entries, text_end);

    let mut personalities = Vec::new();
    for entry in &mut entries {
        if let Some(personality_offset) = entry.personality_offset {
            let personality_index = if let Some(index) =
                personalities.iter().position(|p| *p == personality_offset)
            {
                index
            } else {
                ensure!(
                    personalities.len() < MAX_UNWIND_PERSONALITIES,
                    "Mach-O __unwind_info supports at most {MAX_UNWIND_PERSONALITIES} personalities"
                );
                personalities.push(personality_offset);
                personalities.len() - 1
            };
            entry.encoding = (entry.encoding & !UNWIND_PERSONALITY_MASK)
                | (((personality_index + 1) as u32) << 28);
        }
        if entry.lsda_offset.is_some() {
            entry.encoding |= UNWIND_HAS_LSDA;
        }
    }

    let page_count = entries
        .len()
        .div_ceil(MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT);
    let index_count = page_count + 1;
    let common_encodings_offset = 7 * size_of::<u32>();
    let common_encodings_count = UNWIND_COMMON_ENCODINGS.len();
    let personality_array_offset =
        common_encodings_offset + common_encodings_count * size_of::<u32>();
    let personality_array_count = personalities.len();
    let index_offset = personality_array_offset + personality_array_count * size_of::<u32>();

    let mut lsda_entries = Vec::new();
    let mut page_lsda_starts = Vec::with_capacity(page_count);
    for page_entries in entries.chunks(MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT) {
        page_lsda_starts.push(lsda_entries.len());
        lsda_entries.extend(
            page_entries
                .iter()
                .filter_map(|entry| Some((entry.function_offset, entry.lsda_offset?))),
        );
    }

    let lsda_offset = index_offset + index_count * 3 * size_of::<u32>();
    let second_level_offset = lsda_offset + lsda_entries.len() * 2 * size_of::<u32>();

    let mut page_offsets = Vec::with_capacity(page_count);
    let mut next_page_offset = second_level_offset;
    for page_entries in entries.chunks(MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT) {
        page_offsets.push(next_page_offset);
        next_page_offset +=
            size_of::<u32>() + 2 * size_of::<u16>() + page_entries.len() * 2 * size_of::<u32>();
    }

    ensure!(
        next_page_offset <= out.len(),
        "Mach-O __unwind_info allocation too small. Need {next_page_offset} bytes, got {}",
        out.len()
    );

    write_u32(out, 0, 1)?;
    write_u32(out, 4, common_encodings_offset as u32)?;
    write_u32(out, 8, common_encodings_count as u32)?;
    write_u32(out, 12, personality_array_offset as u32)?;
    write_u32(out, 16, personality_array_count as u32)?;
    write_u32(out, 20, index_offset as u32)?;
    write_u32(out, 24, index_count as u32)?;

    let mut common_encoding_write_offset = common_encodings_offset;
    for encoding in UNWIND_COMMON_ENCODINGS {
        write_u32(out, common_encoding_write_offset, encoding)?;
        common_encoding_write_offset += size_of::<u32>();
    }

    let mut personality_write_offset = personality_array_offset;
    for personality_offset in &personalities {
        write_u32(out, personality_write_offset, *personality_offset)?;
        personality_write_offset += size_of::<u32>();
    }

    let mut lsda_write_offset = lsda_offset;
    for (function_offset, lsda_offset_value) in &lsda_entries {
        write_u32(out, lsda_write_offset, *function_offset)?;
        write_u32(
            out,
            lsda_write_offset + size_of::<u32>(),
            *lsda_offset_value,
        )?;
        lsda_write_offset += 2 * size_of::<u32>();
    }

    for (page_index, page_entries) in entries
        .chunks(MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT)
        .enumerate()
    {
        let index_entry_offset = index_offset + page_index * 3 * size_of::<u32>();
        let page_offset = page_offsets[page_index];
        write_u32(out, index_entry_offset, page_entries[0].function_offset)?;
        write_u32(
            out,
            index_entry_offset + size_of::<u32>(),
            page_offset as u32,
        )?;
        write_u32(
            out,
            index_entry_offset + 2 * size_of::<u32>(),
            (lsda_offset + page_lsda_starts[page_index] * 2 * size_of::<u32>()) as u32,
        )?;

        write_u32(out, page_offset, MACHO_UNWIND_SECOND_LEVEL_REGULAR)?;
        write_u16(out, page_offset + size_of::<u32>(), 8)?;
        write_u16(
            out,
            page_offset + size_of::<u32>() + size_of::<u16>(),
            page_entries
                .len()
                .try_into()
                .context("Mach-O __unwind_info regular page entry count exceeds u16")?,
        )?;

        let mut entry_offset = page_offset + size_of::<u32>() + 2 * size_of::<u16>();
        for entry in page_entries {
            write_u32(out, entry_offset, entry.function_offset)?;
            write_u32(out, entry_offset + size_of::<u32>(), entry.encoding)?;
            entry_offset += 2 * size_of::<u32>();
        }
    }

    let last_entry_end = entries.last().map_or(text_end, |entry| {
        entry.function_offset.saturating_add(entry.length.max(1))
    });
    let sentinel_function_offset = text_end.max(last_entry_end);
    let sentinel_offset = index_offset + page_count * 3 * size_of::<u32>();
    write_u32(out, sentinel_offset, sentinel_function_offset)?;
    write_u32(out, sentinel_offset + size_of::<u32>(), 0)?;
    write_u32(
        out,
        sentinel_offset + 2 * size_of::<u32>(),
        (lsda_offset + lsda_entries.len() * 2 * size_of::<u32>()) as u32,
    )?;

    Ok(())
}

fn add_unwind_info_gap_entries(entries: &mut Vec<UnwindInfoEntry>, text_end: u32) {
    let mut gap_entries = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.encoding == 0 || entry.length == 0 {
            continue;
        }

        let entry_end = entry.function_offset.saturating_add(entry.length);
        let next_start = entries
            .get(index + 1)
            .map_or(text_end, |next| next.function_offset);
        if entry_end >= next_start {
            continue;
        }

        gap_entries.push(UnwindInfoEntry {
            function_offset: entry_end,
            length: next_start - entry_end,
            encoding: 0,
            personality_offset: None,
            lsda_offset: None,
        });
    }

    entries.extend(gap_entries);
    entries.sort_by_key(|entry| entry.function_offset);
}

fn collect_unwind_info_entries(layout: &MachOLayout<'_>) -> Result<Vec<UnwindInfoEntry>> {
    let mut entries = Vec::new();
    for group in &layout.group_layouts {
        for file in &group.files {
            let FileLayout::Object(object) = file else {
                continue;
            };

            let fde_infos = eh_frame_fde_infos(object, layout)?;

            for (section_index, section) in object.sections.iter().enumerate() {
                let SectionSlot::FrameData(_) = section else {
                    continue;
                };
                let section_index = object::SectionIndex(section_index);
                let section_header = object.object.section(section_index)?;
                if object.object.section_name(section_header)? != b"__compact_unwind" {
                    continue;
                }

                let compact_entries = read_compact_unwind_entries(object, layout, section_index)?;
                for entry in compact_entries {
                    let Some(function_offset) = entry
                        .function_address
                        .checked_sub(MACHO_START_MEM_ADDRESS)
                        .and_then(|offset| u32::try_from(offset).ok())
                    else {
                        continue;
                    };

                    let mut encoding = entry.encoding;
                    let mut personality_offset = if entry.personality == 0 {
                        None
                    } else {
                        Some(macho_image_offset(entry.personality)?)
                    };
                    let mut lsda_offset = if entry.lsda == 0 {
                        None
                    } else {
                        Some(macho_image_offset(entry.lsda)?)
                    };

                    if encoding & UNWIND_ARM64_MODE_MASK == UNWIND_ARM64_MODE_DWARF {
                        let Some(fde_info) = fde_infos
                            .as_ref()
                            .and_then(|infos| infos.get(&entry.function_address))
                            .copied()
                        else {
                            continue;
                        };
                        let output_dwarf_offset =
                            compact_unwind_dwarf_offset_hint(fde_info.output_offset);
                        encoding = (encoding & !UNWIND_DWARF_OFFSET_MASK) | output_dwarf_offset;
                        personality_offset = personality_offset.or(fde_info.personality_offset);
                        lsda_offset = lsda_offset.or(fde_info.lsda_offset);
                    }

                    entries.push(UnwindInfoEntry {
                        function_offset,
                        length: entry.length,
                        encoding,
                        personality_offset,
                        lsda_offset,
                    });
                }
            }

            if let Some(fde_infos) = &fde_infos {
                for (function_address, fde_info) in fde_infos {
                    let Some(function_offset) = function_address
                        .checked_sub(MACHO_START_MEM_ADDRESS)
                        .and_then(|offset| u32::try_from(offset).ok())
                    else {
                        continue;
                    };
                    let output_dwarf_offset =
                        compact_unwind_dwarf_offset_hint(fde_info.output_offset);
                    entries.push(UnwindInfoEntry {
                        function_offset,
                        length: fde_info.length,
                        encoding: UNWIND_ARM64_MODE_DWARF | output_dwarf_offset,
                        personality_offset: fde_info.personality_offset,
                        lsda_offset: fde_info.lsda_offset,
                    });
                }
            }
        }
    }

    Ok(entries)
}

fn compact_unwind_dwarf_offset_hint(output_offset: u64) -> u32 {
    if output_offset <= u64::from(UNWIND_DWARF_OFFSET_MASK) {
        output_offset as u32
    } else {
        // This field is only a DWARF FDE search hint. When the exact offset
        // does not fit, Darwin linkers point the unwinder at the start of
        // __eh_frame so it can scan from there.
        0
    }
}

fn eh_frame_fde_infos<'data>(
    object: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
) -> Result<Option<HashMap<u64, FdeInfo>>> {
    let Some((eh_frame_section_index, eh_frame_section)) =
        object.object.section_by_name("__eh_frame")
    else {
        return Ok(None);
    };
    let Some(eh_frame_address) = object.section_resolutions[eh_frame_section_index.0].address()
    else {
        return Ok(None);
    };

    let eh_frame_output_offset = eh_frame_address
        - layout
            .section_layouts
            .get(output_section_id::EH_FRAME)
            .mem_offset;
    let gcc_except_table = layout
        .section_layouts
        .get(output_section_id::GCC_EXCEPT_TABLE);
    let gcc_except_table_start = gcc_except_table.mem_offset;
    let gcc_except_table_end = gcc_except_table_start + gcc_except_table.mem_size;
    let data = object.object.raw_section_data(eh_frame_section)?;
    let filter_live_unwind = macho_writer_unwind_atom_gc_enabled(object, layout);
    let live_fdes = object
        .format_specific
        .live_eh_frame_fdes
        .get(eh_frame_section_index.0);
    let relax_deltas = object.section_relax_deltas.get(eh_frame_section_index.0);
    let live_ranges = if filter_live_unwind {
        live_eh_frame_ranges(data, live_fdes)?
    } else {
        Vec::new()
    };

    let mut relocation_values = HashMap::new();
    let mut relocation_got_values = HashMap::new();
    let mut relocation_sizes = HashMap::new();
    let mut paired_addend = 0;
    for rel in object.relocations(eh_frame_section_index)?.relocations {
        let rel = rel.info(LE);
        if rel.r_type == macho::ARM64_RELOC_ADDEND {
            paired_addend = macho_addend(rel);
            continue;
        }
        if rel.r_type == macho::ARM64_RELOC_SUBTRACTOR {
            continue;
        }
        if filter_live_unwind
            && !live_ranges
                .iter()
                .any(|range| range.contains(&(rel.r_address as usize)))
        {
            paired_addend = 0;
            continue;
        }

        let (resolution, _) = get_resolution(rel, object, layout)?;
        if let Some(got_address) = resolution.format_specific.got_address {
            relocation_got_values.insert(rel.r_address as usize, got_address.get());
        }
        relocation_sizes.insert(rel.r_address as usize, 1usize << rel.r_length);
        relocation_values.insert(
            rel.r_address as usize,
            resolution.raw_value.wrapping_add(paired_addend as u64),
        );
        paired_addend = 0;
    }

    let mut fde_infos = HashMap::new();
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= data.len() {
        let length = read_u32(data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );
        ensure!(
            offset + size_of::<u32>() + length <= data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let cie_pointer = read_u32(data, offset + size_of::<u32>())?;
        if cie_pointer != 0 {
            if filter_live_unwind && !live_fdes.is_some_and(|fdes| fdes.contains(&(offset as u64)))
            {
                offset += size_of::<u32>() + length;
                continue;
            }
            let pc_begin_offset = offset + 2 * size_of::<u32>();
            if let Some(function_address) = relocation_values.get(&pc_begin_offset) {
                let pc_begin_size = relocation_sizes
                    .get(&pc_begin_offset)
                    .copied()
                    .unwrap_or(size_of::<u64>());
                let range_offset = pc_begin_offset + pc_begin_size;
                let function_length = match pc_begin_size {
                    4 => u64::from(read_u32(data, range_offset)?),
                    8 => read_u64(data, range_offset)?,
                    _ => bail!(
                        "Unsupported Mach-O __eh_frame PC-begin encoding size {pc_begin_size}"
                    ),
                };
                let function_length = function_length.try_into().with_context(|| {
                    format!(
                        "Mach-O __eh_frame FDE at offset {offset:#x} has range {function_length:#x}, which exceeds u32::MAX"
                    )
                })?;
                let cie_pointer_offset = offset + size_of::<u32>();
                let cie_start =
                    cie_pointer_offset
                        .checked_sub(cie_pointer as usize)
                        .with_context(|| {
                            format!(
                                "Mach-O __eh_frame FDE at offset {offset:#x} references invalid CIE pointer {cie_pointer:#x}"
                            )
                        })?;
                let cie_length = read_u32(data, cie_start)? as usize;
                ensure!(
                    cie_start + size_of::<u32>() + cie_length <= data.len(),
                    "Mach-O __eh_frame CIE at offset {cie_start:#x} extends past the section"
                );
                let cie_end = cie_start + size_of::<u32>() + cie_length;
                let mut personality_offset = None;
                for (relocation_offset, got_address) in &relocation_got_values {
                    if *relocation_offset >= cie_start && *relocation_offset < cie_end {
                        personality_offset = Some(macho_image_offset(*got_address)?);
                        break;
                    }
                }

                let entry_end = offset + size_of::<u32>() + length;
                let mut lsda_offset = None;
                if gcc_except_table_start != gcc_except_table_end {
                    for (relocation_offset, target_address) in &relocation_values {
                        if *relocation_offset <= pc_begin_offset || *relocation_offset >= entry_end
                        {
                            continue;
                        }
                        if *target_address >= gcc_except_table_start
                            && *target_address < gcc_except_table_end
                        {
                            lsda_offset = Some(macho_image_offset(*target_address)?);
                            break;
                        }
                    }
                }

                fde_infos.insert(
                    *function_address,
                    FdeInfo {
                        output_offset: eh_frame_output_offset
                            + opt_input_to_output(relax_deltas, offset as u64),
                        length: function_length,
                        personality_offset,
                        lsda_offset,
                    },
                );
            }
        }

        offset += size_of::<u32>() + length;
    }

    Ok(Some(fde_infos))
}

fn rewrite_compacted_macho_eh_frame_cie_pointers(
    input_data: &[u8],
    deltas: &SectionRelaxDeltas,
    out: &mut [u8],
) -> Result {
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= input_data.len() {
        let length = read_u32(input_data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );
        let entry_end = offset
            .checked_add(size_of::<u32>())
            .and_then(|entry| entry.checked_add(length))
            .context("Mach-O __eh_frame entry length overflow")?;
        ensure!(
            entry_end <= input_data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let cie_pointer = read_u32(input_data, offset + size_of::<u32>())?;
        if cie_pointer != 0 && !deltas.deletes_input_offset(offset as u64) {
            let cie_pointer_offset = offset + size_of::<u32>();
            let cie_start = cie_pointer_offset
                .checked_sub(cie_pointer as usize)
                .with_context(|| {
                    format!(
                        "Mach-O __eh_frame FDE at offset {offset:#x} references invalid CIE pointer {cie_pointer:#x}"
                    )
                })?;
            ensure!(
                !deltas.deletes_input_offset(cie_start as u64),
                "Compacted Mach-O __eh_frame kept FDE at offset {offset:#x} but deleted its CIE at offset {cie_start:#x}"
            );

            let output_fde_start = usize::try_from(deltas.input_to_output_offset(offset as u64))
                .context("Compacted Mach-O __eh_frame FDE output offset exceeds usize")?;
            let output_cie_start = usize::try_from(deltas.input_to_output_offset(cie_start as u64))
                .context("Compacted Mach-O __eh_frame CIE output offset exceeds usize")?;
            let output_cie_pointer_offset = output_fde_start + size_of::<u32>();
            let output_cie_pointer = output_cie_pointer_offset
                .checked_sub(output_cie_start)
                .context("Compacted Mach-O __eh_frame CIE pointer underflow")?;
            let output_cie_pointer = u32::try_from(output_cie_pointer)
                .context("Compacted Mach-O __eh_frame CIE pointer exceeds u32")?;
            let output_range =
                output_cie_pointer_offset..output_cie_pointer_offset + size_of::<u32>();
            let output = out
                .get_mut(output_range)
                .ok_or_else(|| error!("Write past end of compacted Mach-O __eh_frame buffer"))?;
            output.copy_from_slice(&output_cie_pointer.to_le_bytes());
        }

        offset = entry_end;
    }

    Ok(())
}

fn read_compact_unwind_entries<'data>(
    object: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout<'data>,
    section_index: object::SectionIndex,
) -> Result<Vec<CompactUnwindEntry>> {
    let section = object.object.section(section_index)?;
    let data = object.object.raw_section_data(section)?;
    ensure!(
        data.len() % MACHO_COMPACT_UNWIND_ENTRY_SIZE == 0,
        "__compact_unwind size must be a multiple of {MACHO_COMPACT_UNWIND_ENTRY_SIZE}"
    );

    let mut entries = Vec::with_capacity(data.len() / MACHO_COMPACT_UNWIND_ENTRY_SIZE);
    for offset in (0..data.len()).step_by(MACHO_COMPACT_UNWIND_ENTRY_SIZE) {
        entries.push(CompactUnwindEntry {
            function_address: read_u64(data, offset)?,
            length: read_u32(data, offset + 8)?,
            encoding: read_u32(data, offset + 12)?,
            personality: read_u64(data, offset + 16)?,
            lsda: read_u64(data, offset + 24)?,
        });
    }

    let mut paired_addend = 0;
    let filter_live_unwind = macho_writer_unwind_atom_gc_enabled(object, layout);
    let live_entries = object
        .format_specific
        .live_compact_unwind_entries
        .get(section_index.0);
    for rel in object.relocations(section_index)?.relocations {
        let rel = rel.info(LE);
        if rel.r_type == macho::ARM64_RELOC_ADDEND {
            paired_addend = macho_addend(rel);
            continue;
        }
        ensure!(
            rel.r_type != macho::ARM64_RELOC_SUBTRACTOR,
            "Mach-O __compact_unwind does not support subtractor relocations"
        );

        let offset = rel.r_address as usize;
        let entry_index = offset / MACHO_COMPACT_UNWIND_ENTRY_SIZE;
        let field_offset = offset % MACHO_COMPACT_UNWIND_ENTRY_SIZE;
        let entry_start = entry_index * MACHO_COMPACT_UNWIND_ENTRY_SIZE;
        if filter_live_unwind
            && !live_entries.is_some_and(|entries| entries.contains(&(entry_start as u64)))
        {
            paired_addend = 0;
            continue;
        }
        let entry = entries.get_mut(entry_index).with_context(|| {
            format!("Mach-O __compact_unwind relocation at invalid offset {offset:#x}")
        })?;
        let (resolution, _) = get_resolution(rel, object, layout)?;
        let field_addend = match field_offset {
            0 | 16 | 24 => read_u64(data, offset)?,
            _ => 0,
        };
        let value = resolution
            .raw_value
            .wrapping_add(field_addend)
            .wrapping_add(paired_addend as u64);

        match field_offset {
            0 => entry.function_address = value,
            16 => {
                entry.personality = resolution
                    .format_specific
                    .got_address
                    .map_or(value, |address| address.get().wrapping_add(field_addend));
            }
            24 => entry.lsda = value,
            _ => bail!(
                "Unsupported Mach-O __compact_unwind relocation field offset {field_offset:#x}"
            ),
        }
        paired_addend = 0;
    }

    if filter_live_unwind {
        Ok(entries
            .into_iter()
            .enumerate()
            .filter_map(|(entry_index, entry)| {
                let entry_start = entry_index * MACHO_COMPACT_UNWIND_ENTRY_SIZE;
                live_entries
                    .is_some_and(|entries| entries.contains(&(entry_start as u64)))
                    .then_some(entry)
            })
            .collect())
    } else {
        Ok(entries)
    }
}

fn macho_writer_unwind_atom_gc_enabled(
    object: &ObjectLayout<'_, MachO>,
    layout: &MachOLayout<'_>,
) -> bool {
    layout.symbol_db.args.dead_strip && object.object.flags & macho::MH_SUBSECTIONS_VIA_SYMBOLS != 0
}

fn macho_writer_live_unwind_relocation_ranges(
    object: &ObjectLayout<'_, MachO>,
    layout: &MachOLayout<'_>,
    section_index: object::SectionIndex,
) -> Result<Option<Vec<std::ops::Range<usize>>>> {
    if !macho_writer_unwind_atom_gc_enabled(object, layout) {
        return Ok(None);
    }
    let section = object.object.section(section_index)?;
    match object.object.section_name(section)? {
        b"__eh_frame" => {
            let data = object.object.raw_section_data(section)?;
            let live_fdes = object
                .format_specific
                .live_eh_frame_fdes
                .get(section_index.0);
            Ok(Some(live_eh_frame_ranges(data, live_fdes)?))
        }
        b"__compact_unwind" => {
            let ranges = object
                .format_specific
                .live_compact_unwind_entries
                .get(section_index.0)
                .into_iter()
                .flatten()
                .map(|entry_start| {
                    let entry_start = *entry_start as usize;
                    entry_start..entry_start + MACHO_COMPACT_UNWIND_ENTRY_SIZE
                })
                .collect();
            Ok(Some(ranges))
        }
        _ => Ok(None),
    }
}

fn macho_writer_live_subsection_relocation_ranges(
    object: &ObjectLayout<'_, MachO>,
    layout: &MachOLayout<'_>,
    section_index: object::SectionIndex,
) -> Result<Option<Vec<std::ops::Range<usize>>>> {
    if !layout.symbol_db.args.dead_strip
        || object.object.flags & macho::MH_SUBSECTIONS_VIA_SYMBOLS == 0
    {
        return Ok(None);
    }

    let section = object.object.section(section_index)?;
    let section_name = object.object.section_name(section)?;
    if section.should_retain() || section_name != b"__const" {
        return Ok(None);
    }

    let section_size = object.object.section_size(section)?;
    let mut ranges = Vec::new();
    for range in object
        .format_specific
        .live_subsection_ranges(section_index, section_size)
    {
        let start = usize::try_from(range.start)
            .context("Mach-O __const subsection start exceeds usize")?;
        let end =
            usize::try_from(range.end).context("Mach-O __const subsection end exceeds usize")?;
        ranges.push(start..end);
    }

    Ok(Some(ranges))
}

fn live_eh_frame_ranges(
    data: &[u8],
    live_fdes: Option<&std::collections::BTreeSet<u64>>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let live_cies = macho_live_eh_frame_cies(data, live_fdes)?;
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= data.len() {
        let length = read_u32(data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );
        let entry_end = offset
            .checked_add(size_of::<u32>())
            .and_then(|entry| entry.checked_add(length))
            .context("Mach-O __eh_frame entry length overflow")?;
        ensure!(
            entry_end <= data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let cie_pointer = read_u32(data, offset + size_of::<u32>())?;
        if cie_pointer == 0 {
            if live_cies.contains(&(offset as u64)) {
                ranges.push(offset..entry_end);
            }
        } else if live_fdes.is_some_and(|fdes| fdes.contains(&(offset as u64))) {
            ranges.push(offset..entry_end);
        }

        offset = entry_end;
    }
    Ok(ranges)
}

fn macho_image_offset(address: u64) -> Result<u32> {
    address
        .checked_sub(MACHO_START_MEM_ADDRESS)
        .and_then(|offset| u32::try_from(offset).ok())
        .with_context(|| format!("Mach-O address {address:#x} is outside the 32-bit image range"))
}

struct WrittenSection<'out> {
    bytes: &'out mut [u8],
    reused: bool,
}

fn write_section_raw<'out, 'data>(
    object: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
    sec: &Section,
    section_index: object::SectionIndex,
    buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
    incremental: &PreparedState,
    record_for_reuse: bool,
    allow_reuse: bool,
    group_file_offsets: &OutputSectionPartMap<usize>,
    group_file_sizes: &OutputSectionPartMap<usize>,
) -> Result<WrittenSection<'out>> {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if layout
        .output_sections
        .has_data_in_file(part_id.output_section_id())
    {
        let section_buffer = buffers.get_mut(part_id);
        let allocation_size = sec.capacity(part_id, &layout.output_sections) as usize;
        let consumed_in_group = group_file_sizes
            .get(part_id)
            .checked_sub(section_buffer.len())
            .context("Incremental section buffer is larger than its group allocation")?;
        let output_offset = group_file_offsets
            .get(part_id)
            .checked_add(consumed_in_group)
            .context("Incremental section output offset overflow")?;
        if section_buffer.len() < allocation_size {
            bail!(
                "Insufficient space allocated to section `{}`. Tried to take {} bytes, but only {} remain",
                object.object.section_display_name(section_index),
                allocation_size,
                section_buffer.len()
            );
        }
        let out = section_buffer.split_off_mut(..allocation_size).unwrap();
        if incremental.try_reuse_section(
            object.input,
            section_index,
            output_offset as u64,
            allocation_size as u64,
            record_for_reuse,
            allow_reuse,
        ) {
            return Ok(WrittenSection {
                bytes: &mut [],
                reused: true,
            });
        }
        let object_section = object.object.section(section_index)?;
        let relax_deltas = object.section_relax_deltas.get(section_index.0);

        match relax_deltas {
            None => {
                let section_size = object.object.section_size(object_section)?;
                let (out, padding) = out.split_at_mut(section_size as usize);
                object.object.copy_section_data(object_section, out)?;
                padding.fill(0);
                Ok(WrittenSection {
                    bytes: out,
                    reused: false,
                })
            }
            Some(deltas) => {
                let input_data = object.object.raw_section_data(object_section)?;
                let effective_size = sec.size as usize;

                let mut input_pos = 0usize;
                let mut output_pos = 0usize;

                for delta in deltas.deltas() {
                    let skip_start = delta.input_offset as usize;
                    let copy_len = skip_start - input_pos;
                    if copy_len > 0 {
                        out[output_pos..output_pos + copy_len]
                            .copy_from_slice(&input_data[input_pos..skip_start]);
                        output_pos += copy_len;
                    }
                    input_pos = skip_start + delta.bytes_deleted as usize;
                }

                let remaining = input_data.len() - input_pos;
                if remaining > 0 {
                    out[output_pos..output_pos + remaining]
                        .copy_from_slice(&input_data[input_pos..]);
                    output_pos += remaining;
                }
                out[output_pos..].fill(0);
                if object.object.section_name(object_section)? == b"__eh_frame" {
                    rewrite_compacted_macho_eh_frame_cie_pointers(input_data, deltas, out)?;
                }

                Ok(WrittenSection {
                    bytes: &mut out[..effective_size],
                    reused: false,
                })
            }
        }
    } else {
        Ok(WrittenSection {
            bytes: &mut [],
            reused: false,
        })
    }
}

fn get_resolution<'data>(
    rel: RelocationInfo,
    object_layout: &ObjectLayout<'data, MachO>,
    layout: &MachOLayout,
) -> Result<(Resolution<MachO>, Option<SymbolId>)> {
    if !rel.r_extern {
        ensure!(
            rel.r_symbolnum > 0,
            "Mach-O section relocation must reference a one-based section ordinal"
        );
        let section_index = object::SectionIndex(rel.r_symbolnum as usize - 1);
        let section_address = object_layout
            .section_resolutions
            .get(section_index.0)
            .with_context(|| {
                format!(
                    "Mach-O relocation references missing section ordinal {}",
                    rel.r_symbolnum
                )
            })?
            .address()
            .with_context(|| {
                format!(
                    "Mach-O relocation references unloaded section ordinal {}",
                    rel.r_symbolnum
                )
            })?;
        return Ok((
            Resolution {
                raw_value: section_address,
                dynamic_symbol_index: None,
                flags: ValueFlags::empty(),
                format_specific: Default::default(),
            },
            None,
        ));
    }

    let symbol_index = SymbolIndex(rel.r_symbolnum as usize);
    let local_symbol_id = object_layout.symbol_id_range.input_to_id(symbol_index);
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;
    let local_section_resolution = || {
        section_index.and_then(|section_index| {
            let section_address = object_layout.section_resolutions[section_index.0].address()?;
            let input_offset = object_layout
                .object
                .symbol_offset_in_section(sym, section_index)
                .ok()?;
            let output_offset = opt_input_to_output(
                object_layout.section_relax_deltas.get(section_index.0),
                input_offset,
            );
            Some(Resolution {
                raw_value: section_address + output_offset,
                dynamic_symbol_index: None,
                flags: ValueFlags::empty(),
                format_specific: Default::default(),
            })
        })
    };
    let merged_resolution = layout.merged_symbol_resolution(local_symbol_id);
    let resolution = if sym.is_local() {
        merged_resolution.or_else(local_section_resolution)
    } else if sym.is_hidden() {
        match merged_resolution {
            // Hidden/private externs with duplicate archive definitions should use the selected
            // canonical definition, but if the value still looks like a Mach-O object-file n_value
            // then resolve the symbol through this object's loaded section.
            Some(resolution)
                if !resolution.flags.is_address()
                    || resolution.raw_value >= MACHO_START_MEM_ADDRESS =>
            {
                Some(resolution)
            }
            resolution => local_section_resolution().or(resolution),
        }
    } else {
        merged_resolution.or_else(local_section_resolution)
    }
    .with_context(|| {
        format!(
            "Missing resolution for: {}",
            layout.symbol_debug(local_symbol_id)
        )
    })?;
    Ok((resolution, Some(local_symbol_id)))
}

fn write_entry_point_command<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    command: &mut EntryPointCommand,
) -> Result {
    let SegmentSectionsInfo { segment_size, .. } = get_segment_sections(layout, SegmentType::Text)
        .ok_or_else(|| error!("Text segment is mandatory"))?;
    let entry_address = layout.entry_symbol_address()?;
    let entryoff = entry_address
        .checked_sub(segment_size.mem_offset)
        .and_then(|offset| offset.checked_add(segment_size.file_offset as u64))
        .ok_or_else(|| {
            error!("Entry point address {entry_address:#x} is outside the __TEXT segment")
        })?;

    command.cmd.set(LE, LC_MAIN);
    command
        .cmdsize
        .set(LE, size_of::<EntryPointCommand>() as u32);
    command.entryoff.set(LE, entryoff);
    command.stacksize.set(LE, 0);
    Ok(())
}

fn write_dylinker_command<A: Arch<Platform = MachO>>(
    command: &mut DylinkerCommand,
    path_buffer: &mut [u8],
) {
    command.cmd.set(LE, LC_LOAD_DYLINKER);
    command.cmdsize.set(
        LE,
        ((size_of::<DylinkerCommand>() + DYLINKER_PATH.len() + 1)
            .next_multiple_of(MACHO_COMMAND_ALIGNMENT)) as u32,
    );
    command
        .name
        .offset
        .set(LE, size_of::<DylinkerCommand>() as u32);

    path_buffer[0..DYLINKER_PATH.len()].copy_from_slice(DYLINKER_PATH);
    path_buffer[DYLINKER_PATH.len()..].zero();
}

fn write_build_version_command(layout: &MachOLayout, command: &mut BuildVersionCommand) {
    let platform_version = layout.args().platform_version;
    command.cmd.set(LE, LC_BUILD_VERSION);
    command
        .cmdsize
        .set(LE, size_of::<BuildVersionCommand>() as u32);
    command.platform.set(LE, platform_version.platform);
    command.minos.set(LE, platform_version.minimum_os);
    command.sdk.set(LE, platform_version.sdk);
    command.ntools.set(LE, 0);
}

fn write_uuid_command(command: &mut UuidCommand) {
    command.cmd.set(LE, LC_UUID);
    command.cmdsize.set(LE, size_of::<UuidCommand>() as u32);
    command.uuid = MACHO_UUID;
}

fn write_load_dylib_commands(layout: &MachOLayout, buffer: &mut [u8]) -> Result {
    let mut offset = 0;
    for dylib in load_dylib_commands(layout.args()) {
        let command_size = load_dylib_command_size(dylib.path);
        ensure!(
            offset + command_size <= buffer.len(),
            "Invalid LC_LOAD_DYLIB allocation. Need at least {} bytes, got {}",
            offset + command_size,
            buffer.len()
        );
        let command_buffer = &mut buffer[offset..offset + command_size];
        let (command, path_buffer): (&mut DylibCommand, &mut [u8]) = from_bytes_mut(command_buffer)
            .map_err(|_| error!("Invalid LC_LOAD_DYLIB command allocation"))?;
        write_load_dylib_command(command, path_buffer, dylib.path, dylib.kind);
        offset += command_size;
    }
    ensure!(
        offset == buffer.len(),
        "Unused LC_LOAD_DYLIB allocation. Used {} bytes, got {}",
        offset,
        buffer.len()
    );
    Ok(())
}

fn write_load_dylib_command(
    command: &mut DylibCommand,
    path_buffer: &mut [u8],
    path: &[u8],
    kind: DylibLoadKind,
) {
    let command_id = match kind {
        DylibLoadKind::Regular => LC_LOAD_DYLIB,
        DylibLoadKind::Weak => LC_LOAD_WEAK_DYLIB,
    };
    command.cmd.set(LE, command_id);
    command
        .cmdsize
        .set(LE, load_dylib_command_size(path) as u32);
    command
        .dylib
        .name
        .offset
        .set(LE, size_of::<DylibCommand>() as u32);
    command.dylib.timestamp.set(LE, 2);
    command.dylib.current_version.set(LE, 0);
    command.dylib.compatibility_version.set(LE, 0x1_00_00);

    path_buffer[0..path.len()].copy_from_slice(path);
    path_buffer[path.len()..].zero();
}

fn write_id_dylib_command(
    layout: &MachOLayout,
    command: &mut DylibCommand,
    path_buffer: &mut [u8],
) {
    let path = id_dylib_path(layout.args());
    command.cmd.set(LE, LC_ID_DYLIB);
    command
        .cmdsize
        .set(LE, id_dylib_command_size(layout.args()) as u32);
    command
        .dylib
        .name
        .offset
        .set(LE, size_of::<DylibCommand>() as u32);
    command.dylib.timestamp.set(LE, 1);
    command.dylib.current_version.set(LE, 0);
    command.dylib.compatibility_version.set(LE, 0);

    path_buffer[0..path.len()].copy_from_slice(path);
    path_buffer[path.len()..].zero();
}

fn write_dyld_chained_fixups_command<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    command: &mut DyldChainedFixupsCommand,
) {
    let chained_fixup_table = layout
        .section_layouts
        .get(output_section_id::CHAINED_FIXUP_TABLE);

    command.cmd.set(LE, LC_DYLD_CHAINED_FIXUPS);
    command
        .cmdsize
        .set(LE, size_of::<DyldChainedFixupsCommand>() as u32);
    command
        .dataoff
        .set(LE, chained_fixup_table.file_offset as u32);
    command
        .datasize
        .set(LE, chained_fixup_table.file_size as u32);
}

fn write_symtab_command<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    command: &mut SymtabCommand,
) {
    let symtab = layout.section_layouts.get(output_section_id::SYMTAB_GLOBAL);
    let strtab = layout.section_layouts.get(output_section_id::STRTAB);

    command.cmd.set(LE, LC_SYMTAB);
    command.cmdsize.set(LE, size_of::<SymtabCommand>() as u32);
    command.symoff.set(LE, symtab.file_offset as u32);
    command
        .nsyms
        .set(LE, (symtab.file_size / size_of::<SymtabEntry>()) as u32);
    command.stroff.set(LE, strtab.file_offset as u32);
    command.strsize.set(LE, strtab.file_size as u32);
}

fn write_code_signature_command<A: Arch<Platform = MachO>>(
    layout: &MachOLayout,
    command: &mut CodeSignatureCommand,
) {
    let code_signature = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);

    command.cmd.set(LE, LC_CODE_SIGNATURE);
    command
        .cmdsize
        .set(LE, size_of::<CodeSignatureCommand>() as u32);
    command.dataoff.set(LE, code_signature.file_offset as u32);
    command.datasize.set(LE, code_signature.file_size as u32);
}

fn write_chained_fixup_table(
    out: &mut [u8],
    layout: &MachOLayout,
    chained_rebases: &ChainedRebases,
) -> Result {
    out.fill(0);
    let starts_offset = 32usize;
    let starts_in_image_size = size_of::<u32>() * (DEFAULT_SEGMENT_COUNT + 1);
    let segment_info_offset = starts_in_image_size.next_multiple_of(8);
    let mut imports_offset = starts_offset + starts_in_image_size;
    let imports_size = chained_rebases.imports.len() * size_of::<u32>();

    write_u32(out, 0, 0)?;
    write_u32(out, 4, starts_offset as u32)?;
    write_u32(out, 16, chained_rebases.imports.len() as u32)?;
    write_u32(out, 20, 1)?;
    write_u32(out, 24, 0)?;

    write_u32(out, starts_offset, DEFAULT_SEGMENT_COUNT as u32)?;
    for segment_index in 0..DEFAULT_SEGMENT_COUNT {
        write_u32(
            out,
            starts_offset + size_of::<u32>() * (segment_index + 1),
            0,
        )?;
    }

    if !chained_rebases.slots.is_empty() {
        let data_segment = get_segment_sections(layout, SegmentType::DataSections)
            .ok_or_else(|| error!("Chained rebases require a __DATA segment"))?;
        let data_start = data_segment.segment_size.mem_offset;
        let page_count = data_segment
            .segment_size
            .mem_size
            .div_ceil(MACHO_PAGE_SIZE)
            .max(1);
        let segment_info_size = (size_of::<u32>()
            + size_of::<u16>()
            + size_of::<u16>()
            + size_of::<u64>()
            + size_of::<u32>()
            + size_of::<u16>()
            + size_of::<u16>() * page_count as usize)
            .next_multiple_of(8);
        let segment_info_start = starts_offset + segment_info_offset;
        imports_offset = segment_info_start + segment_info_size;

        ensure!(
            page_count <= u16::MAX.into(),
            "Mach-O chained fixup page count {page_count} exceeds u16::MAX"
        );
        ensure!(
            imports_offset <= out.len(),
            "CHAINED_FIXUP_TABLE allocation too small. Need at least {} bytes, got {}",
            imports_offset,
            out.len()
        );

        write_u32(
            out,
            starts_offset + size_of::<u32>() * (DATA_SEGMENT_CHAIN_INDEX + 1),
            segment_info_offset as u32,
        )?;
        write_u32(out, segment_info_start, segment_info_size as u32)?;
        write_u16(out, segment_info_start + 4, MACHO_PAGE_SIZE as u16)?;
        write_u16(out, segment_info_start + 6, DYLD_CHAINED_PTR_64_OFFSET)?;
        write_u64(
            out,
            segment_info_start + 8,
            data_start
                .checked_sub(MACHO_START_MEM_ADDRESS)
                .context("Invalid Mach-O __DATA segment start")?,
        )?;
        write_u32(out, segment_info_start + 16, 0)?;
        write_u16(out, segment_info_start + 20, page_count as u16)?;

        let page_starts_offset = segment_info_start + 22;
        for page_index in 0..page_count as usize {
            write_u16(
                out,
                page_starts_offset + page_index * size_of::<u16>(),
                DYLD_CHAINED_PTR_START_NONE,
            )?;
        }
        for slot in &chained_rebases.slots {
            ensure!(
                *slot >= data_start,
                "Mach-O chained fixup slot {slot:#x} precedes __DATA"
            );
            let offset_in_segment = slot - data_start;
            ensure!(
                offset_in_segment < data_segment.segment_size.mem_size,
                "Mach-O chained fixup slot {slot:#x} is outside __DATA"
            );
            let page_index = (offset_in_segment / MACHO_PAGE_SIZE) as usize;
            let offset_in_page = (offset_in_segment % MACHO_PAGE_SIZE) as u16;
            let page_start_location = page_starts_offset + page_index * size_of::<u16>();
            let current = read_u16(out, page_start_location)?;
            if current == DYLD_CHAINED_PTR_START_NONE || offset_in_page < current {
                write_u16(out, page_start_location, offset_in_page)?;
            }
        }
    }

    let symbols_offset = imports_offset + imports_size;
    ensure!(
        symbols_offset <= out.len(),
        "CHAINED_FIXUP_TABLE allocation too small. Need at least {} bytes, got {}",
        symbols_offset,
        out.len()
    );

    write_u32(out, 8, imports_offset as u32)?;
    write_u32(out, 12, symbols_offset as u32)?;

    let mut string_offset = 1usize;
    ensure!(
        symbols_offset + string_offset <= out.len(),
        "CHAINED_FIXUP_TABLE allocation too small for symbol strings"
    );
    for (import_index, import) in chained_rebases.imports.iter().enumerate() {
        ensure!(
            string_offset < (1 << 23),
            "Mach-O chained import symbol strings exceed 23-bit offsets"
        );
        write_u32(
            out,
            imports_offset + import_index * size_of::<u32>(),
            u32::from(import.lib_ordinal) | ((string_offset as u32) << 9),
        )?;
        let string_start = symbols_offset + string_offset;
        let string_end = string_start + import.name.len() + 1;
        ensure!(
            string_end <= out.len(),
            "CHAINED_FIXUP_TABLE allocation too small for import `{}`",
            String::from_utf8_lossy(&import.name)
        );
        out[string_start..string_start + import.name.len()].copy_from_slice(&import.name);
        out[string_start + import.name.len()] = 0;
        string_offset += import.name.len() + 1;
    }

    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let bytes = bytes
        .get(offset..offset + size_of::<u16>())
        .ok_or_else(|| error!("Read past end of Mach-O chained fixup table"))?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| error!("Read past end of Mach-O buffer"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let bytes = bytes
        .get(offset..offset + size_of::<u64>())
        .ok_or_else(|| error!("Read past end of Mach-O buffer"))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result {
    let out = bytes
        .get_mut(offset..offset + size_of::<u16>())
        .ok_or_else(|| error!("Write past end of Mach-O chained fixup table"))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result {
    let out = bytes
        .get_mut(offset..offset + size_of::<u32>())
        .ok_or_else(|| error!("Write past end of Mach-O chained fixup table"))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result {
    let out = bytes
        .get_mut(offset..offset + size_of::<u64>())
        .ok_or_else(|| error!("Write past end of Mach-O chained fixup table"))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_code_signature(layout: &MachOLayout, sized_output: &mut SizedOutput) -> Result {
    let code_signature_section = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);
    let calculated_hashes: Vec<_> = sized_output.out[..code_signature_section.file_offset]
        .par_chunks(CS_BLOCK_SIZE)
        .map(Sha256::digest)
        .collect();
    let calculated_hashes = calculated_hashes.into_iter().flatten().collect_vec();

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let code_signature = section_buffers.get_mut(output_section_id::CODE_SIGNATURE);

    let (super_blob, rest): (&mut CodeSignatureSuperBlob, &mut [u8]) =
        CodeSignatureSuperBlob::mut_from_prefix(code_signature)
            .map_err(|_| error!("Invalid CODE_SIGNATURE allocation"))?;
    let (blob_indices, rest) = <[CodeSignatureBlobIndex]>::mut_from_prefix_with_elems(rest, 1)
        .map_err(|_| error!("Invalid CODE_SIGNATURE allocation"))?;
    let blob_index = &mut blob_indices[0];
    let (code_directories, rest) =
        <[CodeSignatureCodeDirectory]>::mut_from_prefix_with_elems(rest, 1)
            .map_err(|_| error!("Invalid CODE_SIGNATURE allocation"))?;
    let code_dir = &mut code_directories[0];
    let (identifier, hashes) = rest.split_at_mut(CS_PADDED_FILENAME_SIZE as usize);

    super_blob.magic.set(CSMAGIC_EMBEDDED_SIGNATURE);
    super_blob
        .length
        .set(code_signature_section.file_size as u32);
    super_blob.count.set(1);

    blob_index.type_.set(CSSLOT_CODEDIRECTORY);
    blob_index.offset.set(CS_BLOB_HEADERS_SIZE as u32);
    blob_index.padding.set(0);

    code_dir.magic.set(CSMAGIC_CODEDIRECTORY);
    code_dir
        .length
        .set((code_signature_section.file_size as u64 - CS_BLOB_HEADERS_SIZE) as u32);
    code_dir.version.set(CS_SUPPORTSEXECSEG);
    code_dir.flags.set(CS_ADHOC | CS_LINKER_SIGNED);
    code_dir
        .hash_offset
        .set(size_of::<CodeSignatureCodeDirectory>() as u32 + CS_PADDED_FILENAME_SIZE as u32);
    code_dir
        .ident_offset
        .set(size_of::<CodeSignatureCodeDirectory>() as u32);
    code_dir.n_special_slots.set(0);
    code_dir
        .n_code_slots
        .set(code_signature_section.file_offset.div_ceil(CS_BLOCK_SIZE) as u32);
    code_dir
        .code_limit
        .set(code_signature_section.file_offset as u32);
    code_dir.hash_size = CS_HASH_SIZE;
    code_dir.hash_type = CS_HASHTYPE_SHA256;
    code_dir.platform = 0;
    code_dir.page_size = CS_BLOCK_SIZE_EXP;
    code_dir.spare2.set(0);
    code_dir.scatter_offset.set(0);
    code_dir.team_offset.set(0);
    code_dir.spare3.set(0);
    code_dir.code_limit64.set(0);

    let text_segment_size = get_segment_sections(layout, SegmentType::Text)
        .ok_or_else(|| error!("Text segment is mandatory"))?
        .segment_size;
    code_dir
        .exec_seg_base
        .set(text_segment_size.file_offset as u64);
    code_dir
        .exec_seg_limit
        .set(text_segment_size.file_size as u64);
    // TODO: change once shared libraries are supported
    code_dir.exec_seg_flags.set(CS_EXECSEG_MAIN_BINARY);

    identifier[..CS_IDENTIFIER_STRING.len()].copy_from_slice(CS_IDENTIFIER_STRING);
    identifier[CS_IDENTIFIER_STRING.len()..].fill(0);
    hashes.copy_from_slice(&calculated_hashes);

    #[cfg(target_os = "macos")]
    if let crate::file_writer::OutputBuffer::Mmap(output) = &mut sized_output.out {
        // Match lld's workaround for the macOS kernel caching signature-verification
        // data before the final code signature has been written:
        //
        // https://openradar.appspot.com/FB8914231
        unsafe {
            libc::msync(
                output.as_mut_ptr().cast(),
                code_signature_section.file_offset + code_signature_section.file_size,
                libc::MS_INVALIDATE,
            );
        }
    }

    Ok(())
}

struct MachOSymbolTableWriter {
    next_strtab_offset: u32,
}

impl MachOSymbolTableWriter {
    fn write_str(&mut self, name: &[u8], buffers: &mut OutputSectionPartMap<&mut [u8]>) -> u32 {
        let len_with_terminator = name.len() + 1;
        let offset = self.next_strtab_offset;
        let out = buffers
            .get_mut(part_id::STRTAB)
            .split_off_mut(..len_with_terminator)
            .unwrap();
        out[..name.len()].copy_from_slice(name);
        out[name.len()] = 0;
        self.next_strtab_offset += len_with_terminator as u32;
        offset
    }

    #[inline(always)]
    fn define_symbol(
        &mut self,
        buffers: &mut OutputSectionPartMap<&mut [u8]>,
        name: &[u8],
        section: u8,
        symbol_type: u8,
        desc: u16,
        value: u64,
    ) -> Result {
        let entry = self.write_entry(name, buffers)?;
        entry.n_sect = section;
        entry.n_type = symbol_type;
        entry.n_value.set(LE, value);
        entry.n_desc.set(LE, desc);

        Ok(())
    }

    fn write_entry<'out>(
        &mut self,
        name: &[u8],
        buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
    ) -> Result<&'out mut SymtabEntry> {
        let string_offset = self.write_str(name, buffers);
        let entry_bytes = buffers
            .get_mut(part_id::SYMTAB_GLOBAL)
            .split_off_mut(..size_of::<SymtabEntry>())
            .unwrap();
        let entry: &mut SymtabEntry = from_bytes_mut(entry_bytes)
            .map_err(|_| error!("Invalid SYMTAB_GLOBAL entry allocation"))?
            .0;
        entry.n_strx.set(LE, string_offset);
        Ok(entry)
    }
}

fn write_symbols<'data>(
    object: &ObjectLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
) -> Result {
    for ((sym_index, sym), flags) in object
        .object
        .enumerate_symbols()
        .zip(layout.per_symbol_flags.raw_range(object.symbol_id_range))
    {
        let symbol_id = object.symbol_id_range.input_to_id(sym_index);
        let Some(info) = SymbolCopyInfo::new(
            object.object,
            sym_index,
            sym,
            symbol_id,
            &layout.symbol_db,
            flags.get(),
            &object.sections,
            &object.section_relax_deltas,
        ) else {
            continue;
        };

        let mut value = 0;
        let (section, symbol_type, desc) =
            if let Some(section_index) = object.object.symbol_section(sym, sym_index)? {
                let section_id = match &object.sections[section_index.0] {
                    SectionSlot::Loaded(_) => object
                        .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                        .output_section_id(),
                    _ => bail!(
                        "Tried to copy a symbol in a section we didn't load. {}",
                        layout.symbol_debug(symbol_id)
                    ),
                };
                let primary_id = layout.output_sections.primary_output_section(section_id);
                let n_type = (sym.n_type & !object::macho::N_TYPE) | N_SECT;
                let n_sect = macho_section_index(layout, primary_id).with_context(|| {
                    format!(
                        "No Mach-O section index for {} while writing {}",
                        primary_id,
                        layout.symbol_debug(symbol_id)
                    )
                })?;
                let n_desc = sym.n_desc.get(LE);
                (n_sect, n_type, n_desc)
            } else if sym.is_absolute() {
                let n_desc = sym.n_desc.get(LE);
                (0, (sym.n_type & !object::macho::N_TYPE) | N_ABS, n_desc)
            } else if let Some(common) = sym.as_common() {
                let primary_id = layout
                    .output_sections
                    .primary_output_section(common.part_id.output_section_id());
                let n_sect = macho_section_index(layout, primary_id).with_context(|| {
                    format!(
                        "No Mach-O section index for {} while writing {}",
                        primary_id,
                        layout.symbol_debug(symbol_id)
                    )
                })?;
                let n_type = (sym.n_type & !object::macho::N_TYPE) | N_SECT;
                (n_sect, n_type, 0)
            } else {
                bail!("Attempted to output a Mach-O symtab entry with an unexpected section type")
            };

        if let Some(res) = layout.local_symbol_resolution(symbol_id) {
            value = res.value_for_symbol_table();
        }

        symbol_writer.define_symbol(buffers, info.name, section, symbol_type, desc, value)?;
    }

    Ok(())
}

fn write_internal_symbols<'data>(
    prelude: &PreludeLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    symbol_writer: &mut MachOSymbolTableWriter,
) -> Result {
    for (local_index, def_info) in prelude
        .internal_symbols
        .symbol_definitions
        .iter()
        .enumerate()
    {
        let symbol_id = prelude
            .internal_symbols
            .start_symbol_id
            .add_usize(local_index);
        if !layout.symbol_db.is_canonical(symbol_id) || symbol_id.is_undefined() {
            continue;
        }
        let Some(resolution) = layout.local_symbol_resolution(symbol_id) else {
            continue;
        };

        let symbol_name = layout.symbol_db.symbol_name(symbol_id)?;
        let (section, symbol_type) = match def_info.placement {
            crate::parsing::SymbolPlacement::Undefined
            | crate::parsing::SymbolPlacement::ForceUndefined => {
                (0, macho_internal_symbol_type(def_info, N_UNDF))
            }
            crate::parsing::SymbolPlacement::DefsymAbsolute(_)
            | crate::parsing::SymbolPlacement::Redirect(_) => {
                (0, macho_internal_symbol_type(def_info, N_ABS))
            }
            crate::parsing::SymbolPlacement::SectionStart(section_id)
            | crate::parsing::SymbolPlacement::SectionGroupStart(section_id)
            | crate::parsing::SymbolPlacement::SectionEnd(section_id)
            | crate::parsing::SymbolPlacement::SectionGroupEnd(section_id) => (
                macho_section_index_for_internal_symbol(layout, section_id, symbol_id)?,
                macho_internal_symbol_type(def_info, N_SECT),
            ),
            crate::parsing::SymbolPlacement::LoadBaseAddress
            | crate::parsing::SymbolPlacement::SegmentStart(_, _) => (
                macho_section_index(layout, output_section_id::TEXT).with_context(|| {
                    format!(
                        "No Mach-O section index for __text while writing {}",
                        layout.symbol_debug(symbol_id)
                    )
                })?,
                macho_internal_symbol_type(def_info, N_SECT),
            ),
        };

        symbol_writer.define_symbol(
            buffers,
            symbol_name.bytes(),
            section,
            symbol_type,
            0,
            resolution.value_for_symbol_table(),
        )?;
    }

    Ok(())
}

fn macho_internal_symbol_type(
    def_info: &crate::parsing::InternalSymDefInfo<MachO>,
    base_type: u8,
) -> u8 {
    if def_info.symbol.is_hidden() {
        base_type | N_EXT | N_PEXT
    } else {
        base_type | N_EXT
    }
}

fn macho_section_index_for_internal_symbol(
    layout: &MachOLayout<'_>,
    section_id: output_section_id::OutputSectionId,
    symbol_id: SymbolId,
) -> Result<u8> {
    let symtab_section_id = if section_id == output_section_id::FILE_HEADER {
        output_section_id::TEXT
    } else {
        layout.output_sections.primary_output_section(section_id)
    };
    macho_section_index(layout, symtab_section_id).with_context(|| {
        format!(
            "No Mach-O section index for {} while writing {}",
            layout.output_sections.display_name(symtab_section_id),
            layout.symbol_debug(symbol_id)
        )
    })
}

// TODO: This is inefficient; simplify it once load commands use a table allocator instead of
// being modeled as a section.
fn macho_section_index(
    layout: &MachOLayout<'_>,
    section_id: output_section_id::OutputSectionId,
) -> Result<u8> {
    // The section index is one-based.
    let mut section_idx = 1u8;
    let mut in_section_segment = false;
    for event in &layout.output_order {
        match event {
            output_section_id::OrderEvent::SegmentStart(segment_id) => {
                let segment_type = layout.program_segments.segment_def(segment_id).segment_type;
                // TODO: Right now, the various load commands are mapped as "sections", so we can't
                // just take the mapped index of the output "section".
                in_section_segment = matches!(
                    segment_type,
                    SegmentType::TextSections
                        | SegmentType::DataSections
                        | SegmentType::DataConstSections
                );
            }
            output_section_id::OrderEvent::SegmentEnd(_) => {
                in_section_segment = false;
            }
            output_section_id::OrderEvent::Section(current) if in_section_segment => {
                if !layout.output_sections.will_emit_section(current) {
                    continue;
                }
                if current == section_id {
                    return Ok(section_idx);
                }
                section_idx = section_idx
                    .checked_add(1)
                    .ok_or(error!("Section index out of range (u8)"))?;
            }
            _ => {}
        }
    }

    bail!("cannot find the output section")
}
