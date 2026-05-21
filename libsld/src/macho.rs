// TODO
#![allow(unused_variables)]
#![allow(unused)]

use crate::OutputKind;
use crate::alignment;
use crate::alignment::Alignment;
use crate::alignment::MACHO_PAGE_ALIGNMENT;
use crate::args::macho::MachOArgs;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::copy_section_data;
use crate::layout;
use crate::layout::Layout;
use crate::layout::OutputRecordLayout;
use crate::layout::Resolution;
use crate::layout::SymbolCopyInfo;
use crate::layout_rules::SectionKind;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use crate::macho_writer;
use crate::output_section_id;
use crate::output_section_id::NUM_BUILT_IN_SECTIONS;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrderBuilder;
use crate::output_section_id::SectionName;
use crate::output_section_id::SectionOutputInfo;
use crate::part_id;
use crate::platform;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionHeader as _;
use crate::platform::Symbol as _;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::Visibility;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::ValueFlags;
use gimli::LittleEndian;
use linker_utils::relaxation::SectionRelaxDeltas;
use object::Endian;
use object::Endianness;
use object::SymbolIndex;
use object::macho;
use object::macho::N_ABS;
use object::macho::N_ALT_ENTRY;
use object::macho::N_EXT;
use object::macho::N_NO_DEAD_STRIP;
use object::macho::N_PEXT;
use object::macho::N_SECT;
use object::macho::N_TYPE;
use object::macho::N_WEAK_DEF;
use object::macho::SEG_DATA;
use object::macho::SEG_LINKEDIT;
use object::macho::SEG_PAGEZERO;
use object::macho::SEG_TEXT;
use object::macho::Section64;
use object::read::macho::MachHeader;
use object::read::macho::Nlist;
use object::read::macho::Section;
use object::read::macho::Segment;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::num::NonZeroU64;
#[cfg(target_os = "macos")]
use std::path::Path;
use zerocopy::BigEndian;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::U32;
use zerocopy::U64;

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct MachO;

const LE: Endianness = Endianness::Little;

/// Mach-O uses a zero page for all 32bit addresses and thus we begin the memory
/// offsets right after that (1GiB).
pub(crate) const MACHO_START_MEM_ADDRESS: u64 = 0x1_0000_0000;

/// The command alignment is 8B for 64-bit platforms.
pub(crate) const MACHO_COMMAND_ALIGNMENT: usize = 8;
pub(crate) const MACHO_PAGE_SIZE: u64 = 0x4000;
pub(crate) const MACHO_GOT_ENTRY_SIZE: u64 = 8;
pub(crate) const MACHO_STUB_SIZE: u64 = 12;
pub(crate) const MACHO_COMPACT_UNWIND_ENTRY_SIZE: usize = 32;
pub(crate) const MACHO_UNWIND_SECOND_LEVEL_REGULAR: u32 = 2;
pub(crate) const MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT: usize = 511;

/// A path to the default dynamic linker.
pub(crate) const DYLINKER_PATH: &[u8] = b"/usr/lib/dyld";
pub(crate) const LIBSYSTEM_PATH: &[u8] = b"/usr/lib/libSystem.B.dylib";
pub(crate) const DEFAULT_ID_DYLIB_PATH: &[u8] = b"@rpath/sld-linked.dylib";
// TODO: optionality of __DATA and __CONST_DATA segments not respected
pub(crate) const DEFAULT_SEGMENT_COUNT: usize = 4;
const CHAINED_FIXUP_TABLE_HEADER_SIZE: u64 = 32;

pub(crate) fn macho_unwind_info_allocation_size(entry_count: usize) -> u64 {
    if entry_count == 0 {
        return 0;
    }

    let page_count = entry_count.div_ceil(MACHO_UNWIND_REGULAR_SECOND_LEVEL_ENTRY_COUNT);
    let header_size = 7 * size_of::<u32>();
    let common_encodings_array_size = 3 * size_of::<u32>();
    let max_personality_array_size = 3 * size_of::<u32>();
    let index_size = (page_count + 1) * 3 * size_of::<u32>();
    let max_lsda_index_size = entry_count * 2 * size_of::<u32>();
    let second_level_pages_size =
        page_count * (size_of::<u32>() + 2 * size_of::<u16>()) + entry_count * 2 * size_of::<u32>();

    (header_size
        + common_encodings_array_size
        + max_personality_array_size
        + index_size
        + max_lsda_index_size
        + second_level_pages_size) as u64
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum DylibLoadKind {
    Regular,
    Weak,
}

pub(crate) struct DylibLoadCommand<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) kind: DylibLoadKind,
}

pub(crate) fn load_dylib_commands<'a>(
    args: &'a MachOArgs,
) -> impl Iterator<Item = DylibLoadCommand<'a>> + 'a {
    std::iter::once(DylibLoadCommand {
        path: LIBSYSTEM_PATH,
        kind: DylibLoadKind::Regular,
    })
    .chain(args.extra_dylib_paths.iter().map(|path| DylibLoadCommand {
        path: path.as_slice(),
        kind: if args.weak_dylib_paths.contains(path) {
            DylibLoadKind::Weak
        } else {
            DylibLoadKind::Regular
        },
    }))
}

fn macho_eh_frame_fde_count(data: &[u8]) -> Result<usize> {
    let mut count = 0;
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= data.len() {
        let length = read_macho_u32(data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );

        let entry_end = offset
            .checked_add(size_of::<u32>())
            .and_then(|entry_start| entry_start.checked_add(length))
            .context("Mach-O __eh_frame entry length overflow")?;
        ensure!(
            entry_end <= data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let cie_pointer = read_macho_u32(data, offset + size_of::<u32>())?;
        if cie_pointer != 0 {
            count += 1;
        }

        offset = entry_end;
    }

    Ok(count)
}

fn read_macho_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset + size_of::<u32>())
        .ok_or_else(|| error!("Read past end of Mach-O buffer"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::MachO;
    use super::macho_eh_frame_fde_count;
    use super::macho_live_eh_frame_cies;
    use crate::platform;
    use crate::platform::Symbol as _;
    use object::macho::N_SECT;
    use std::collections::BTreeSet;

    #[test]
    fn macho_eh_frame_fde_count_ignores_cies() {
        let data = [
            4, 0, 0, 0, // CIE length
            0, 0, 0, 0, // CIE pointer
            4, 0, 0, 0, // FDE length
            8, 0, 0, 0, // CIE pointer
            0, 0, 0, 0, // terminator
        ];

        assert_eq!(macho_eh_frame_fde_count(&data).unwrap(), 1);
    }

    #[test]
    fn macho_live_eh_frame_cies_only_keeps_live_fde_dependencies() {
        let data = [
            4, 0, 0, 0, // Referenced CIE length.
            0, 0, 0, 0, // Referenced CIE marker.
            4, 0, 0, 0, // Unreferenced CIE length.
            0, 0, 0, 0, // Unreferenced CIE marker.
            4, 0, 0, 0, // Live FDE length.
            20, 0, 0, 0, // Pointer back to the first CIE.
        ];

        let live_fdes = BTreeSet::from([16]);
        let live_cies = macho_live_eh_frame_cies(&data, Some(&live_fdes)).unwrap();

        assert_eq!(live_cies, BTreeSet::from([0]));
    }

    #[test]
    fn macho_default_strips_local_assembler_labels() {
        let mut sym = <MachO as platform::Platform>::default_symtab_entry();
        sym.n_type = N_SECT;

        assert!(sym.is_default_strippable(b"ltmp1"));
        assert!(sym.is_default_strippable(b".Ldata1"));
        assert!(sym.is_default_strippable(b"_.Ldata1"));
        assert!(sym.is_default_strippable(b"l_.str"));
        assert!(sym.is_default_strippable(b"l_.str.1"));
        assert!(!sym.is_default_strippable(b"_main"));
    }
}

pub(crate) fn load_dylib_paths<'a>(args: &'a MachOArgs) -> impl Iterator<Item = &'a [u8]> + 'a {
    load_dylib_commands(args).map(|command| command.path)
}

pub(crate) fn load_dylib_command_size(path: &[u8]) -> usize {
    (size_of::<DylibCommand>() + path.len() + 1).next_multiple_of(MACHO_COMMAND_ALIGNMENT)
}

pub(crate) fn load_dylib_commands_size(args: &MachOArgs) -> usize {
    load_dylib_commands(args)
        .map(|command| load_dylib_command_size(command.path))
        .sum()
}

pub(crate) fn load_dylib_command_count(args: &MachOArgs) -> usize {
    load_dylib_commands(args).count()
}

pub(crate) fn id_dylib_path(args: &MachOArgs) -> &[u8] {
    args.install_name
        .as_deref()
        .unwrap_or(DEFAULT_ID_DYLIB_PATH)
}

pub(crate) fn id_dylib_command_size(args: &MachOArgs) -> usize {
    load_dylib_command_size(id_dylib_path(args))
}

type SectionHeader = Section64<crate::macho::Endianness>;
type SectionTable<'data> = &'data [Section64<crate::macho::Endianness>];
type SymbolTable<'data> = object::read::macho::SymbolTable<'data, macho::MachHeader64<Endianness>>;
type SymtabEntry = object::macho::Nlist64<Endianness>;
type Relocation = object::macho::Relocation<Endianness>;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct ResolutionExt {
    pub(crate) got_address: Option<NonZeroU64>,
    pub(crate) stub_address: Option<NonZeroU64>,
    pub(crate) is_import: bool,
}

#[derive(Clone)]
struct CompactUnwindEntryLookup {
    entry_start: u64,
    relocation_indices: Vec<usize>,
}

struct CompactUnwindLookup {
    section_index: object::SectionIndex,
    entries_by_symbol: HashMap<usize, Vec<CompactUnwindEntryLookup>>,
}

#[derive(Clone)]
struct EhFrameEntryLookup {
    fde_start: u64,
    fde_end: usize,
    cie_start: u64,
    cie_end: usize,
    fde_relocation_indices: Vec<usize>,
    cie_relocation_indices: Vec<usize>,
}

struct EhFrameLookup {
    section_index: object::SectionIndex,
    entries_by_symbol: HashMap<usize, Vec<EhFrameEntryLookup>>,
}

#[derive(Clone, Copy)]
struct EhFrameEntryBounds {
    start: usize,
    end: usize,
    cie_pointer: u32,
}

#[derive(Default)]
pub(crate) struct ObjectLayoutStateExt {
    subsection_boundaries: Vec<Option<Vec<u64>>>,
    live_subsections: Vec<BTreeSet<u64>>,
    pub(crate) live_compact_unwind_entries: Vec<BTreeSet<u64>>,
    pub(crate) live_eh_frame_fdes: Vec<BTreeSet<u64>>,
    pub(crate) live_eh_frame_cies: Vec<BTreeSet<u64>>,
    compact_unwind_lookup: Option<Option<CompactUnwindLookup>>,
    eh_frame_lookup: Option<Option<EhFrameLookup>>,
}

impl ObjectLayoutStateExt {
    fn ensure_section(&mut self, section_index: object::SectionIndex) {
        let required_len = section_index.0 + 1;
        if self.subsection_boundaries.len() < required_len {
            self.subsection_boundaries
                .resize_with(required_len, || None);
        }
        if self.live_subsections.len() < required_len {
            self.live_subsections
                .resize_with(required_len, BTreeSet::new);
        }
        if self.live_compact_unwind_entries.len() < required_len {
            self.live_compact_unwind_entries
                .resize_with(required_len, BTreeSet::new);
        }
        if self.live_eh_frame_fdes.len() < required_len {
            self.live_eh_frame_fdes
                .resize_with(required_len, BTreeSet::new);
        }
        if self.live_eh_frame_cies.len() < required_len {
            self.live_eh_frame_cies
                .resize_with(required_len, BTreeSet::new);
        }
    }

    pub(crate) fn live_subsection_ranges(
        &self,
        section_index: object::SectionIndex,
        section_size: u64,
    ) -> Vec<std::ops::Range<u64>> {
        let Some(Some(boundaries)) = self.subsection_boundaries.get(section_index.0) else {
            return Vec::new();
        };
        let live_subsections = self.live_subsections.get(section_index.0);
        boundaries
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, start)| {
                let end = boundaries.get(index + 1).copied().unwrap_or(section_size);
                (end > start
                    && live_subsections
                        .is_some_and(|live_subsections| live_subsections.contains(&start)))
                .then_some(start..end)
            })
            .collect()
    }
}

pub(crate) type FileHeader = object::macho::MachHeader64<Endianness>;
pub(crate) type SegmentCommand = object::macho::SegmentCommand64<Endianness>;
pub(crate) type SectionEntry = object::macho::Section64<Endianness>;
pub(crate) type EntryPointCommand = object::macho::EntryPointCommand<Endianness>;
pub(crate) type BuildVersionCommand = object::macho::BuildVersionCommand<Endianness>;
pub(crate) type UuidCommand = object::macho::UuidCommand<Endianness>;
pub(crate) type DylibCommand = object::macho::DylibCommand<Endianness>;
pub(crate) type DylinkerCommand = object::macho::DylinkerCommand<Endianness>;
pub(crate) type CodeSignatureCommand = object::macho::LinkeditDataCommand<Endianness>;
pub(crate) type DyldChainedFixupsCommand = object::macho::LinkeditDataCommand<Endianness>;
pub(crate) type ChainedFixupsHeader = DyldChainedFixupsHeader;
pub(crate) type SymtabCommand = object::macho::SymtabCommand<Endianness>;

// TODO: move the following data types to object crate

// values for dyld_chained_fixups_header.imports_format
#[allow(non_camel_case_types)]
#[repr(u32)]
pub(crate) enum DyldChainedFixupsImporstFormat {
    DYLD_CHAINED_IMPORT = 1,
    DYLD_CHAINED_IMPORT_ADDEND = 2,
    DYLD_CHAINED_IMPORT_ADDEND64 = 3,
}

// header of the LC_DYLD_CHAINED_FIXUPS payload
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
#[repr(C)]
pub(crate) struct DyldChainedFixupsHeader {
    // 0
    pub(crate) fixups_version: U32<zerocopy::LittleEndian>,
    // offset of dyld_chained_starts_in_image in chain_data
    pub(crate) starts_offset: U32<zerocopy::LittleEndian>,
    // offset of imports table in chain_data
    pub(crate) imports_offset: U32<zerocopy::LittleEndian>,
    // offset of symbol strings in chain_data
    pub(crate) symbols_offset: U32<zerocopy::LittleEndian>,
    // number of imported symbol names
    pub(crate) imports_count: U32<zerocopy::LittleEndian>,
    // DYLD_CHAINED_IMPORT*
    pub(crate) imports_format: U32<zerocopy::LittleEndian>,
    // 0 => uncompressed, 1 => zlib compressed
    pub(crate) symbols_format: U32<zerocopy::LittleEndian>,
}

