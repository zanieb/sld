// TODO
#![allow(unused_variables)]

use crate::alignment::Alignment;
use crate::bail;
use crate::macho::MachO;
use crate::output_section_id;
use crate::platform::ThunkConfig;
use linker_utils::elf::AArch64Instruction;
use linker_utils::elf::AllowedRange;
use linker_utils::elf::PAGE_MASK_4KB;
use linker_utils::elf::PageMask;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::RelocationKindInfo;
use linker_utils::elf::RelocationSize;
use linker_utils::elf::SIZE_4KB;
use linker_utils::elf::Sign;

pub(crate) struct MachOAArch64;

const THUNK_TEMPLATE: &[u8] = &[
    0x10, 0x00, 0x00, 0x90, // adrp x16, 0
    0x10, 0x02, 0x00, 0x91, // add  x16, x16, #0
    0x00, 0x02, 0x1f, 0xd6, // br   x16
];

const MIN_BRANCH_RANGE: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Relaxation {}

impl crate::platform::Relaxation for Relaxation {
    fn apply(&self, section_bytes: &mut [u8], offset_in_section: &mut u64, addend: &mut i64) {
        todo!()
    }

    fn rel_info(&self) -> linker_utils::elf::RelocationKindInfo {
        todo!()
    }

    fn debug_kind(&self) -> impl std::fmt::Debug {
        todo!()
    }

    fn next_modifier(&self) -> linker_utils::relaxation::RelocationModifier {
        todo!()
    }

    fn is_mandatory(&self) -> bool {
        todo!()
    }
}

impl crate::platform::Arch for MachOAArch64 {
    type Relaxation = Relaxation;

    type Platform = MachO;

    fn arch_identifier() -> <Self::Platform as crate::platform::Platform>::ArchIdentifier {
        todo!()
    }

    fn get_dynamic_relocation_type(relocation: linker_utils::elf::DynamicRelocationKind) -> u32 {
        todo!()
    }

    fn write_plt_entry(
        plt_entry: &mut [u8],
        got_address: u64,
        plt_address: u64,
    ) -> crate::error::Result {
        todo!()
    }