// This struct is embedded in LC_DYLD_CHAINED_FIXUPS payload
// struct dyld_chained_starts_in_image
// {
//     uint32_t    seg_count;
//     uint32_t    seg_info_offset[1];  // each entry is offset into this struct for that segment
//     // followed by pool of dyld_chain_starts_in_segment data
// };

// Data structures mirroring the following URL:
// https://github.com/apple-oss-distributions/xnu/blob/94d3b452840153a99b38a3a9659680b2a006908e/osfmk/kern/cs_blobs.h.

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
#[repr(C)]
pub(crate) struct CodeSignatureSuperBlob {
    // magic number
    pub(crate) magic: U32<BigEndian>,
    // total length of SuperBlob
    pub(crate) length: U32<BigEndian>,
    // number of index entries following
    pub(crate) count: U32<BigEndian>,
    // (count) entries
    // CodeSignatureBlobIndex index[];
    // followed by Blobs in no particular order as indicated by offsets in index
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
#[repr(C)]
pub(crate) struct CodeSignatureBlobIndex {
    // type of entry
    pub(crate) type_: U32<BigEndian>,
    // offset of entry
    pub(crate) offset: U32<BigEndian>,
    // an extra padding so that we have CodeSignatureSuperBlob + CodeSignatureBlobIndex aligned to
    // 8 bytes!
    pub(crate) padding: U32<BigEndian>,
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy)]
#[repr(C)]
pub(crate) struct CodeSignatureCodeDirectory {
    // magic number (CSMAGIC_CODEDIRECTORY)
    pub(crate) magic: U32<BigEndian>,
    // total length of CodeDirectory blob
    pub(crate) length: U32<BigEndian>,
    // compatibility version
    pub(crate) version: U32<BigEndian>,
    // setup and mode flags
    pub(crate) flags: U32<BigEndian>,
    // offset of hash slot element at index zero
    pub(crate) hash_offset: U32<BigEndian>,
    // offset of identifier string
    pub(crate) ident_offset: U32<BigEndian>,
    // number of special hash slots
    pub(crate) n_special_slots: U32<BigEndian>,
    // number of ordinary (code) hash slots
    pub(crate) n_code_slots: U32<BigEndian>,
    // limit to main image signature range
    pub(crate) code_limit: U32<BigEndian>,
    // size of each hash in bytes
    pub(crate) hash_size: u8,
    // type of hash (cdHashType* constants)
    pub(crate) hash_type: u8,
    // platform identifier; zero if not platform binary
    pub(crate) platform: u8,
    // log2(page size in bytes); 0 => infinite
    pub(crate) page_size: u8,
    // unused (must be zero)
    pub(crate) spare2: U32<BigEndian>,

    // Version 0x20100
    //
    // offset of optional scatter vector
    pub(crate) scatter_offset: U32<BigEndian>,

    // Version 0x20200
    //
    // offset of optional team identifier
    pub(crate) team_offset: U32<BigEndian>,

    // Version 0x20300
    //
    // unused (must be zero)
    pub(crate) spare3: U32<BigEndian>,
    // limit to main image signature range, 64 bits
    pub(crate) code_limit64: U64<BigEndian>,

    // Version 0x20400
    //
    // offset of executable segment
    pub(crate) exec_seg_base: U64<BigEndian>,
    // limit of executable segment
    pub(crate) exec_seg_limit: U64<BigEndian>,
    // executable segment flags
    pub(crate) exec_seg_flags: U64<BigEndian>,
    // Version 0x20500 and 0x20600 are unused!
    // followed by dynamic content as located by offset fields above
}

pub(crate) const CS_SECTION_ALIGNMENT_EXP: u8 = 4;
pub(crate) const CS_SECTION_ALIGNMENT: u64 = 2u64.pow(CS_SECTION_ALIGNMENT_EXP as u32);
// TODO: properly implement
pub(crate) const CS_IDENTIFIER_STRING: &[u8] = b"a.out";

pub(crate) const CS_BLOB_HEADERS_SIZE: u64 =
    (size_of::<CodeSignatureSuperBlob>() + size_of::<CodeSignatureBlobIndex>()) as u64;
const _: () = assert!(CS_BLOB_HEADERS_SIZE.is_multiple_of(8));
pub(crate) const CS_HEADERS_SIZE: u64 =
    CS_BLOB_HEADERS_SIZE + size_of::<CodeSignatureCodeDirectory>() as u64;
pub(crate) const CS_PADDED_FILENAME_SIZE: u64 =
    (CS_IDENTIFIER_STRING.len() as u64 + 1).next_multiple_of(CS_SECTION_ALIGNMENT);
pub(crate) const CS_HEADERS_WITH_FILENAME_SIZE: u64 = CS_HEADERS_SIZE + CS_PADDED_FILENAME_SIZE;
pub(crate) const CS_BLOCK_SIZE_EXP: u8 = 12;
pub(crate) const CS_BLOCK_SIZE: usize = 2usize.pow(CS_BLOCK_SIZE_EXP as u32);
// SHA-256 is being used
pub(crate) const CS_HASH_SIZE: u8 = 32;

pub(crate) const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade0cc0;
pub(crate) const CSSLOT_CODEDIRECTORY: u32 = 0;
pub(crate) const CSMAGIC_CODEDIRECTORY: u32 = 0xfade0c02;
pub(crate) const CS_SUPPORTSEXECSEG: u32 = 0x20400;
// Ad hoc signed
pub(crate) const CS_ADHOC: u32 = 0x00000002;
// Automatically signed by the linker
pub(crate) const CS_LINKER_SIGNED: u32 = 0x00020000;
pub(crate) const CS_HASHTYPE_SHA256: u8 = 2;
pub(crate) const CS_EXECSEG_MAIN_BINARY: u64 = 0x1;

#[derive(derive_more::Debug)]
pub(crate) struct File<'data> {
    #[debug(skip)]
    pub(crate) data: &'data [u8],
    #[debug(skip)]
    pub(crate) sections: SectionTable<'data>,
    #[debug(skip)]
    pub(crate) symbols: SymbolTable<'data>,
    pub(crate) flags: u32,
}

impl<'data> platform::ObjectFile<'data> for File<'data> {
    type Platform = MachO;

    fn parse_bytes(input: &'data [u8], is_dynamic: bool) -> crate::error::Result<Self> {
        let header = macho::MachHeader64::<object::Endianness>::parse(input, 0)?;
        let mut commands = header.load_commands(LE, input, 0)?;

        let mut symbols = None;
        let mut sections = None;

        while let Some(command) = commands.next()? {
            if let Some(symtab_command) = command.symtab()? {
                ensure!(symbols.is_none(), "At most one symtab command expected");
                symbols = Some(symtab_command.symbols::<macho::MachHeader64<_>, _>(LE, input)?);
            } else if let Some((segment_command, segment_data)) = command.segment_64()? {
                ensure!(sections.is_none(), "At most one segment command expected");
                let section_list = segment_command.sections(LE, segment_data)?;
                sections = Some(section_list);
            }
        }

        Ok(File {
            data: input,
            symbols: symbols.ok_or("Missing symbol table")?,
            sections: sections.ok_or("Missing segment command")?,
            flags: header.flags(LE),
        })
    }

    fn parse(
        input: &crate::input_data::InputBytes<'data>,
        args: &<Self::Platform as platform::Platform>::Args,
    ) -> crate::error::Result<Self> {
        // TODO
        Self::parse_bytes(input.data, false)
    }

    fn is_dynamic(&self) -> bool {
        // TODO
        false
    }

    fn num_symbols(&self) -> usize {
        self.symbols.len()
    }

    fn symbols_iter(&self) -> impl Iterator<Item = &'data SymtabEntry> {
        self.symbols.iter()
    }

    fn symbol(
        &self,
        index: object::SymbolIndex,
    ) -> crate::error::Result<&'data <Self::Platform as platform::Platform>::SymtabEntry> {
        Ok(self.symbols.symbol(index)?)
    }

    fn section_size(
        &self,
        header: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<u64> {
        Ok(header.size.get(LE))
    }

    fn symbol_name(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
    ) -> crate::error::Result<&'data [u8]> {
        Ok(symbol.name(LE, self.symbols.strings())?)
    }

    fn symbol_offset_in_section(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        section_index: object::SectionIndex,
    ) -> crate::error::Result<u64> {
        let section = self.section(section_index)?;
        // On Mach-O the symbol value is the global offset, not a relative to the start of a
        // section.
        symbol
            .n_value
            .get(LE)
            .checked_sub(section.addr.get(LE))
            .ok_or_else(|| error!("Mach-O symbol value is before its section address"))
    }

    fn num_sections(&self) -> usize {
        self.sections.len()
    }

    fn section_iter(&self) -> <Self::Platform as platform::Platform>::SectionIterator<'data> {
        self.sections.iter()
    }

    fn enumerate_sections(
        &self,
    ) -> impl Iterator<
        Item = (
            object::SectionIndex,
            &'data <Self::Platform as platform::Platform>::SectionHeader,
        ),
    > {
        self.sections
            .iter()
            .enumerate()
            .map(|(i, section)| (object::SectionIndex(i), section))
    }

    fn section(
        &self,
        index: object::SectionIndex,
    ) -> crate::error::Result<&'data <Self::Platform as platform::Platform>::SectionHeader> {
        self.sections
            .get(index.0)
            .ok_or(error!("section index out of range"))
    }

    fn section_by_name(
        &self,
        name: &str,
    ) -> Option<(
        object::SectionIndex,
        &'data <Self::Platform as platform::Platform>::SectionHeader,
    )> {
        let name = name.as_bytes();
        self.sections
            .iter()
            .enumerate()
            .find(|(_, section)| section.name() == name)
            .map(|(index, section)| (object::SectionIndex(index), section))
    }

    fn symbol_section(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        _index: object::SymbolIndex,
    ) -> crate::error::Result<Option<object::SectionIndex>> {
        if symbol.n_type & N_TYPE == N_SECT && symbol.n_sect != 0 {
            // The index is one-based, NO_SECT == 0, marks a missing section for the symbol.
            Ok(Some(object::SectionIndex(usize::from(symbol.n_sect - 1))))
        } else {
            Ok(None)
        }
    }

    fn symbol_versions(&self) -> &[<Self::Platform as platform::Platform>::SymbolVersionIndex] {
        &[]
    }

    fn dynamic_symbol_used(
        &self,
        symbol_index: object::SymbolIndex,
        state: &mut <Self::Platform as platform::Platform>::DynamicLayoutStateExt<'data>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn finalise_sizes_dynamic(
        &self,
        lib_name: &[u8],
        state: &mut <Self::Platform as platform::Platform>::DynamicLayoutStateExt<'data>,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn apply_non_addressable_indexes_dynamic(
        &self,
        indexes: &mut <Self::Platform as platform::Platform>::NonAddressableIndexes,
        counts: &mut <Self::Platform as platform::Platform>::NonAddressableCounts,
        state: &mut <Self::Platform as platform::Platform>::DynamicLayoutStateExt<'data>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn section_name(
        &self,
        section_header: &'data <Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<&'data [u8]> {
        Ok(section_header.name())
    }

    fn raw_section_data(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<&'data [u8]> {
        section
            .data(LE, self.data)
            .map_err(|_e| error!("cannot get section data"))
    }

    fn section_data(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
        member: &bumpalo_herd::Member<'data>,
        loaded_metrics: &crate::resolution::LoadedMetrics,
    ) -> crate::error::Result<&'data [u8]> {
        let data = self.raw_section_data(section)?;
        loaded_metrics
            .loaded_bytes
            .fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
        Ok(data)
    }

    fn copy_section_data(&self, section: &SectionHeader, out: &mut [u8]) -> Result {
        let data = section
            .data(LE, self.data)
            .map_err(|_e| error!("cannot get section data"))?;
        if section.is_no_bits() {
            out.fill(0);
        } else {
            copy_section_data(data, out);
        }

        Ok(())
    }

    fn section_data_cow(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<std::borrow::Cow<'data, [u8]>> {
        Ok(Cow::Borrowed(self.raw_section_data(section)?))
    }

    fn section_alignment(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<u64> {
        Ok(2u64.pow(section.align(LE)))
    }

    fn relocations(
        &self,
        index: object::SectionIndex,
        relocations: &<Self::Platform as platform::Platform>::RelocationSections,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RelocationList<'data>> {
        Ok(RelocationList {
            relocations: self
                .sections
                .get(index.0)
                .ok_or(error!("section index out of range"))?
                .relocations(LE, self.data)?,
        })
    }

    fn parse_relocations(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RelocationSections> {
        Ok(())
    }

    fn symbol_version_debug(&self, symbol_index: object::SymbolIndex) -> Option<String> {
        None
    }

    fn section_display_name(&self, index: object::SectionIndex) -> Cow<'data, str> {
        self.section(index)
            .and_then(|section| self.section_name(section))
            .map_or_else(
                |_| format!("<index {}>", index.0).into(),
                String::from_utf8_lossy,
            )
    }

    fn dynamic_tag_values(
        &self,
    ) -> Option<<Self::Platform as platform::Platform>::DynamicTagValues<'data>> {
        None
    }

    fn get_version_names(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::VersionNames<'data>> {
        Ok(())
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        local_index: usize,
        version_names: &<Self::Platform as platform::Platform>::VersionNames<'data>,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RawSymbolName<'data>> {
        Ok(RawSymbolName {
            name: self.symbol_name(symbol)?,
        })
    }

    fn should_enforce_undefined(
        &self,
        resources: &crate::layout::GraphResources<'data, '_, Self::Platform>,
    ) -> bool {
        false
    }

    fn verneed_table(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::VerneedTable<'data>> {
        Ok(VerneedTable { _phantom: &[] })
    }

    fn process_gnu_note_section(
        &self,
        state: &mut <Self::Platform as platform::Platform>::ObjectLayoutStateExt<'data>,
        section_index: object::SectionIndex,
    ) -> crate::error::Result {
        Ok(())
    }

    fn dynamic_tags(
        &self,
    ) -> crate::error::Result<&'data [<Self::Platform as platform::Platform>::DynamicEntry]> {
        Ok(&[])
    }
}

impl platform::SectionHeader for SectionHeader {
    fn is_alloc(&self) -> bool {
        !self.segname.starts_with(b"__DWARF") && self.flags.get(LE) & macho::S_ATTR_DEBUG == 0
    }

    fn is_writable(&self) -> bool {
        self.segname.starts_with(b"__DATA") && !self.segname.starts_with(b"__DATA_CONST")
    }

    fn is_executable(&self) -> bool {
        self.flags.get(LE) & (macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS)
            != 0
    }

    fn is_tls(&self) -> bool {
        matches!(
            self.section_type(LE),
            macho::S_THREAD_LOCAL_REGULAR
                | macho::S_THREAD_LOCAL_ZEROFILL
                | macho::S_THREAD_LOCAL_VARIABLES
                | macho::S_THREAD_LOCAL_VARIABLE_POINTERS
                | macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS
        )
    }

    fn is_merge_section(&self) -> bool {
        // TODO
        false
    }

    fn is_strings(&self) -> bool {
        self.flags.get(LE) & macho::SECTION_TYPE == macho::S_CSTRING_LITERALS
    }

    fn should_retain(&self) -> bool {
        self.flags.get(LE) & macho::S_ATTR_NO_DEAD_STRIP != 0
    }

    fn should_exclude(&self) -> bool {
        // TODO
        false
    }

    fn is_group(&self) -> bool {
        false
    }

    fn is_note(&self) -> bool {
        false
    }

    fn is_prog_bits(&self) -> bool {
        !self.is_no_bits()
    }

    fn is_no_bits(&self) -> bool {
        matches!(
            self.section_type(LE),
            macho::S_ZEROFILL | macho::S_GB_ZEROFILL | macho::S_THREAD_LOCAL_ZEROFILL
        )
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionType {}

impl platform::SectionType for SectionType {
    fn is_rela(&self) -> bool {
        false
    }

    fn is_rel(&self) -> bool {
        false
    }

    fn is_symtab(&self) -> bool {
        false
    }

    fn is_strtab(&self) -> bool {
        false
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionFlags(u32);

impl SectionFlags {
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn from_u32(raw: u32) -> SectionFlags {
        SectionFlags(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl platform::SectionFlags for SectionFlags {
    fn is_alloc(self) -> bool {
        true
    }
}

// Documentation link for Nlist64 type: https://leopard-adc.pepas.com/documentation/DeveloperTools/Conceptual/MachORuntime/Reference/reference.html
impl platform::Symbol for SymtabEntry {
    fn as_common(&self) -> Option<platform::CommonSymbol> {
        if self.n_type & (N_TYPE | N_EXT) != N_EXT {
            return None;
        }
        let size = self.n_value.get(LE);
        if size == 0 {
            return None;
        }

        let alignment_exponent = (self.n_desc.get(LE) >> 8) as u8;
        let alignment = Alignment::new(1u64.checked_shl(alignment_exponent.into())?).ok()?;
        let part_id = output_section_id::BSS.part_id_with_alignment(alignment);

        Some(platform::CommonSymbol {
            size: alignment.align_up(size),
            part_id,
        })
    }

    fn is_undefined(&self) -> bool {
        Nlist::is_undefined(self) && self.as_common().is_none()
    }

    fn is_local(&self) -> bool {
        self.n_type & N_EXT == 0
    }

    fn is_absolute(&self) -> bool {
        self.n_type & N_TYPE == N_ABS
    }

    fn is_weak(&self) -> bool {
        self.n_desc.get(LE) & N_WEAK_DEF != 0
    }

    fn visibility(&self) -> crate::symbol_db::Visibility {
        if self.n_type & N_PEXT != 0 {
            Visibility::Hidden
        } else {
            Visibility::Default
        }
    }

    fn value(&self) -> u64 {
        self.n_value.get(LE)
    }

    fn size(&self) -> u64 {
        self.as_common()
            .map(|common| common.size)
            .unwrap_or_default()
    }

    fn has_name(&self) -> bool {
        self.n_strx.get(LE) != 0
    }

    fn is_default_strippable(&self, name: &[u8]) -> bool {
        self.is_local()
            && (name.starts_with(b"ltmp")
                || name.starts_with(b".L")
                || name.starts_with(b"_.L")
                || name == b"l_.str"
                || name.starts_with(b"l_.str."))
    }

    fn debug_string(&self) -> String {
        // TODO
        String::new()
    }

    fn is_tls(&self) -> bool {
        // TODO: derive from section name
        false
    }

    fn is_interposable(&self) -> bool {
        false
    }

    fn is_func(&self) -> bool {
        // TODO: derive from section name
        false
    }

    fn is_ifunc(&self) -> bool {
        false
    }

    fn is_hidden(&self) -> bool {
        self.visibility() == Visibility::Hidden
    }

    fn is_gnu_unique(&self) -> bool {
        false
    }

    fn with_hidden(mut self, hidden: bool) -> Self {
        if hidden {
            self.n_type |= N_PEXT;
        } else {
            self.n_type &= !N_PEXT;
        }
        self
    }
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionAttributes {
    pub(crate) flags: SectionFlags,
    pub(crate) alloc: bool,
    pub(crate) writable: bool,
    pub(crate) executable: bool,
    pub(crate) tls: bool,
    pub(crate) no_bits: bool,
}

impl platform::SectionAttributes for SectionAttributes {
    type Platform = MachO;

    fn merge(&mut self, rhs: Self) {
        let was_unset = !self.alloc && self.flags.raw() == 0;
        self.flags = SectionFlags::from_u32(self.flags.raw() | rhs.flags.raw());
        self.alloc |= rhs.alloc;
        self.writable |= rhs.writable;
        self.executable |= rhs.executable;
        self.tls |= rhs.tls;
        self.no_bits = if was_unset {
            rhs.no_bits
        } else {
            self.no_bits && rhs.no_bits
        };
    }

    fn apply(
        &self,
        output_sections: &mut crate::output_section_id::OutputSections<Self::Platform>,
        section_id: crate::output_section_id::OutputSectionId,
    ) {
        output_sections
            .section_infos
            .get_mut(section_id)
            .section_attributes
            .merge(*self);
    }

    fn is_null(&self) -> bool {
        false
    }

    fn is_alloc(&self) -> bool {
        self.alloc
    }

    fn is_executable(&self) -> bool {
        self.executable
    }

    fn is_tls(&self) -> bool {
        self.tls
    }

    fn is_writable(&self) -> bool {
        self.writable
    }

    fn is_no_bits(&self) -> bool {
        self.no_bits
    }

    fn flags(&self) -> <Self::Platform as platform::Platform>::SectionFlags {
        self.flags
    }

    fn ty(&self) -> <Self::Platform as platform::Platform>::SectionType {
        SectionType {}
    }

    fn set_to_default_type(&mut self) {
        self.alloc = true;
        self.flags = SectionFlags::from_u32(macho::S_REGULAR);
    }
}

pub(crate) struct NonAddressableIndexes {}

impl platform::NonAddressableIndexes for NonAddressableIndexes {
    fn new<P: platform::Platform>(symbol_db: &crate::symbol_db::SymbolDb<P>) -> Self {
        NonAddressableIndexes {}
    }
}

// TODO: update comment

#[derive(Debug, Copy, Clone, Default, PartialEq)]
pub(crate) enum SegmentType {
    Text,
    LoadCommands,
    TextSections,
    DataSections,
    DataConstSections,
    LinkeditSections,
    // The other ELF-specific (or unused) parts/sections will be collected here.
    #[default]
    Unused,
}

impl platform::SegmentType for SegmentType {}

#[derive(Debug, Copy, Clone, Default, PartialEq)]
pub(crate) struct ProgramSegmentDef {
    pub(crate) segment_type: SegmentType,
}

impl std::fmt::Display for ProgramSegmentDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.segment_type)
    }
}

impl platform::ProgramSegmentDef for ProgramSegmentDef {
    type Platform = MachO;

    fn is_writable(self) -> bool {
        false
    }

    fn is_executable(self) -> bool {
        false
    }

    fn always_keep(self) -> bool {
        matches!(
            self.segment_type,
            SegmentType::Text
                | SegmentType::LoadCommands
                | SegmentType::TextSections
                | SegmentType::LinkeditSections
        )
    }

    fn is_loadable(self) -> bool {
        true
    }

    fn is_stack(self) -> bool {
        false
    }

    fn is_tls(self) -> bool {
        false
    }

    fn order_key(self) -> usize {
        self.segment_type as usize
    }

    fn should_include_section(
        self,
        section_info: &crate::output_section_id::SectionOutputInfo<Self::Platform>,
        section_id: crate::output_section_id::OutputSectionId,
    ) -> bool {
        let mapped_segment = match section_id {
            output_section_id::FILE_HEADER => SegmentType::Text,
            output_section_id::PAGEZERO_SEGMENT
            | output_section_id::TEXT_SEGMENT
            | output_section_id::DATA_SEGMENT
            | output_section_id::LINK_EDIT_SEGMENT
            | output_section_id::ENTRY_POINT
            | output_section_id::BUILD_VERSION
            | output_section_id::UUID_COMMAND
            | output_section_id::LIBSYSTEM
            | output_section_id::ID_DYLIB
            | output_section_id::INTERP
            | output_section_id::DYLD_CHAINED_FIXUPS
            | output_section_id::SYMTAB_COMMAND
            | output_section_id::CODE_SIGNATURE_COMMAND => SegmentType::LoadCommands,
            output_section_id::TEXT
            | output_section_id::PLT_GOT
            | output_section_id::GCC_EXCEPT_TABLE
            | output_section_id::MACHO_UNWIND_INFO
            | output_section_id::EH_FRAME => SegmentType::TextSections,
            output_section_id::GOT
            | output_section_id::RODATA
            | output_section_id::CSTRING
            | output_section_id::RUSTC_METADATA
            | output_section_id::DATA
            | output_section_id::TDATA
            | output_section_id::TBSS
            | output_section_id::BSS
            | output_section_id::MACHO_MOD_INIT_FUNC
            | output_section_id::MACHO_THREAD_VARS
            | output_section_id::MACHO_THREAD_PTRS => SegmentType::DataSections,
            output_section_id::CHAINED_FIXUP_TABLE
            | output_section_id::DYNSYM
            | output_section_id::SYMTAB_GLOBAL
            | output_section_id::STRTAB
            | output_section_id::CODE_SIGNATURE => SegmentType::LinkeditSections,
            _ if section_info.section_attributes.is_executable() => SegmentType::TextSections,
            _ if section_info.section_attributes.is_alloc()
                && !section_info.section_attributes.is_writable() =>
            {
                SegmentType::DataSections
            }
            _ if section_info.section_attributes.is_alloc() => SegmentType::DataSections,
            _ => SegmentType::Unused,
        };

        match (self.segment_type, mapped_segment) {
            (SegmentType::Text, SegmentType::LoadCommands | SegmentType::TextSections) => true,
            _ => self.segment_type == mapped_segment,
        }
    }
}

pub(crate) struct BuiltInSectionDetails {
    pub(crate) kind: SectionKind<'static>,
    pub(crate) section_flags: SectionFlags,
    pub(crate) min_alignment: Alignment,
    pub(crate) target_segment_type: Option<SegmentType>,
}

impl platform::BuiltInSectionDetails for BuiltInSectionDetails {}

const DEFAULT_DEFS: BuiltInSectionDetails = BuiltInSectionDetails {
    kind: SectionKind::Primary(SectionName(&[])),
    section_flags: SectionFlags::empty(),
    min_alignment: alignment::MIN,
    target_segment_type: None,
};

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct DynamicTagValues<'data> {
    phantom: &'data [u8],
}

#[derive(Debug)]
pub(crate) struct RelocationList<'data> {
    pub(crate) relocations: &'data [Relocation],
}

impl<'data> platform::RelocationList<'data> for RelocationList<'data> {
    fn num_relocations(&self) -> usize {
        self.relocations.len()
    }
}

impl<'data> platform::DynamicTagValues<'data> for DynamicTagValues<'data> {
    fn lib_name(&self, input: &crate::input_data::InputRef<'data>) -> &'data [u8] {
        &[]
    }
}

#[derive(Debug)]
pub(crate) struct RawSymbolName<'data> {
    pub(crate) name: &'data [u8],
}

impl<'data> platform::RawSymbolName<'data> for RawSymbolName<'data> {
    fn parse(bytes: &'data [u8]) -> Self {
        Self { name: bytes }
    }

    fn name(&self) -> &'data [u8] {
        self.name
    }

    fn version_name(&self) -> Option<&'data [u8]> {
        None
    }

    fn is_default(&self) -> bool {
        // This port does not use symbol versioning, so every symbol is treated as
        // the default version.
        true
    }
}

impl std::fmt::Display for RawSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.name))
    }
}

pub(crate) struct VerneedTable<'data> {
    // TODO
    _phantom: &'data [u8],
}

impl<'data> platform::VerneedTable<'data> for VerneedTable<'data> {
    fn version_name(&self, local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]> {
        None
    }
}

impl platform::Platform for MachO {
    type File<'data> = File<'data>;
    type SymtabEntry = SymtabEntry;
    type SectionHeader = SectionHeader;
    type SectionFlags = SectionFlags;
    type SectionAttributes = SectionAttributes;
    type SectionType = SectionType;
    type SegmentType = SegmentType;
    type ProgramSegmentDef = ProgramSegmentDef;
    type BuiltInSectionDetails = BuiltInSectionDetails;
    type RelocationSections = ();
    type DynamicEntry = ();
    type DynamicSymbolDefinitionExt = ();
    type RelocationInfo = object::macho::RelocationInfo;
    type NonAddressableIndexes = NonAddressableIndexes;
    type NonAddressableCounts = ();
    type EpilogueLayoutExt = ();
    type GroupLayoutExt = ();
    type CommonGroupStateExt = ();
    type ArchIdentifier = ();
    type Args = MachOArgs;
    type ResolutionExt = ResolutionExt;
    type SymtabShndxEntry = ();
    type SymbolVersionIndex = ();
    type LayoutExt = ();
    type SectionIterator<'data> = core::slice::Iter<'data, SectionHeader>;
    type DynamicTagValues<'data> = DynamicTagValues<'data>;
    type RelocationList<'data> = RelocationList<'data>;
    type DynamicLayoutStateExt<'data> = ();
    type DynamicLayoutExt<'data> = ();
    type LayoutResourcesExt<'data> = ();
    type PreludeLayoutStateExt = ();
    type PreludeLayoutExt = ();
    type ObjectLayoutStateExt<'data> = ObjectLayoutStateExt;
    type RawSymbolName<'data> = RawSymbolName<'data>;
    type VersionNames<'data> = ();
    type VerneedTable<'data> = VerneedTable<'data>;