    fn relocation_from_raw(
        rel: object::macho::RelocationInfo,
    ) -> crate::error::Result<RelocationKindInfo> {
        let rel_size_in_bytes = 1 << rel.r_length;
        let rel_size = RelocationSize::ByteSize(rel_size_in_bytes);
        let rel_kind = if rel.r_pcrel {
            RelocationKind::Relative
        } else {
            RelocationKind::Absolute
        };

        let (kind, size, mask, range, alignment) = match rel.r_type {
            object::macho::ARM64_RELOC_UNSIGNED => {
                (rel_kind, rel_size, None, AllowedRange::no_check(), 1)
            }
            object::macho::ARM64_RELOC_BRANCH26 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    rel_kind,
                    RelocationSize::bit_mask_aarch64(2, 28, AArch64Instruction::JumpCall),
                    None,
                    AllowedRange::from_bit_size(28, Sign::Signed),
                    4,
                )
            }
            object::macho::ARM64_RELOC_GOT_LOAD_PAGE21 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    RelocationKind::GotRelative,
                    RelocationSize::bit_mask_aarch64(12, 33, AArch64Instruction::Adr),
                    Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB)),
                    AllowedRange::from_bit_size(33, Sign::Signed),
                    1,
                )
            }
            object::macho::ARM64_RELOC_TLVP_LOAD_PAGE21 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    RelocationKind::Relative,
                    RelocationSize::bit_mask_aarch64(12, 33, AArch64Instruction::Adr),
                    Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB)),
                    AllowedRange::from_bit_size(33, Sign::Signed),
                    1,
                )
            }
            object::macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12
            | object::macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    if rel.r_type == object::macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 {
                        RelocationKind::AbsoluteLowPart
                    } else {
                        RelocationKind::Got
                    },
                    RelocationSize::bit_mask_aarch64(0, 12, AArch64Instruction::MachOLow12),
                    None,
                    AllowedRange::no_check(),
                    1,
                )
            }
            object::macho::ARM64_RELOC_POINTER_TO_GOT => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    RelocationKind::GotRelative,
                    rel_size,
                    None,
                    AllowedRange::from_bit_size(32, Sign::Signed),
                    1,
                )
            }
            object::macho::ARM64_RELOC_PAGE21 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    rel_kind,
                    RelocationSize::bit_mask_aarch64(12, 33, AArch64Instruction::Adr),
                    Some(PageMask::SymbolPlusAddendAndPosition(PAGE_MASK_4KB)),
                    AllowedRange::from_bit_size(33, Sign::Signed),
                    1,
                )
            }
            object::macho::ARM64_RELOC_PAGEOFF12 => {
                debug_assert_eq!(rel_size, RelocationSize::ByteSize(4));
                (
                    RelocationKind::AbsoluteLowPart,
                    RelocationSize::bit_mask_aarch64(0, 12, AArch64Instruction::MachOLow12),
                    None,
                    AllowedRange::no_check(),
                    1,
                )
            }
            _ => bail!("Unknown relocation: {}", rel.r_type),
        };
        Ok(RelocationKindInfo {
            alignment,
            bias: 0,
            kind,
            mask,
            range,
            size,
            thunkable: rel.r_type == object::macho::ARM64_RELOC_BRANCH26,
        })
    }

    fn rel_type_to_string(r_type: u32) -> std::borrow::Cow<'static, str> {
        todo!()
    }

    fn tp_offset_start(layout: &crate::layout::Layout<Self::Platform>) -> u64 {
        todo!()
    }

    fn get_property_class(property_type: u32) -> Option<crate::elf::PropertyClass> {
        todo!()
    }

    fn merge_eflags(eflags: impl Iterator<Item = u32>) -> crate::error::Result<u32> {
        todo!()
    }

    fn high_part_relocations() -> &'static [u32] {
        todo!()
    }

    fn thunk_config() -> Option<ThunkConfig> {
        Some(ThunkConfig {
            primary_function_part_id: const {
                output_section_id::TEXT.part_id_with_alignment(Alignment { exponent: 2 })
            },
            min_branch_range: MIN_BRANCH_RANGE,
            thunk_size: THUNK_TEMPLATE.len() as u64,
        })
    }

    fn write_thunk(thunk_address: u64, target_address: u64, buf: &mut [u8]) {
        buf.copy_from_slice(THUNK_TEMPLATE);

        let thunk_page = thunk_address & !PAGE_MASK_4KB;
        let target_page = target_address & !PAGE_MASK_4KB;
        let page_diff = (target_page as i64).wrapping_sub(thunk_page as i64);
        let page_count = (page_diff / SIZE_4KB as i64) as u64 & 0x1f_ffff;
        AArch64Instruction::Adr.write_to_value(page_count, false, &mut buf[0..4]);
        AArch64Instruction::Add.write_to_value(
            target_address & PAGE_MASK_4KB,
            false,
            &mut buf[4..8],
        );
    }

    fn get_source_info<'data>(
        object: &<Self::Platform as crate::platform::Platform>::File<'data>,
        relocations: &<Self::Platform as crate::platform::Platform>::RelocationSections,
        section: &<Self::Platform as crate::platform::Platform>::SectionHeader,
        offset_in_section: u64,
    ) -> crate::error::Result<crate::platform::SourceInfo> {
        todo!()
    }

    fn new_relaxation(
        relocation_kind: u32,
        section_bytes: &[u8],
        offset_in_section: u64,
        flags: crate::value_flags::ValueFlags,
        output_kind: crate::output_kind::OutputKind,
        section_flags: <Self::Platform as crate::platform::Platform>::SectionFlags,
        non_zero_address: bool,
        relax_deltas: Option<&linker_utils::relaxation::SectionRelaxDeltas>,
    ) -> Option<Self::Relaxation> {
        todo!()
    }
}