    fn link_for_arch<'data>(
        linker: &'data crate::Linker,
        args: &'data Self::Args,
    ) -> crate::error::Result<crate::LinkerOutput<'data>> {
        linker.link_for_arch::<MachO, crate::macho_aarch64::MachOAArch64>(args)
    }

    fn write_output_file<'data, A: platform::Arch<Platform = Self>>(
        output: &crate::file_writer::Output,
        layout: &crate::layout::Layout<'data, Self>,
        incremental: &crate::incremental::PreparedState,
    ) -> crate::error::Result {
        output.write(layout, |sized_output, layout| {
            macho_writer::write::<A>(sized_output, layout, incremental)
        })?;
        #[cfg(target_os = "macos")]
        if layout.args().should_adhoc_codesign && !layout.symbol_db.output_kind.is_partial_object()
        {
            ad_hoc_codesign(output.path())?;
        }
        Ok(())
    }

    fn section_attributes(header: &Self::SectionHeader) -> Self::SectionAttributes {
        Self::SectionAttributes {
            flags: SectionFlags::from_u32(header.flags.get(LE)),
            alloc: header.is_alloc(),
            writable: header.is_writable(),
            executable: header.is_executable(),
            tls: header.is_tls(),
            no_bits: header.is_no_bits(),
        }
    }

    fn apply_force_keep_sections(
        keep_sections: &mut crate::output_section_map::OutputSectionMap<bool>,
        args: &Self::Args,
    ) {
    }

    fn is_zero_sized_section_content(
        section_id: crate::output_section_id::OutputSectionId,
    ) -> bool {
        false
    }

    fn built_in_section_details() -> &'static [Self::BuiltInSectionDetails] {
        &SECTION_DEFINITIONS
    }

    fn finalise_group_layout(
        _common: &crate::layout::CommonGroupState<Self>,
        _memory_offsets: &crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> Self::GroupLayoutExt {
    }

    fn frame_data_base_address(
        memory_offsets: &crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> u64 {
        0
    }

    fn activate_dynamic<'data>(
        state: &mut crate::layout::DynamicLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) {
        todo!()
    }

    fn pre_finalise_sizes_prelude<'scope, 'data>(
        prelude: &mut crate::layout::PreludeLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        resources: &crate::layout::GraphResources<'data, 'scope, Self>,
    ) {
    }

    fn finalise_sizes_dynamic<'data>(
        object: &mut crate::layout::DynamicLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
    ) -> crate::error::Result {
        todo!()
    }

    fn finalise_object_sizes<'data>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        output_sections: &crate::output_section_id::OutputSections<Self>,
        per_symbol_flags: &AtomicPerSymbolFlags,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
        compact_dead_macho_subsections(
            object,
            common,
            output_sections,
            per_symbol_flags,
            symbol_db,
        );
        compact_macho_eh_frame(object, common, output_sections, symbol_db);
    }

    fn finalise_object_layout<'data>(
        object: &crate::layout::ObjectLayoutState<'data, Self>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
    }

    fn file_thunk_config<'data>(file: &File<'data>) -> Option<crate::platform::ThunkConfig> {
        <crate::macho_aarch64::MachOAArch64 as platform::Arch>::thunk_config()
    }

    fn finalise_layout_dynamic<'data>(
        state: &mut crate::layout::DynamicLayoutState<'data, Self>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
        resolutions_out: &mut crate::layout::ResolutionWriter<Self>,
    ) -> crate::error::Result<Self::DynamicLayoutExt<'data>> {
        todo!()
    }

    fn take_dynsym_index(
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        section_layouts: &crate::output_section_map::OutputSectionMap<
            crate::layout::OutputRecordLayout,
        >,
    ) -> crate::error::Result<u32> {
        let index = ((memory_offsets.get(part_id::DYNSYM)
            - section_layouts.get(output_section_id::DYNSYM).mem_offset)
            / size_of::<SymtabEntry>() as u64)
            .try_into()
            .context("Too many Mach-O dynamic symbols")?;
        memory_offsets.increment(part_id::DYNSYM, size_of::<SymtabEntry>() as u64);
        Ok(index)
    }

    fn compute_object_addresses<'data>(
        object: &crate::layout::ObjectLayoutState<'data, Self>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
    }

    fn layout_resources_ext<'data>(
        groups: &[crate::grouping::Group<'data, Self>],
    ) -> Self::LayoutResourcesExt<'data> {
    }

    fn load_object_section_relocations<'data, 'scope, A: platform::Arch<Platform = Self>>(
        state: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        queue: &mut crate::layout::LocalWorkQueue,
        resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        section: crate::layout::Section,
        section_index: object::SectionIndex,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        // TODO
        let section_part_id =
            state.section_part_id(section_index, &resources.symbol_db.section_part_ids);
        let section_header = state.object.section(section_index)?;
        if macho_subsection_gc_enabled(state, section_index, resources.symbol_db.args)? {
            // Relocations in these sections are traversed atom-by-atom from
            // `load_object_symbol`, so section materialisation alone must not
            // make every atom reachable.
            // Compaction retains no-dead-strip atoms in a loaded section, so
            // traverse them as roots before their relocations reach the writer.
            let mut no_dead_strip_symbols = Vec::new();
            for (symbol_index, symbol) in state.object.enumerate_symbols() {
                if symbol.n_desc.get(LE) & (N_ALT_ENTRY | N_NO_DEAD_STRIP) != N_NO_DEAD_STRIP {
                    continue;
                }
                if state.object.symbol_section(symbol, symbol_index)? == Some(section_index) {
                    no_dead_strip_symbols.push(symbol_index);
                }
            }
            for symbol_index in no_dead_strip_symbols {
                load_macho_subsection_symbol::<A>(
                    state,
                    common,
                    symbol_index,
                    section_index,
                    resources,
                    queue,
                    scope,
                )?;
            }
            return Ok(());
        }
        if state.object.section_name(section_header)? == b"__eh_frame" {
            let data = state.object.raw_section_data(section_header)?;
            common.allocate(
                part_id::MACHO_UNWIND_INFO,
                macho_unwind_info_allocation_size(macho_eh_frame_fde_count(data)? * 2),
            );
            if macho_unwind_atom_gc_enabled(state, resources.symbol_db.args) {
                return Ok(());
            }
        }

        for rel in state.relocations(section_index)?.relocations {
            process_relocation::<A>(
                state,
                common,
                rel,
                section_part_id,
                resources,
                queue,
                false,
                ValueFlags::empty(),
                scope,
            )?;
        }
        Ok(())
    }

    fn load_object_symbol<'data, 'scope, A: platform::Arch<Platform = Self>>(
        state: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        symbol_id: crate::symbol_db::SymbolId,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        queue: &mut crate::layout::LocalWorkQueue,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result<bool> {
        let symbol_index = state.symbol_id_range.id_to_input(symbol_id);
        let symbol = state.object.symbol(symbol_index)?;
        let Some(section_index) = state.object.symbol_section(symbol, symbol_index)? else {
            return Ok(false);
        };
        if !macho_subsection_gc_enabled(state, section_index, resources.symbol_db.args)? {
            return Ok(false);
        }
        load_macho_subsection_symbol::<A>(
            state,
            common,
            symbol_index,
            section_index,
            resources,
            queue,
            scope,
        )
    }

    fn create_dynamic_symbol_definition<'data>(
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        symbol_id: crate::symbol_db::SymbolId,
    ) -> crate::error::Result<crate::layout::DynamicSymbolDefinition<'data, Self>> {
        Ok(crate::layout::DynamicSymbolDefinition {
            symbol_id,
            name: symbol_db.symbol_name(symbol_id)?.bytes(),
            format_specific: (),
        })
    }

    fn update_segment_keep_list(
        program_segments: &crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
        keep_segments: &mut [bool],
        args: &Self::Args,
    ) {
    }

    fn program_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        PROGRAM_SEGMENT_DEFS
    }

    fn unconditional_segment_defs() -> &'static [Self::ProgramSegmentDef] {
        &[]
    }

    fn create_linker_defined_symbols(
        symbols: &mut crate::parsing::InternalSymbolsBuilder<Self>,
        output_kind: crate::output_kind::OutputKind,
        args: &Self::Args,
    ) {
        // Mach-O symbol tables don't reserve symbol index 0, but sld reserves SymbolId 0 as the
        // undefined symbol. Keep the prelude sentinel so real input symbols start after it.
        symbols
            .add_symbol(crate::parsing::InternalSymDefInfo::new(
                crate::parsing::SymbolPlacement::Undefined,
                b"",
            ))
            .hide();

        symbols
            .section_start(output_section_id::FILE_HEADER, "___dso_handle")
            .hide();
    }

    fn built_in_section_infos<'data>()
    -> Vec<crate::output_section_id::SectionOutputInfo<'data, Self>> {
        SECTION_DEFINITIONS
            .iter()
            .map(|d| SectionOutputInfo {
                section_attributes: SectionAttributes {
                    flags: d.section_flags,
                    no_bits: section_flags_are_no_bits(d.section_flags),
                    ..Default::default()
                },
                kind: d.kind,
                min_alignment: d.min_alignment,
                location: None,
                secondary_order: None,
            })
            .collect()
    }

    fn create_layout_properties<'data, 'states, 'files, A: platform::Arch<Platform = Self>>(
        args: &Self::Args,
        objects: impl Iterator<Item = &'files Self::File<'data>>,
        states: impl Iterator<Item = &'states Self::ObjectLayoutStateExt<'data>> + Clone,
    ) -> crate::error::Result<Self::LayoutExt>
    where
        'data: 'files,
        'data: 'states,
    {
        Ok(())
    }

    fn load_exception_frame_data<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        compact_unwind_section_index: object::SectionIndex,
        resources: &'scope crate::layout::GraphResources<'data, '_, Self>,
        queue: &mut crate::layout::LocalWorkQueue,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        let section = object.object.section(compact_unwind_section_index)?;
        let name = object.object.section_name(section)?;
        if name != b"__compact_unwind" {
            return Ok(());
        }

        let data = object.object.raw_section_data(section)?;
        ensure!(
            data.len() % MACHO_COMPACT_UNWIND_ENTRY_SIZE == 0,
            "__compact_unwind size must be a multiple of {MACHO_COMPACT_UNWIND_ENTRY_SIZE}"
        );
        common.allocate(
            part_id::MACHO_UNWIND_INFO,
            macho_unwind_info_allocation_size(data.len() / MACHO_COMPACT_UNWIND_ENTRY_SIZE * 2),
        );
        if macho_unwind_atom_gc_enabled(object, resources.symbol_db.args) {
            return Ok(());
        }

        for rel in object
            .relocations(compact_unwind_section_index)?
            .relocations
        {
            let rel_info = rel.info(LE);
            if rel_info.r_type == macho::ARM64_RELOC_ADDEND {
                continue;
            }

            let offset = rel_info.r_address as usize;
            ensure!(
                offset < data.len(),
                "Mach-O __compact_unwind relocation at invalid offset {offset:#x}"
            );
            let field_offset = offset % MACHO_COMPACT_UNWIND_ENTRY_SIZE;
            ensure!(
                matches!(field_offset, 0 | 16 | 24),
                "Unsupported Mach-O __compact_unwind relocation field offset {field_offset:#x}"
            );
            let extra_flags = if rel_info.r_extern && field_offset == 16 {
                ValueFlags::GOT
            } else {
                ValueFlags::empty()
            };
            process_relocation::<A>(
                object,
                common,
                rel,
                part_id::MACHO_UNWIND_INFO,
                resources,
                queue,
                true,
                extra_flags,
                scope,
            )?;
        }
        Ok(())
    }

    fn non_empty_section_loaded<'data, 'scope, A: platform::Arch<Platform = Self>>(
        object: &mut crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        queue: &mut crate::layout::LocalWorkQueue,
        unloaded: crate::resolution::UnloadedSection,
        resources: &'scope crate::layout::GraphResources<'data, 'scope, Self>,
        scope: &rayon::Scope<'scope>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn new_epilogue_layout(
        args: &Self::Args,
        output_kind: crate::output_kind::OutputKind,
        dynamic_symbol_definitions: &mut [crate::layout::DynamicSymbolDefinition<'_, Self>],
    ) -> Self::EpilogueLayoutExt {
    }

    fn apply_non_addressable_indexes_epilogue(
        counts: &mut Self::NonAddressableCounts,
        state: &mut Self::EpilogueLayoutExt,
    ) {
    }

    fn apply_non_addressable_indexes<'data, 'groups>(
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        counts: &Self::NonAddressableCounts,
        mem_sizes_iter: impl Iterator<
            Item = &'groups mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        >,
    ) {
    }

    fn finalise_sizes_epilogue<'data>(
        state: &mut Self::EpilogueLayoutExt,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        dynamic_symbol_definitions: &[crate::layout::DynamicSymbolDefinition<'data, Self>],
        properties: &Self::LayoutExt,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
        if symbol_db.output_kind.needs_dynsym() {
            mem_sizes.increment(
                part_id::DYNSYM,
                dynamic_symbol_definitions.len() as u64 * size_of::<SymtabEntry>() as u64,
            );
        }
    }

    fn finalise_sizes_all<'data>(
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
    ) {
    }

    fn apply_late_size_adjustments_epilogue(
        state: &mut Self::EpilogueLayoutExt,
        current_sizes: &crate::output_section_part_map::OutputSectionPartMap<u64>,
        extra_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        dynamic_symbol_defs: &[crate::layout::DynamicSymbolDefinition<Self>],
        args: &Self::Args,
    ) -> crate::error::Result {
        Ok(())
    }

    fn finalise_layout_epilogue<'data>(
        epilogue_state: &mut Self::EpilogueLayoutExt,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        common_state: &Self::LayoutExt,
        dynsym_start_index: u32,
        dynamic_symbol_defs: &[crate::layout::DynamicSymbolDefinition<Self>],
    ) -> crate::error::Result {
        memory_offsets.increment(
            part_id::DYNSYM,
            dynamic_symbol_defs.len() as u64 * size_of::<SymtabEntry>() as u64,
        );
        Ok(())
    }

    fn is_symbol_non_interposable<'data>(
        object: &Self::File<'data>,
        args: &Self::Args,
        sym: &Self::SymtabEntry,
        output_kind: crate::output_kind::OutputKind,
        export_list: Option<&crate::export_list::ExportList>,
        lib_name: &[u8],
        archive_semantics: bool,
        is_undefined: bool,
    ) -> bool {
        // TODO
        true
    }

    fn allow_duplicate_definition<'data>(
        args: &Self::Args,
        symbol_db: &SymbolDb<'data, Self>,
        existing: SymbolId,
        duplicate: SymbolId,
    ) -> bool {
        if !args.dead_strip {
            return false;
        }

        if symbol_db.input_symbol_visibility(existing) != Visibility::Hidden
            || symbol_db.input_symbol_visibility(duplicate) != Visibility::Hidden
        {
            return false;
        }

        let is_archive_entry = |symbol_id| {
            let file_id = symbol_db.file_id_for_symbol(symbol_id);
            match symbol_db.file(file_id) {
                crate::grouping::SequencedInput::Object(obj) => {
                    obj.parsed.input.has_archive_semantics()
                }
                _ => false,
            }
        };

        is_archive_entry(existing) && is_archive_entry(duplicate)
    }

    fn has_data_in_file(section_attributes: Self::SectionAttributes) -> bool {
        // Mach-O segments are page-mapped by the kernel. If a zero-fill output section has no
        // backing file space, later file contents can be observed in the zero-fill virtual range
        // when another segment starts at the same file offset.
        !section_attributes.is_no_bits() || section_flags_are_no_bits(section_attributes.flags)
    }

    fn allocate_header_sizes(
        _prelude: &mut crate::layout::PreludeLayoutState<Self>,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        header_info: &crate::layout::HeaderInfo,
        output_sections: &crate::output_section_id::OutputSections<Self>,
        args: &Self::Args,
        _output_kind: OutputKind,
    ) {
        sizes.increment(part_id::FILE_HEADER, size_of::<FileHeader>() as u64);
        sizes.increment(
            part_id::PAGEZERO_SEGMENT,
            size_of::<SegmentCommand>() as u64,
        );
        sizes.increment(
            part_id::TEXT_SEGMENT,
            (size_of::<SegmentCommand>()
                + size_of::<SectionEntry>()
                    * count_sections_for_segment_type(output_sections, SegmentType::TextSections))
                as u64,
        );
        if has_active_segment(header_info, SegmentType::DataSections) {
            sizes.increment(
                part_id::DATA_SEGMENT,
                (size_of::<SegmentCommand>()
                    + size_of::<SectionEntry>()
                        * count_sections_for_segment_type(
                            output_sections,
                            SegmentType::DataSections,
                        )) as u64,
            );
        }
        sizes.increment(
            part_id::LINK_EDIT_SEGMENT,
            size_of::<SegmentCommand>() as u64,
        );
        sizes.increment(
            part_id::BUILD_VERSION,
            size_of::<BuildVersionCommand>() as u64,
        );
        sizes.increment(part_id::UUID_COMMAND, size_of::<UuidCommand>() as u64);
        sizes.increment(part_id::LIBSYSTEM, load_dylib_commands_size(args) as u64);
        if !args.is_dynamiclib {
            sizes.increment(part_id::ENTRY_POINT, size_of::<EntryPointCommand>() as u64);
            sizes.increment(
                part_id::INTERP,
                ((size_of::<DylinkerCommand>() + DYLINKER_PATH.len() + 1)
                    .next_multiple_of(MACHO_COMMAND_ALIGNMENT)) as u64,
            );
        }
        if args.is_dynamiclib {
            sizes.increment(part_id::ID_DYLIB, id_dylib_command_size(args) as u64);
        }
        sizes.increment(
            part_id::DYLD_CHAINED_FIXUPS,
            size_of::<DyldChainedFixupsCommand>() as u64,
        );
        sizes.increment(part_id::SYMTAB_COMMAND, size_of::<SymtabCommand>() as u64);
        sizes.increment(
            part_id::CODE_SIGNATURE_COMMAND,
            size_of::<CodeSignatureCommand>() as u64,
        );
    }

    fn finalise_sizes_for_symbol<'data>(
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        _symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        _symbol_id: crate::symbol_db::SymbolId,
        flags: crate::value_flags::ValueFlags,
    ) -> crate::error::Result {
        if flags.is_dynamic() && flags.has_resolution() {
            common.allocate(part_id::DYNSYM, size_of::<SymtabEntry>() as u64);
        }
        Ok(())
    }

    fn allocate_resolution(
        flags: crate::value_flags::ValueFlags,
        mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        output_kind: crate::output_kind::OutputKind,
        _args: &Self::Args,
    ) {
        if flags.needs_got() {
            mem_sizes.increment(part_id::GOT, MACHO_GOT_ENTRY_SIZE);
        }
        if flags.needs_plt() {
            mem_sizes.increment(part_id::PLT_GOT, MACHO_STUB_SIZE);
        }
    }

    fn allocate_object_symtab_space<'data>(
        state: &crate::layout::ObjectLayoutState<'data, Self>,
        common: &mut crate::layout::CommonGroupState<'data, Self>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, Self>,
        per_symbol_flags: &crate::value_flags::AtomicPerSymbolFlags,
    ) -> Result {
        let mut num_globals = 0;
        let mut strings_size = 0;
        for ((sym_index, sym), flags) in state
            .object
            .enumerate_symbols()
            .zip(per_symbol_flags.range(state.symbol_id_range))
        {
            let symbol_id = state.symbol_id_range.input_to_id(sym_index);
            if let Some(info) = SymbolCopyInfo::new(
                state.object,
                sym_index,
                sym,
                symbol_id,
                symbol_db,
                flags.get(),
                &state.sections,
                state.section_relax_deltas(),
            ) {
                num_globals += 1;
                strings_size += info.name.len() + 1;
            }
        }
        let entry_size = size_of::<SymtabEntry>() as u64;
        common.allocate(part_id::SYMTAB_GLOBAL, num_globals * entry_size);
        common.allocate(part_id::STRTAB, strings_size as u64);

        Ok(())
    }

    fn allocate_internal_symbol(
        symbol_id: crate::symbol_db::SymbolId,
        def_info: &crate::parsing::InternalSymDefInfo<Self>,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        symbol_db: &crate::symbol_db::SymbolDb<Self>,
    ) -> crate::error::Result {
        sizes.increment(part_id::SYMTAB_GLOBAL, size_of::<SymtabEntry>() as u64);
        let symbol_name = symbol_db.symbol_name(symbol_id)?;
        sizes.increment(part_id::STRTAB, symbol_name.bytes().len() as u64 + 1);

        Ok(())
    }

    fn allocate_prelude(
        common: &mut crate::layout::CommonGroupState<Self>,
        symbol_db: &crate::symbol_db::SymbolDb<Self>,
    ) {
        if symbol_db.output_kind.needs_dynsym() {
            common.allocate(part_id::DYNSYM, size_of::<SymtabEntry>() as u64);
        }
        // Allocate one extra character as n_strx == 0 is treated as unnamed.
        common.allocate(part_id::STRTAB, 1);
        common.allocate(part_id::CODE_SIGNATURE, CS_HEADERS_WITH_FILENAME_SIZE);
        common.allocate(
            part_id::CHAINED_FIXUP_TABLE,
            chained_fixup_table_allocation_size(common, symbol_db),
        );
    }

    fn finalise_prelude_layout<'data>(
        prelude: &crate::layout::PreludeLayoutState<Self>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
        resources: &crate::layout::FinaliseLayoutResources<'_, 'data, Self>,
    ) -> crate::error::Result<Self::PreludeLayoutExt> {
        if resources.symbol_db.output_kind.needs_dynsym() {
            MachO::take_dynsym_index(memory_offsets, resources.section_layouts)?;
        }
        Ok(())
    }

    fn create_resolution(
        flags: crate::value_flags::ValueFlags,
        raw_value: u64,
        dynamic_symbol_index: Option<std::num::NonZeroU32>,
        memory_offsets: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> crate::layout::Resolution<Self> {
        let got_address = flags.needs_got().then(|| {
            let address = NonZeroU64::new(*memory_offsets.get(part_id::GOT))
                .expect("Mach-O GOT address must never be zero");
            memory_offsets.increment(part_id::GOT, MACHO_GOT_ENTRY_SIZE);
            address
        });
        let stub_address = flags.needs_plt().then(|| {
            let address = NonZeroU64::new(*memory_offsets.get(part_id::PLT_GOT))
                .expect("Mach-O stub address must never be zero");
            memory_offsets.increment(part_id::PLT_GOT, MACHO_STUB_SIZE);
            address
        });
        Resolution {
            raw_value,
            dynamic_symbol_index,
            format_specific: ResolutionExt {
                got_address,
                stub_address,
                is_import: flags.is_absolute() && got_address.is_some(),
            },
            flags,
        }
    }

    fn raw_symbol_name<'data>(
        name_bytes: &'data [u8],
        _verneed_table: &Self::VerneedTable<'data>,
        _symbol_index: object::SymbolIndex,
    ) -> Self::RawSymbolName<'data> {
        RawSymbolName { name: name_bytes }
    }

    fn default_layout_rules(args: &Self::Args) -> Vec<crate::layout_rules::SectionRule<'static>> {
        let gc_rule = |name, section_id| {
            if args.dead_strip {
                SectionRule::exact_section(name, section_id)
            } else {
                SectionRule::exact_section_keep(name, section_id)
            }
        };

        let mut rules = Vec::with_capacity(DEFAULT_SECTION_RULES.len() + 7);
        rules.push(gc_rule(b"__text", crate::output_section_id::TEXT));
        rules.push(gc_rule(
            b"__gcc_except_tab",
            crate::output_section_id::GCC_EXCEPT_TABLE,
        ));
        rules.extend(DEFAULT_SECTION_RULES.iter().cloned());
        rules.push(gc_rule(b"__const", crate::output_section_id::RODATA));
        rules.push(gc_rule(b"__cstring", crate::output_section_id::CSTRING));
        rules.push(gc_rule(b"__data", crate::output_section_id::DATA));
        rules.push(gc_rule(b"__bss", crate::output_section_id::BSS));
        rules.push(gc_rule(b"__common", crate::output_section_id::BSS));
        rules
    }

    fn build_output_order_and_program_segments<'data>(
        custom: &crate::output_section_id::CustomSectionIds,
        output_kind: OutputKind,
        output_sections: &crate::output_section_id::OutputSections<'data, Self>,
        secondary: &crate::output_section_map::OutputSectionMap<
            Vec<crate::output_section_id::OutputSectionId>,
        >,
    ) -> (
        crate::output_section_id::OutputOrder,
        crate::program_segments::ProgramSegments<Self::ProgramSegmentDef>,
    ) {
        let mut builder = OutputOrderBuilder::<Self>::new(output_kind, output_sections, secondary);

        // File header and all load commands.
        builder.add_section(output_section_id::FILE_HEADER);
        builder.add_section(output_section_id::PAGEZERO_SEGMENT);
        builder.add_section(output_section_id::TEXT_SEGMENT);
        builder.add_section(output_section_id::DATA_SEGMENT);
        builder.add_section(output_section_id::LINK_EDIT_SEGMENT);
        builder.add_section(output_section_id::ENTRY_POINT);
        builder.add_section(output_section_id::BUILD_VERSION);
        builder.add_section(output_section_id::UUID_COMMAND);
        builder.add_section(output_section_id::LIBSYSTEM);
        builder.add_section(output_section_id::ID_DYLIB);
        builder.add_section(output_section_id::INTERP); // DYLINKER
        builder.add_section(output_section_id::DYLD_CHAINED_FIXUPS);
        builder.add_section(output_section_id::SYMTAB_COMMAND);
        builder.add_section(output_section_id::CODE_SIGNATURE_COMMAND);
        // Content of the sections (e.g. __text, __data).
        builder.add_section(output_section_id::PLT_GOT);
        builder.add_sections(&custom.exec);
        builder.add_section(output_section_id::TEXT);
        builder.add_section(output_section_id::GCC_EXCEPT_TABLE);
        builder.add_section(output_section_id::MACHO_UNWIND_INFO);
        builder.add_section(output_section_id::EH_FRAME);
        builder.add_section(output_section_id::RODATA);
        builder.add_section(output_section_id::CSTRING);
        builder.add_sections(&custom.ro);
        builder.add_section(output_section_id::GOT);
        builder.add_section(output_section_id::RUSTC_METADATA);
        builder.add_section(output_section_id::DATA);
        builder.add_section(output_section_id::MACHO_MOD_INIT_FUNC);
        builder.add_section(output_section_id::MACHO_THREAD_VARS);
        builder.add_section(output_section_id::MACHO_THREAD_PTRS);
        builder.add_section(output_section_id::TDATA);
        builder.add_section(output_section_id::TBSS);
        builder.add_section(output_section_id::BSS);
        builder.add_sections(&custom.data);
        builder.add_sections(&custom.bss);
        // The rest (e.g. symbol table, string table).
        builder.add_section(output_section_id::CHAINED_FIXUP_TABLE);
        builder.add_section(output_section_id::DYNSYM);
        builder.add_section(output_section_id::SYMTAB_GLOBAL);
        builder.add_section(output_section_id::STRTAB);
        builder.add_section(output_section_id::CODE_SIGNATURE);

        builder.build()
    }

    fn start_memory_address(output_kind: OutputKind) -> u64 {
        MACHO_START_MEM_ADDRESS
    }

    fn align_load_segment_start(
        segment_def: ProgramSegmentDef,
        segment_alignment: Alignment,
        file_offset: &mut usize,
        mem_offset: &mut u64,
    ) {
        match segment_def.segment_type {
            SegmentType::Text
            | SegmentType::DataSections
            | SegmentType::DataConstSections
            | SegmentType::LinkeditSections => {
                *file_offset = segment_alignment.align_up(*file_offset as u64) as usize;
                *mem_offset = segment_alignment.align_up(*mem_offset);
            }
            _ => {}
        }
    }

    fn default_symtab_entry() -> Self::SymtabEntry {
        Self::SymtabEntry {
            n_strx: Default::default(),
            n_type: Default::default(),
            n_sect: Default::default(),
            n_desc: Default::default(),
            n_value: Default::default(),
        }
    }

    fn last_part_size_to_extend(
        record: &OutputRecordLayout,
        last_part_id: part_id::PartId,
    ) -> Result<usize> {
        ensure!(
            last_part_id == part_id::CODE_SIGNATURE,
            "code signature must be last part_id"
        );
        // The CODE_SIGNATURE size depends on the final file size, excluding the
        // signature itself. Compute it after layout because there is one SHA hash
        // per file block (4 KiB) covered by the signature.
        Ok(record.file_offset.div_ceil(CS_BLOCK_SIZE) * CS_HASH_SIZE as usize)
    }
}

#[cfg(target_os = "macos")]
fn ad_hoc_codesign(path: &Path) -> Result {
    timing_phase!("Ad-hoc code sign Mach-O output");

    let output = std::process::Command::new("codesign")
        .arg("-s")
        .arg("-")
        .arg("-f")
        .arg(path)
        .output()
        .with_context(|| format!("Failed to run codesign for `{}`", path.display()))?;

    ensure!(
        output.status.success(),
        "codesign failed for `{}`:\n{}{}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}

fn section_flags_are_no_bits(flags: SectionFlags) -> bool {
    matches!(
        flags.raw() & macho::SECTION_TYPE,
        macho::S_ZEROFILL | macho::S_GB_ZEROFILL | macho::S_THREAD_LOCAL_ZEROFILL
    )
}

fn should_relax_got_load_to_direct(
    output_kind: crate::output_kind::OutputKind,
    relocation_type: u8,
    is_undefined: bool,
) -> bool {
    output_kind.is_executable()
        && !is_undefined
        && matches!(
            relocation_type,
            macho::ARM64_RELOC_GOT_LOAD_PAGE21 | macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12
        )
}

fn macho_subsection_gc_enabled<'data>(
    state: &crate::layout::ObjectLayoutState<'data, MachO>,
    section_index: object::SectionIndex,
    args: &MachOArgs,
) -> Result<bool> {
    if !args.dead_strip || state.object.flags & macho::MH_SUBSECTIONS_VIA_SYMBOLS == 0 {
        return Ok(false);
    }

    let section = state.object.section(section_index)?;
    let section_name = state.object.section_name(section)?;
    Ok(!section.should_retain()
        && (section.is_executable()
            || section_name == b"__gcc_except_tab"
            || section_name == b"__const"))
}

fn load_macho_subsection_symbol<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    symbol_index: object::SymbolIndex,
    section_index: object::SectionIndex,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, MachO>,
    queue: &mut crate::layout::LocalWorkQueue,
    scope: &rayon::Scope<'scope>,
) -> Result<bool> {
    let Some((start, end, newly_live)) =
        mark_macho_subsection_live(state, symbol_index, section_index)?
    else {
        return Ok(false);
    };

    queue.send_section_request::<A>(state.file_id, section_index, resources, scope);

    if newly_live {
        let section_part_id =
            state.section_part_id(section_index, &resources.symbol_db.section_part_ids);
        for rel in state.relocations(section_index)?.relocations {
            let rel_offset = u64::from(rel.info(LE).r_address);
            if rel_offset < start || rel_offset >= end {
                continue;
            }
            process_relocation::<A>(
                state,
                common,
                rel,
                section_part_id,
                resources,
                queue,
                false,
                ValueFlags::empty(),
                scope,
            )?;
        }
        if state.object.section(section_index)?.is_executable() {
            load_macho_unwind_metadata_for_symbol::<A>(
                state,
                common,
                symbol_index,
                resources,
                queue,
                scope,
            )?;
        }
    }

    Ok(true)
}

fn macho_unwind_atom_gc_enabled<'data>(
    state: &crate::layout::ObjectLayoutState<'data, MachO>,
    args: &MachOArgs,
) -> bool {
    args.dead_strip && state.object.flags & macho::MH_SUBSECTIONS_VIA_SYMBOLS != 0
}

fn load_macho_unwind_metadata_for_symbol<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    symbol_index: object::SymbolIndex,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, MachO>,
    queue: &mut crate::layout::LocalWorkQueue,
    scope: &rayon::Scope<'scope>,
) -> Result {
    if !macho_unwind_atom_gc_enabled(state, resources.symbol_db.args) {
        return Ok(());
    }

    load_macho_compact_unwind_for_symbol::<A>(
        state,
        common,
        symbol_index,
        resources,
        queue,
        scope,
    )?;
    load_macho_eh_frame_for_symbol::<A>(state, common, symbol_index, resources, queue, scope)
}

fn load_macho_compact_unwind_for_symbol<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    symbol_index: object::SymbolIndex,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, MachO>,
    queue: &mut crate::layout::LocalWorkQueue,
    scope: &rayon::Scope<'scope>,
) -> Result {
    ensure_compact_unwind_lookup(state)?;
    let Some(lookup) = state
        .format_specific
        .compact_unwind_lookup
        .as_ref()
        .and_then(Option::as_ref)
    else {
        return Ok(());
    };
    let compact_section_index = lookup.section_index;
    let entries = state
        .format_specific
        .compact_unwind_lookup
        .as_ref()
        .and_then(Option::as_ref)
        .and_then(|lookup| lookup.entries_by_symbol.get(&symbol_index.0))
        .cloned()
        .unwrap_or_default();
    let relocations = state.relocations(compact_section_index)?.relocations;

    for entry in entries {
        state.format_specific.ensure_section(compact_section_index);
        if !state.format_specific.live_compact_unwind_entries[compact_section_index.0]
            .insert(entry.entry_start)
        {
            continue;
        }

        for relocation_index in entry.relocation_indices {
            let entry_rel = relocations[relocation_index];
            let entry_rel_info = entry_rel.info(LE);
            let entry_offset = usize::try_from(entry_rel_info.r_address)
                .context("Mach-O compact-unwind relocation offset overflow")?;
            let entry_field_offset = entry_offset % MACHO_COMPACT_UNWIND_ENTRY_SIZE;
            let extra_flags = if entry_rel_info.r_extern && entry_field_offset == 16 {
                ValueFlags::GOT
            } else {
                ValueFlags::empty()
            };
            process_relocation::<A>(
                state,
                common,
                &entry_rel,
                part_id::MACHO_UNWIND_INFO,
                resources,
                queue,
                true,
                extra_flags,
                scope,
            )?;
        }
    }

    Ok(())
}

fn load_macho_eh_frame_for_symbol<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    symbol_index: object::SymbolIndex,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, MachO>,
    queue: &mut crate::layout::LocalWorkQueue,
    scope: &rayon::Scope<'scope>,
) -> Result {
    ensure_eh_frame_lookup(state)?;
    let Some(lookup) = state
        .format_specific
        .eh_frame_lookup
        .as_ref()
        .and_then(Option::as_ref)
    else {
        return Ok(());
    };
    let eh_frame_section_index = lookup.section_index;
    let section_part_id = state.section_part_id(
        eh_frame_section_index,
        &resources.symbol_db.section_part_ids,
    );
    let entries = state
        .format_specific
        .eh_frame_lookup
        .as_ref()
        .and_then(Option::as_ref)
        .and_then(|lookup| lookup.entries_by_symbol.get(&symbol_index.0))
        .cloned()
        .unwrap_or_default();

    for entry in entries {
        state.format_specific.ensure_section(eh_frame_section_index);
        let fde_is_new = state.format_specific.live_eh_frame_fdes[eh_frame_section_index.0]
            .insert(entry.fde_start);
        let cie_is_new = state.format_specific.live_eh_frame_cies[eh_frame_section_index.0]
            .insert(entry.cie_start);

        if fde_is_new {
            process_macho_relocation_indices::<A>(
                state,
                common,
                eh_frame_section_index,
                section_part_id,
                &entry.fde_relocation_indices,
                resources,
                queue,
                scope,
            )?;
        }
        if cie_is_new {
            process_macho_relocation_indices::<A>(
                state,
                common,
                eh_frame_section_index,
                section_part_id,
                &entry.cie_relocation_indices,
                resources,
                queue,
                scope,
            )?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_macho_relocation_indices<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    section_index: object::SectionIndex,
    section_part_id: part_id::PartId,
    relocation_indices: &[usize],
    resources: &'scope crate::layout::GraphResources<'data, 'scope, MachO>,
    queue: &mut crate::layout::LocalWorkQueue,
    scope: &rayon::Scope<'scope>,
) -> Result {
    let relocations = state.relocations(section_index)?.relocations;
    for relocation_index in relocation_indices {
        let rel = relocations[*relocation_index];
        process_relocation::<A>(
            state,
            common,
            &rel,
            section_part_id,
            resources,
            queue,
            false,
            ValueFlags::empty(),
            scope,
        )?;
    }
    Ok(())
}

fn ensure_compact_unwind_lookup<'data>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
) -> Result {
    if state.format_specific.compact_unwind_lookup.is_some() {
        return Ok(());
    }
    let Some((compact_section_index, compact_section)) =
        state.object.section_by_name("__compact_unwind")
    else {
        state.format_specific.compact_unwind_lookup = Some(None);
        return Ok(());
    };
    let data = state.object.raw_section_data(compact_section)?;
    ensure!(
        data.len() % MACHO_COMPACT_UNWIND_ENTRY_SIZE == 0,
        "__compact_unwind size must be a multiple of {MACHO_COMPACT_UNWIND_ENTRY_SIZE}"
    );

    let relocations = state.relocations(compact_section_index)?.relocations;
    let mut relocation_indices_by_entry: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut entry_starts_by_symbol: HashMap<usize, Vec<u64>> = HashMap::new();

    for (relocation_index, rel) in relocations.iter().copied().enumerate() {
        let rel_info = rel.info(LE);
        if rel_info.r_type == macho::ARM64_RELOC_ADDEND {
            continue;
        }

        let offset = usize::try_from(rel_info.r_address)
            .context("Mach-O compact-unwind relocation offset overflow")?;
        ensure!(
            offset < data.len(),
            "Mach-O __compact_unwind relocation at invalid offset {offset:#x}"
        );
        let field_offset = offset % MACHO_COMPACT_UNWIND_ENTRY_SIZE;
        ensure!(
            matches!(field_offset, 0 | 16 | 24),
            "Unsupported Mach-O __compact_unwind relocation field offset {field_offset:#x}"
        );
        let entry_start = (offset - field_offset) as u64;
        relocation_indices_by_entry
            .entry(entry_start)
            .or_default()
            .push(relocation_index);

        if field_offset == 0 && rel_info.r_extern {
            entry_starts_by_symbol
                .entry(rel_info.r_symbolnum as usize)
                .or_default()
                .push(entry_start);
        }
    }

    let entries_by_symbol = entry_starts_by_symbol
        .into_iter()
        .map(|(symbol_index, entry_starts)| {
            let entries = entry_starts
                .into_iter()
                .map(|entry_start| CompactUnwindEntryLookup {
                    entry_start,
                    relocation_indices: relocation_indices_by_entry
                        .get(&entry_start)
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect();
            (symbol_index, entries)
        })
        .collect();

    state.format_specific.compact_unwind_lookup = Some(Some(CompactUnwindLookup {
        section_index: compact_section_index,
        entries_by_symbol,
    }));
    Ok(())
}

fn ensure_eh_frame_lookup<'data>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
) -> Result {
    if state.format_specific.eh_frame_lookup.is_some() {
        return Ok(());
    }
    let Some((eh_frame_section_index, eh_frame_section)) =
        state.object.section_by_name("__eh_frame")
    else {
        state.format_specific.eh_frame_lookup = Some(None);
        return Ok(());
    };
    let data = state.object.raw_section_data(eh_frame_section)?;

    let mut entry_bounds = Vec::new();
    let mut entry_by_start = HashMap::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let length = macho_read_u32(data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );
        let entry_end = offset
            .checked_add(4)
            .and_then(|entry| entry.checked_add(length))
            .context("Mach-O __eh_frame entry length overflow")?;
        ensure!(
            entry_end <= data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let bounds = EhFrameEntryBounds {
            start: offset,
            end: entry_end,
            cie_pointer: macho_read_u32(data, offset + 4)?,
        };
        entry_bounds.push(bounds);
        entry_by_start.insert(offset, bounds);
        offset = entry_end;
    }

    let relocations = state.relocations(eh_frame_section_index)?.relocations;
    let mut relocation_order = relocations
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(relocation_index, rel)| {
            let rel_info = rel.info(LE);
            (rel_info.r_type != macho::ARM64_RELOC_ADDEND)
                .then_some((rel_info.r_address as usize, relocation_index))
        })
        .collect::<Vec<_>>();
    relocation_order.sort_unstable_by_key(|(offset, _)| *offset);

    let mut relocation_indices_by_entry: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut relocation_cursor = 0usize;
    for bounds in &entry_bounds {
        while relocation_cursor < relocation_order.len()
            && relocation_order[relocation_cursor].0 < bounds.start
        {
            relocation_cursor += 1;
        }
        let entry_relocation_start = relocation_cursor;
        while relocation_cursor < relocation_order.len()
            && relocation_order[relocation_cursor].0 < bounds.end
        {
            relocation_cursor += 1;
        }
        relocation_indices_by_entry.insert(
            bounds.start as u64,
            relocation_order[entry_relocation_start..relocation_cursor]
                .iter()
                .map(|(_, relocation_index)| *relocation_index)
                .collect(),
        );
    }

    let mut entries_by_symbol: HashMap<usize, Vec<EhFrameEntryLookup>> = HashMap::new();
    for bounds in entry_bounds
        .iter()
        .copied()
        .filter(|bounds| bounds.cie_pointer != 0)
    {
        let pc_begin_offset = bounds.start + 8;
        let fde_relocation_indices = relocation_indices_by_entry
            .get(&(bounds.start as u64))
            .cloned()
            .unwrap_or_default();
        let symbol_indices = fde_relocation_indices
            .iter()
            .filter_map(|relocation_index| {
                let rel_info = relocations[*relocation_index].info(LE);
                (rel_info.r_address as usize == pc_begin_offset && rel_info.r_extern)
                    .then_some(rel_info.r_symbolnum as usize)
            });

        let cie_pointer_offset = bounds.start + 4;
        let cie_start = cie_pointer_offset
            .checked_sub(bounds.cie_pointer as usize)
            .with_context(|| {
                format!(
                    "Mach-O __eh_frame FDE at offset {:#x} references invalid CIE pointer {:#x}",
                    bounds.start, bounds.cie_pointer
                )
            })?;
        let cie_bounds = entry_by_start.get(&cie_start).copied().with_context(|| {
            format!(
                "Mach-O __eh_frame FDE at offset {:#x} references missing CIE at offset {cie_start:#x}",
                bounds.start
            )
        })?;
        ensure!(
            cie_bounds.cie_pointer == 0,
            "Mach-O __eh_frame FDE at offset {:#x} references non-CIE entry {cie_start:#x}",
            bounds.start
        );
        let cie_relocation_indices = relocation_indices_by_entry
            .get(&(cie_start as u64))
            .cloned()
            .unwrap_or_default();

        for symbol_index in symbol_indices {
            entries_by_symbol
                .entry(symbol_index)
                .or_default()
                .push(EhFrameEntryLookup {
                    fde_start: bounds.start as u64,
                    fde_end: bounds.end,
                    cie_start: cie_start as u64,
                    cie_end: cie_bounds.end,
                    fde_relocation_indices: fde_relocation_indices.clone(),
                    cie_relocation_indices: cie_relocation_indices.clone(),
                });
        }
    }

    state.format_specific.eh_frame_lookup = Some(Some(EhFrameLookup {
        section_index: eh_frame_section_index,
        entries_by_symbol,
    }));
    Ok(())
}

fn macho_read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .context("Mach-O 32-bit read offset overflow")?;
    let bytes = data
        .get(offset..end)
        .with_context(|| format!("Mach-O 32-bit read at offset {offset:#x} is out of bounds"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

pub(crate) fn macho_live_eh_frame_cies(
    data: &[u8],
    live_fdes: Option<&BTreeSet<u64>>,
) -> Result<BTreeSet<u64>> {
    let Some(live_fdes) = live_fdes else {
        return Ok(BTreeSet::new());
    };

    let mut entry_bounds = Vec::new();
    let mut entry_by_start = HashMap::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let length = macho_read_u32(data, offset)? as usize;
        if length == 0 {
            break;
        }
        ensure!(
            length != 0xffff_ffff,
            "Mach-O 64-bit __eh_frame lengths are not supported"
        );
        let entry_end = offset
            .checked_add(4)
            .and_then(|entry| entry.checked_add(length))
            .context("Mach-O __eh_frame entry length overflow")?;
        ensure!(
            entry_end <= data.len(),
            "Mach-O __eh_frame entry at offset {offset:#x} extends past the section"
        );

        let bounds = EhFrameEntryBounds {
            start: offset,
            end: entry_end,
            cie_pointer: macho_read_u32(data, offset + 4)?,
        };
        entry_bounds.push(bounds);
        entry_by_start.insert(offset, bounds);
        offset = entry_end;
    }

    let mut live_cies = BTreeSet::new();
    for bounds in entry_bounds
        .iter()
        .copied()
        .filter(|bounds| bounds.cie_pointer != 0 && live_fdes.contains(&(bounds.start as u64)))
    {
        let cie_pointer_offset = bounds.start + 4;
        let cie_start = cie_pointer_offset
            .checked_sub(bounds.cie_pointer as usize)
            .with_context(|| {
                format!(
                    "Mach-O __eh_frame FDE at offset {:#x} references invalid CIE pointer {:#x}",
                    bounds.start, bounds.cie_pointer
                )
            })?;
        let cie_bounds = entry_by_start.get(&cie_start).copied().with_context(|| {
            format!(
                "Mach-O __eh_frame FDE at offset {:#x} references missing CIE at offset {cie_start:#x}",
                bounds.start
            )
        })?;
        ensure!(
            cie_bounds.cie_pointer == 0,
            "Mach-O __eh_frame FDE at offset {:#x} references non-CIE entry {cie_start:#x}",
            bounds.start
        );
        live_cies.insert(cie_start as u64);
    }

    Ok(live_cies)
}

fn mark_macho_subsection_live<'data>(
    state: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    symbol_index: object::SymbolIndex,
    section_index: object::SectionIndex,
) -> Result<Option<(u64, u64, bool)>> {
    let symbol = state.object.symbol(symbol_index)?;
    let symbol_offset = state
        .object
        .symbol_offset_in_section(symbol, section_index)?;
    let section_size = state
        .object
        .section_size(state.object.section(section_index)?)?;
    let object = state.object;
    let ext = &mut state.format_specific;
    ext.ensure_section(section_index);

    let boundaries = ext.subsection_boundaries[section_index.0].get_or_insert_with(|| {
        let mut offsets = vec![0];
        offsets.extend(
            object
                .enumerate_symbols()
                .filter_map(|(candidate_index, candidate)| {
                    if candidate.n_desc.get(LE) & N_ALT_ENTRY != 0 {
                        return None;
                    }
                    let candidate_section = object
                        .symbol_section(candidate, candidate_index)
                        .ok()
                        .flatten()?;
                    if candidate_section != section_index {
                        return None;
                    }
                    object
                        .symbol_offset_in_section(candidate, candidate_section)
                        .ok()
                })
                .filter(|offset| *offset < section_size),
        );
        offsets.sort_unstable();
        offsets.dedup();
        offsets
    });

    let boundary_index = boundaries.partition_point(|boundary| *boundary <= symbol_offset);
    if boundary_index == 0 {
        return Ok(None);
    }
    let boundary_index = boundary_index - 1;
    let start = boundaries[boundary_index];
    let end = boundaries
        .get(boundary_index + 1)
        .copied()
        .unwrap_or(section_size);
    if end <= start {
        return Ok(None);
    }

    let newly_live = ext.live_subsections[section_index.0].insert(start);
    Ok(Some((start, end, newly_live)))
}

fn compact_dead_macho_subsections<'data>(
    object: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    output_sections: &crate::output_section_id::OutputSections<MachO>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    symbol_db: &crate::symbol_db::SymbolDb<'data, MachO>,
) {
    if !symbol_db.args.dead_strip || object.object.flags & macho::MH_SUBSECTIONS_VIA_SYMBOLS == 0 {
        return;
    }

    let mut symbols_by_section = vec![Vec::<(u64, bool)>::new(); object.sections.len()];
    for ((symbol_index, symbol), flags) in object
        .object
        .enumerate_symbols()
        .zip(per_symbol_flags.range(object.symbol_id_range))
    {
        let Ok(Some(section_index)) = object.object.symbol_section(symbol, symbol_index) else {
            continue;
        };
        if symbol.n_desc.get(LE) & N_ALT_ENTRY != 0 {
            continue;
        }
        let Ok(offset) = object
            .object
            .symbol_offset_in_section(symbol, section_index)
        else {
            continue;
        };
        let explicitly_live = object
            .format_specific
            .live_subsections
            .get(section_index.0)
            .is_some_and(|live_subsections| live_subsections.contains(&offset));
        let keep = explicitly_live
            || flags.get().has_resolution()
            || symbol.n_desc.get(LE) & N_NO_DEAD_STRIP != 0;
        symbols_by_section[section_index.0].push((offset, keep));
    }

    for (section_number, atoms) in symbols_by_section.iter_mut().enumerate() {
        if !matches!(
            object.sections.get(section_number),
            Some(crate::resolution::SectionSlot::Loaded(_))
        ) {
            continue;
        }
        let section_index = object::SectionIndex(section_number);
        let Ok(section_header) = object.object.section(section_index) else {
            continue;
        };
        let section_name = object.object.section_name(section_header).ok();
        if section_header.should_retain()
            || !(section_header.is_executable()
                || section_name == Some(b"__gcc_except_tab".as_slice())
                || section_name == Some(b"__const".as_slice()))
        {
            continue;
        }
        let Ok(section_size) = object.object.section_size(section_header) else {
            continue;
        };

        if atoms.is_empty() {
            continue;
        }
        atoms.sort_unstable_by_key(|(offset, _)| *offset);
        if atoms.first().is_some_and(|(offset, _)| *offset > 0) {
            atoms.insert(0, (0, false));
        }

        let mut merged_atoms = Vec::with_capacity(atoms.len());
        for &(offset, keep) in atoms.iter() {
            if let Some((last_offset, last_keep)) = merged_atoms.last_mut()
                && *last_offset == offset
            {
                *last_keep |= keep;
            } else {
                merged_atoms.push((offset, keep));
            }
        }

        let mut raw_deltas = Vec::new();
        if section_name == Some(b"__const".as_slice()) {
            let mut suffix_live_alignments = vec![1u64; merged_atoms.len() + 1];
            for index in (0..merged_atoms.len()).rev() {
                let inherited = suffix_live_alignments[index + 1];
                suffix_live_alignments[index] = if merged_atoms[index].1 {
                    let offset = merged_atoms[index].0;
                    let alignment = if offset == 0 {
                        1
                    } else {
                        1u64 << offset.trailing_zeros()
                    };
                    inherited.max(alignment)
                } else {
                    inherited
                };
            }

            let mut index = 0usize;
            while index < merged_atoms.len() {
                let (run_start, keep) = merged_atoms[index];
                if keep {
                    index += 1;
                    continue;
                }

                let mut next_live_index = index + 1;
                while next_live_index < merged_atoms.len() && !merged_atoms[next_live_index].1 {
                    next_live_index += 1;
                }
                let run_end = merged_atoms
                    .get(next_live_index)
                    .map_or(section_size, |(next_start, _)| *next_start);
                if run_end <= run_start {
                    index = next_live_index;
                    continue;
                }

                let raw_delete = run_end - run_start;
                let alignment = suffix_live_alignments[next_live_index];
                let bytes_deleted = raw_delete - (raw_delete % alignment);
                if bytes_deleted == 0 {
                    index = next_live_index;
                    continue;
                }
                let Ok(bytes_deleted_u32) = u32::try_from(bytes_deleted) else {
                    raw_deltas.clear();
                    break;
                };
                raw_deltas.push((run_start, bytes_deleted_u32));
                index = next_live_index;
            }
        } else {
            for (index, &(start, keep)) in merged_atoms.iter().enumerate() {
                let end = merged_atoms
                    .get(index + 1)
                    .map_or(section_size, |(next_start, _)| *next_start);
                if keep || end <= start {
                    continue;
                }
                let Ok(bytes_deleted) = u32::try_from(end - start) else {
                    raw_deltas.clear();
                    break;
                };
                raw_deltas.push((start, bytes_deleted));
            }
        }
        if raw_deltas.is_empty() {
            continue;
        }

        let deleted = raw_deltas
            .iter()
            .map(|(_, bytes_deleted)| u64::from(*bytes_deleted))
            .sum::<u64>();
        let part_id = object.section_part_id(section_index, &symbol_db.section_part_ids);
        let Some(crate::resolution::SectionSlot::Loaded(section)) =
            object.sections.get_mut(section_number)
        else {
            continue;
        };
        let old_capacity = section.capacity(part_id, output_sections);
        section.size = section.size.saturating_sub(deleted);
        let new_capacity = section.capacity(part_id, output_sections);
        if old_capacity > new_capacity {
            common.deallocate(part_id, old_capacity - new_capacity);
        }

        if let Some(existing) = object.section_relax_deltas_mut().get_mut(section_number) {
            existing.merge_additional(raw_deltas);
        } else {
            object
                .section_relax_deltas_mut()
                .insert_sorted(section_number, SectionRelaxDeltas::new(raw_deltas));
        }
    }
}

fn compact_macho_eh_frame<'data>(
    object: &mut crate::layout::ObjectLayoutState<'data, MachO>,
    common: &mut crate::layout::CommonGroupState<'data, MachO>,
    output_sections: &crate::output_section_id::OutputSections<MachO>,
    symbol_db: &crate::symbol_db::SymbolDb<'data, MachO>,
) {
    let Some((section_index, section_header)) = object.object.section_by_name("__eh_frame") else {
        return;
    };
    if !matches!(
        object.sections.get(section_index.0),
        Some(crate::resolution::SectionSlot::Loaded(_))
    ) {
        return;
    }

    let Ok(data) = object.object.raw_section_data(section_header) else {
        return;
    };
    let filter_live_entries = macho_unwind_atom_gc_enabled(object, symbol_db.args);
    let live_fdes = object
        .format_specific
        .live_eh_frame_fdes
        .get(section_index.0);
    let live_cies = if filter_live_entries {
        let Ok(live_cies) = macho_live_eh_frame_cies(data, live_fdes) else {
            return;
        };
        Some(live_cies)
    } else {
        None
    };

    let mut raw_deltas = Vec::new();
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= data.len() {
        let Ok(length) = macho_read_u32(data, offset) else {
            raw_deltas.clear();
            break;
        };
        let length = length as usize;
        if length == 0 {
            // Final linked images concatenate input frame streams, so an input
            // terminator must not hide FDEs from later objects.
            let Ok(bytes_deleted) = u32::try_from(data.len() - offset) else {
                raw_deltas.clear();
                break;
            };
            raw_deltas.push((offset as u64, bytes_deleted));
            break;
        }
        if length == 0xffff_ffff {
            raw_deltas.clear();
            break;
        }
        let Some(entry_end) = offset
            .checked_add(size_of::<u32>())
            .and_then(|entry| entry.checked_add(length))
        else {
            raw_deltas.clear();
            break;
        };
        if entry_end > data.len() {
            raw_deltas.clear();
            break;
        }
        let Ok(cie_pointer) = macho_read_u32(data, offset + size_of::<u32>()) else {
            raw_deltas.clear();
            break;
        };
        let keep = !filter_live_entries
            || if cie_pointer == 0 {
                live_cies
                    .as_ref()
                    .is_some_and(|entries| entries.contains(&(offset as u64)))
            } else {
                live_fdes.is_some_and(|entries| entries.contains(&(offset as u64)))
            };
        if !keep {
            let Ok(bytes_deleted) = u32::try_from(entry_end - offset) else {
                raw_deltas.clear();
                break;
            };
            raw_deltas.push((offset as u64, bytes_deleted));
        }
        offset = entry_end;
    }
    if raw_deltas.is_empty() {
        return;
    }

    let deleted = raw_deltas
        .iter()
        .map(|(_, bytes_deleted)| u64::from(*bytes_deleted))
        .sum::<u64>();
    let part_id = object.section_part_id(section_index, &symbol_db.section_part_ids);
    let Some(crate::resolution::SectionSlot::Loaded(section)) =
        object.sections.get_mut(section_index.0)
    else {
        return;
    };
    let old_capacity = section.capacity(part_id, output_sections);
    section.size = section.size.saturating_sub(deleted);
    let new_capacity = section.capacity(part_id, output_sections);
    if old_capacity > new_capacity {
        common.deallocate(part_id, old_capacity - new_capacity);
    }

    if let Some(existing) = object.section_relax_deltas_mut().get_mut(section_index.0) {
        existing.merge_additional(raw_deltas);
    } else {
        object
            .section_relax_deltas_mut()
            .insert_sorted(section_index.0, SectionRelaxDeltas::new(raw_deltas));
    }
}

fn chained_fixup_table_allocation_size(
    common: &crate::layout::CommonGroupState<MachO>,
    symbol_db: &crate::symbol_db::SymbolDb<MachO>,
) -> u64 {
    let total_mem_size = common.total_mem_size().max(64 << 20);
    let page_count = total_mem_size.div_ceil(MACHO_PAGE_SIZE).max(1);
    let starts_in_image_size = (size_of::<u32>() * (DEFAULT_SEGMENT_COUNT + 1)) as u64;
    let starts_in_segment_size = size_of::<u32>() as u64
        + size_of::<u16>() as u64
        + size_of::<u16>() as u64
        + size_of::<u64>() as u64
        + size_of::<u32>() as u64
        + size_of::<u16>() as u64
        + size_of::<u16>() as u64 * page_count;

    let mut symbol_strings_size = 1;
    let mut import_count = 0u64;
    for group in &symbol_db.groups {
        for symbol_id in group.symbol_id_range() {
            if !symbol_db.is_undefined(symbol_db.definition(symbol_id)) {
                continue;
            }
            import_count += 1;
            if let Ok(name) = symbol_db.symbol_name(symbol_id) {
                symbol_strings_size += name.bytes().len() as u64 + 1;
            }
        }
    }

    (CHAINED_FIXUP_TABLE_HEADER_SIZE
        + starts_in_image_size.next_multiple_of(8)
        + starts_in_segment_size.next_multiple_of(8)
        + import_count * size_of::<u32>() as u64
        + symbol_strings_size)
        .next_multiple_of(8)
}

const SECTION_DEFINITIONS: [BuiltInSectionDetails; NUM_BUILT_IN_SECTIONS] = {
    let mut defs: [BuiltInSectionDetails; NUM_BUILT_IN_SECTIONS] =
        [DEFAULT_DEFS; NUM_BUILT_IN_SECTIONS];

    defs[output_section_id::FILE_HEADER.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"FILE_HEADER")),
        target_segment_type: Some(SegmentType::Text),
        ..DEFAULT_DEFS
    };
    // Load commands
    defs[output_section_id::PAGEZERO_SEGMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(SEG_PAGEZERO.as_bytes())),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::TEXT_SEGMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(SEG_TEXT.as_bytes())),
        target_segment_type: Some(SegmentType::LoadCommands),
        section_flags: SectionFlags::from_u32(macho::VM_PROT_READ | macho::VM_PROT_EXECUTE),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::DATA_SEGMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(SEG_DATA.as_bytes())),
        target_segment_type: Some(SegmentType::LoadCommands),
        section_flags: SectionFlags::from_u32(macho::VM_PROT_READ | macho::VM_PROT_WRITE),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::LINK_EDIT_SEGMENT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(SEG_LINKEDIT.as_bytes())),
        target_segment_type: Some(SegmentType::LoadCommands),
        section_flags: SectionFlags::from_u32(macho::VM_PROT_READ),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::ENTRY_POINT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_MAIN")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::BUILD_VERSION.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_BUILD_VERSION")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::UUID_COMMAND.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_UUID")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::LIBSYSTEM.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_LOAD_DYLIB")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::ID_DYLIB.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_ID_DYLIB")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::INTERP.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_LOAD_DYLINKER")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::DYLD_CHAINED_FIXUPS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_DYLD_CHAINED_FIXUPS")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::SYMTAB_COMMAND.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_SYMTAB")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CODE_SIGNATURE_COMMAND.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"LC_CODE_SIGNATURE")),
        target_segment_type: Some(SegmentType::LoadCommands),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CHAINED_FIXUP_TABLE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"DYLD_CHAINED_FIXUPS_TABLE")),
        target_segment_type: Some(SegmentType::LinkeditSections),
        min_alignment: alignment::GOT_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::DYNSYM.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"DYNSYM")),
        target_segment_type: Some(SegmentType::LinkeditSections),
        min_alignment: alignment::SYMTAB_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::SYMTAB_GLOBAL.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"SYMTAB")),
        target_segment_type: Some(SegmentType::LinkeditSections),
        min_alignment: alignment::SYMTAB_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::STRTAB.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"STRTAB")),
        target_segment_type: Some(SegmentType::LinkeditSections),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CODE_SIGNATURE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"CODE_SIGNATURE")),
        target_segment_type: Some(SegmentType::LinkeditSections),
        min_alignment: Alignment {
            exponent: CS_SECTION_ALIGNMENT_EXP,
        },
        ..DEFAULT_DEFS
    };
    // Multi-part generated sections
    // Start of regular sections
    defs[output_section_id::TEXT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__text")),
        section_flags: SectionFlags::from_u32(
            macho::S_REGULAR | macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS,
        ),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::PLT_GOT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__stubs")),
        section_flags: SectionFlags::from_u32(
            macho::S_SYMBOL_STUBS
                | macho::S_ATTR_PURE_INSTRUCTIONS
                | macho::S_ATTR_SOME_INSTRUCTIONS,
        ),
        min_alignment: Alignment { exponent: 2 },
        ..DEFAULT_DEFS
    };
    defs[output_section_id::GCC_EXCEPT_TABLE.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__gcc_except_tab")),
        section_flags: SectionFlags::from_u32(macho::S_REGULAR),
        min_alignment: Alignment { exponent: 2 },
        ..DEFAULT_DEFS
    };
    defs[output_section_id::MACHO_UNWIND_INFO.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__unwind_info")),
        section_flags: SectionFlags::from_u32(macho::S_REGULAR),
        min_alignment: Alignment { exponent: 2 },
        ..DEFAULT_DEFS
    };
    defs[output_section_id::EH_FRAME.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__eh_frame")),
        section_flags: SectionFlags::from_u32(
            macho::S_COALESCED
                | macho::S_ATTR_NO_TOC
                | macho::S_ATTR_STRIP_STATIC_SYMS
                | macho::S_ATTR_LIVE_SUPPORT,
        ),
        min_alignment: Alignment { exponent: 3 },
        ..DEFAULT_DEFS
    };
    defs[output_section_id::RODATA.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__const")),
        section_flags: SectionFlags::from_u32(macho::S_REGULAR),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::CSTRING.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__cstring")),
        section_flags: SectionFlags::from_u32(macho::S_CSTRING_LITERALS),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::RUSTC_METADATA.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b".rustc")),
        section_flags: SectionFlags::from_u32(macho::S_REGULAR),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::DATA.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__data")),
        section_flags: SectionFlags::from_u32(macho::S_REGULAR),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::MACHO_MOD_INIT_FUNC.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__mod_init_func")),
        section_flags: SectionFlags::from_u32(macho::S_MOD_INIT_FUNC_POINTERS),
        min_alignment: alignment::GOT_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::MACHO_THREAD_VARS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__thread_vars")),
        section_flags: SectionFlags::from_u32(macho::S_THREAD_LOCAL_VARIABLES),
        min_alignment: alignment::GOT_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::MACHO_THREAD_PTRS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__thread_ptrs")),
        section_flags: SectionFlags::from_u32(macho::S_THREAD_LOCAL_VARIABLE_POINTERS),
        min_alignment: alignment::GOT_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::TDATA.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__thread_data")),
        section_flags: SectionFlags::from_u32(macho::S_THREAD_LOCAL_REGULAR),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::TBSS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__thread_bss")),
        section_flags: SectionFlags::from_u32(macho::S_THREAD_LOCAL_ZEROFILL),
        ..DEFAULT_DEFS
    };
    defs[output_section_id::GOT.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__got")),
        section_flags: SectionFlags::from_u32(macho::S_NON_LAZY_SYMBOL_POINTERS),
        min_alignment: alignment::GOT_ENTRY,
        ..DEFAULT_DEFS
    };
    defs[output_section_id::BSS.as_usize()] = BuiltInSectionDetails {
        kind: SectionKind::Primary(SectionName(b"__bss")),
        section_flags: SectionFlags::from_u32(macho::S_ZEROFILL),
        ..DEFAULT_DEFS
    };

    defs
};

// TODO: sort properly
const DEFAULT_SECTION_RULES: &[SectionRule<'static>] = &[
    SectionRule::exact_section_keep(b"__eh_frame", crate::output_section_id::EH_FRAME),
    SectionRule::exact(b"__compact_unwind", SectionRuleOutcome::EhFrame),
    SectionRule::exact_section_keep(b".rustc", crate::output_section_id::RUSTC_METADATA),
    SectionRule::exact_section_keep(
        b"__mod_init_func",
        crate::output_section_id::MACHO_MOD_INIT_FUNC,
    ),
    SectionRule::exact_section_keep(
        b"__thread_vars",
        crate::output_section_id::MACHO_THREAD_VARS,
    ),
    SectionRule::exact_section_keep(
        b"__thread_ptrs",
        crate::output_section_id::MACHO_THREAD_PTRS,
    ),
    SectionRule::exact_section_keep(b"__thread_data", crate::output_section_id::TDATA),
    SectionRule::exact_section_keep(b"__thread_bss", crate::output_section_id::TBSS),
    // SectionRule::exact_section_keep(b"__compact_unwind", crate::output_section_id::EH_FRAME),
];

const PROGRAM_SEGMENT_DEFS: &[ProgramSegmentDef] = &[
    ProgramSegmentDef {
        segment_type: SegmentType::Text,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::LoadCommands,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::TextSections,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::DataSections,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::DataConstSections,
    },
    ProgramSegmentDef {
        segment_type: SegmentType::LinkeditSections,
    },
];

fn has_active_segment(header_info: &crate::layout::HeaderInfo, segment_type: SegmentType) -> bool {
    header_info.active_segment_ids.iter().any(|id| {
        PROGRAM_SEGMENT_DEFS
            .get(id.as_usize())
            .is_some_and(|def| def.segment_type == segment_type)
    })
}

fn count_sections_for_segment_type(
    output_sections: &crate::output_section_id::OutputSections<MachO>,
    segment_type: SegmentType,
) -> usize {
    let segment_def = ProgramSegmentDef { segment_type };
    output_sections
        .ids_with_info()
        .filter(|(section_id, _)| {
            output_sections.will_emit_section(*section_id)
                && output_sections.should_include_in_segment(*section_id, segment_def)
        })
        .count()
}

pub(crate) struct SegmentSectionsInfo<'data> {
    pub(crate) segment_size: OutputRecordLayout,
    pub(crate) segment_sections:
        Vec<(OutputRecordLayout, Option<SectionName<'data>>, SectionFlags)>,
}

pub(crate) fn get_segment_sections<'data>(
    layout: &Layout<'data, MachO>,
    segment_type: SegmentType,
) -> Option<SegmentSectionsInfo<'data>> {
    let mut in_matching_segment = false;
    let mut sections = Vec::new();
    let mut segment_id = None;

    for event in &layout.output_order {
        match event {
            OrderEvent::SegmentStart(seg_id)
                if layout.program_segments.segment_def(seg_id).segment_type == segment_type =>
            {
                segment_id = Some(seg_id);
                in_matching_segment = true;
            }
            OrderEvent::SegmentEnd(seg_id)
                if layout.program_segments.segment_def(seg_id).segment_type == segment_type
                    && in_matching_segment =>
            {
                break;
            }
            OrderEvent::Section(section_id) if in_matching_segment => {
                if matches!(
                    segment_type,
                    SegmentType::TextSections
                        | SegmentType::DataSections
                        | SegmentType::DataConstSections
                ) && !layout.output_sections.will_emit_section(section_id)
                {
                    continue;
                }
                let sizes = *layout.section_layouts.get(section_id);
                sections.push((
                    sizes,
                    layout.output_sections.name(section_id),
                    layout.output_sections.section_flags(section_id),
                ));
            }
            _ => {}
        }
    }

    let segment_id = segment_id.expect("must be visited in the output order");
    let segment_size = layout
        .segment_layouts
        .segments
        .iter()
        .find(|seg| seg.id == segment_id)
        .map(|seg| seg.sizes);

    segment_size.map(|segment_size| SegmentSectionsInfo {
        segment_sections: sections,
        segment_size,
    })
}

#[inline(always)]
fn process_relocation<'data, 'scope, A: platform::Arch<Platform = MachO>>(
    object: &mut layout::ObjectLayoutState<'data, MachO>,
    common: &mut layout::CommonGroupState<'data, MachO>,
    rel: &Relocation,
    section_part_id: part_id::PartId,
    resources: &'scope layout::GraphResources<'data, '_, MachO>,
    queue: &mut layout::LocalWorkQueue,
    is_debug_section: bool,
    extra_flags: ValueFlags,
    scope: &rayon::Scope<'scope>,
) -> Result {
    let rel_info = rel.info(LE);
    if rel_info.r_type == object::macho::ARM64_RELOC_ADDEND {
        return Ok(());
    }
    if rel_info.r_type == object::macho::ARM64_RELOC_SUBTRACTOR {
        if rel_info.r_extern {
            let local_sym_index = SymbolIndex(rel_info.r_symbolnum as usize);
            let sym = object.object.symbol(local_sym_index)?;
            if let Some(section_index) = object.object.symbol_section(sym, local_sym_index)? {
                queue.send_section_request::<A>(object.file_id, section_index, resources, scope);
            }
        } else if rel_info.r_symbolnum > 0 {
            // Non-extern Mach-O relocations use a one-based section ordinal.
            let section_index = rel_info.r_symbolnum as usize - 1;
            ensure!(
                section_index < object.sections.len(),
                "Mach-O relocation references invalid section ordinal {} in {}",
                rel_info.r_symbolnum,
                object.input,
            );
            queue.send_section_request::<A>(
                object.file_id,
                object::SectionIndex(section_index),
                resources,
                scope,
            );
        }
        return Ok(());
    }
    if rel_info.r_extern {
        let local_sym_index = SymbolIndex(rel_info.r_symbolnum as usize);
        let symbol_db = resources.symbol_db;
        let local_symbol_id = object.symbol_id_range.input_to_id(local_sym_index);
        let symbol_id = symbol_db.definition(local_symbol_id);
        let local_symbol = object.object.symbol(local_sym_index)?;
        if let Some(section_index) = object
            .object
            .symbol_section(local_symbol, local_sym_index)?
        {
            let keeps_same_object_subsection =
                local_symbol.is_local() || local_symbol.is_hidden() || symbol_id == local_symbol_id;
            if keeps_same_object_subsection
                && macho_subsection_gc_enabled(object, section_index, resources.symbol_db.args)?
            {
                load_macho_subsection_symbol::<A>(
                    object,
                    common,
                    local_sym_index,
                    section_index,
                    resources,
                    queue,
                    scope,
                )?;
            } else if local_symbol.is_local() || local_symbol.is_hidden() {
                // Local/private-extern Mach-O references may need this object's
                // section address even when the canonical symbol request resolves
                // elsewhere. Keep the local section materialised so writer-time
                // relocation fallback has an address to use.
                queue.send_section_request::<A>(object.file_id, section_index, resources, scope);
            }
        }
        let mut flags = resources.local_flags_for_symbol(symbol_id);
        flags.merge(resources.local_flags_for_symbol(local_symbol_id));
        let rel_offset = rel_info.r_address;

        let raw_relocation_type = rel.info(LE).r_type;
        let rel_info = A::relocation_from_raw(rel_info)?;
        let mut flags_to_add = layout::resolution_flags(rel_info.kind) | extra_flags;
        if should_relax_got_load_to_direct(
            symbol_db.output_kind,
            raw_relocation_type,
            symbol_db.is_undefined(symbol_id),
        ) {
            flags_to_add.remove(ValueFlags::GOT);
            flags_to_add |= ValueFlags::DIRECT;
        }
        if symbol_db.is_undefined(symbol_id) {
            match raw_relocation_type {
                macho::ARM64_RELOC_BRANCH26 => {
                    flags_to_add |= ValueFlags::PLT | ValueFlags::GOT;
                }
                macho::ARM64_RELOC_GOT_LOAD_PAGE21
                | macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12
                | macho::ARM64_RELOC_POINTER_TO_GOT
                | macho::ARM64_RELOC_TLVP_LOAD_PAGE21
                | macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
                    flags_to_add |= ValueFlags::GOT;
                }
                _ => {}
            }
        }

        let atomic_flags = &resources.per_symbol_flags.get_atomic(symbol_id);
        let previous_flags = atomic_flags.fetch_or(flags_to_add);

        if !previous_flags.has_resolution() {
            queue.send_symbol_request::<A>(symbol_id, resources, scope);
        }

        if !is_debug_section {
            crate::thunks::handle_thunk_extensions_for_relocation::<A>(
                section_part_id,
                resources,
                local_symbol_id,
                symbol_id,
                rel.info(LE),
            );
        }
    } else if rel_info.r_symbolnum > 0 {
        // Non-extern Mach-O relocations use a one-based section ordinal instead of a symbol index.
        let section_index = rel_info.r_symbolnum as usize - 1;
        ensure!(
            section_index < object.sections.len(),
            "Mach-O relocation references invalid section ordinal {} in {}",
            rel_info.r_symbolnum,
            object.input,
        );
        queue.send_section_request::<A>(
            object.file_id,
            object::SectionIndex(section_index),
            resources,
            scope,
        );
    }

    Ok(())
}
