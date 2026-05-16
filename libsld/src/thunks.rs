//! Support for range-extension thunks.
//!
//! Thunks are needed when range-limited branch instructions are used, if the target of the branch
//! is outside that range. For example, on aarch64, many branches are limited to +/- 128 MiB.
//!
//! Our support for range-extension thunks makes some assumptions in order to be as efficient as
//! possible. The main assumption is that the bulk of the executable code will be placed into a
//! single output part. We call this part the primary-part. Its ID can be obtained from
//! `ThunkConfig::primary_function_part_id`. Other executable code is tracked as non-primary parts.
//! It includes functions with higher alignment, the PLT, .init, .fini etc.
//!
//! When processing relocations, we check if a relocation is range-limited. If it is, then we handle
//! it in one of the following ways depending on whether the section containing the relocation is
//! mapped to the primary part and whether the definition symbol is contained in a section that's
//! mapped to the primary part.
//!
//! * Non-primary part references anything: The source part and target range are checked after
//!   layout. If they're too far apart, a thunk block is allocated in the source part.
//! * Primary part references anything: ValueFlags::HAS_RANGE_LIMITED_REL set for local symbol in
//!   the object that made the reference.

use crate::input_data::FileId;
use crate::layout;
use crate::layout::FileLayoutState;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::ObjectFile as _;
use crate::platform::Platform;
use crate::platform::SectionHeader as _;
use crate::program_segments::ProgramSegments;
use crate::resolution;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crossbeam_queue::SegQueue;
use itertools::Itertools as _;
use rayon::iter::IntoParallelIterator;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator as _;
use std::collections::HashMap;
use std::collections::HashSet;

/// Identifies a ThunkBlock within a Vec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ThunkBlockId(u32);

impl ThunkBlockId {
    /// The first ThunkBlock. Covers non-primary parts as well as the start of the primary part.
    pub(crate) const FIRST: ThunkBlockId = ThunkBlockId(0);

    pub(crate) fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub(crate) fn from_usize(value: usize) -> Self {
        Self(value as u32)
    }
}

pub(crate) struct ThunkBlock {
    /// Part in which thunks for this block are placed.
    pub(crate) part_id: PartId,

    /// Sorted and deduplicated SymbolIds for which we need thunks.
    pub(crate) symbols: Vec<SymbolId>,
}

struct ThunkBlockBuilder {
    part_id: PartId,
    objects: Vec<FileId>,
    symbols: Vec<SymbolId>,
}

impl ThunkBlockBuilder {
    fn new(part_id: PartId) -> Self {
        Self {
            part_id,
            objects: Vec::new(),
            symbols: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ThunkLayoutBuilder {
    /// The range beyond which we'll allocate thunks, allowing a bit of overhead for the thunks
    /// themselves.
    branch_range: u64,

    primary_function_part_id: PartId,

    /// Symbols referenced by range-limited relocations from non-primary parts, paired with the
    /// source part that needs to branch to them.
    non_primary_referenced_symbols: SegQueue<(PartId, SymbolId)>,
}

/// How much space we allow for the thunks themselves in the thunk block. Note, we don't actually
/// allocate this much space. This is used for determining whether we might need a thunk for a
/// particular reference. i.e. we subtract this from the relocation range. At some stage, we may
/// want to try and get rid of this so that we have tighter bounds on when thunks are used. In that
/// case, a good starting bound would be a count of the number of symbols in each block where we set
/// ValueFlags::HAS_RANGE_LIMITED_REL. This value is enough to link a debug build of Chromium, which
/// has slightly more than 1MB of thunks in its largest thunk block.
const MAXIMUM_THUNK_BYTES_PER_BLOCK: u64 = 2 * 1024 * 1024;

impl ThunkLayoutBuilder {
    /// Creates a thunk layout builder or returns None if thunks either aren't supported or aren't
    /// needed.
    pub(crate) fn new<A: Arch>(
        groups: &[resolution::ResolvedGroup<A::Platform>],
        output_sections: &OutputSections<A::Platform>,
        section_part_ids: &[PartId],
        args: &<A::Platform as Platform>::Args,
    ) -> Option<ThunkLayoutBuilder> {
        let config = A::thunk_config()?;

        timing_phase!("Create thunk layout builder");

        let force_conservative_thunks =
            <<A::Platform as Platform>::Args as crate::platform::Args>::has_section_start_address_overrides(args)
                || has_fixed_executable_section_addresses(groups, output_sections, section_part_ids);

        let total_executable_bytes: u64 = groups
            .iter()
            .flat_map(|group| group.files.iter())
            .filter_map(|file| {
                if let resolution::ResolvedFile::Object(obj) = file {
                    Some(obj.executable_bytes)
                } else {
                    None
                }
            })
            .sum();

        if total_executable_bytes < config.min_branch_range && !force_conservative_thunks {
            // Total text size is small enough and no executable output section has an explicit
            // address that could introduce a large gap, so we know we won't need any thunks.
            return None;
        }

        Some(ThunkLayoutBuilder {
            branch_range: config.min_branch_range - MAXIMUM_THUNK_BYTES_PER_BLOCK,
            primary_function_part_id: config.primary_function_part_id,
            non_primary_referenced_symbols: SegQueue::new(),
        })
    }

    /// Assigns thunk blocks to objects and builds the final `Vec<ThunkBlock>`.
    pub(crate) fn build<'data, P: Platform>(
        mut self,
        group_states: &mut [layout::GroupState<'data, P>],
        symbol_db: &crate::symbol_db::SymbolDb<'data, P>,
        per_symbol_flags: &crate::value_flags::PerSymbolFlags,
        output_sections: &OutputSections<P>,
        section_part_sizes: &OutputSectionPartMap<u64>,
        output_order: &OutputOrder,
        program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    ) -> Vec<ThunkBlock> {
        timing_phase!("Build thunk layout");

        let section_part_layouts = layout::layout_section_parts::<P>(
            section_part_sizes,
            output_sections,
            program_segments,
            output_order,
            symbol_db.args,
        );

        let primary_ranges = collect_primary_ranges(
            group_states,
            section_part_layouts
                .get(self.primary_function_part_id)
                .mem_offset,
        );

        let mut block_builders = assign_thunk_blocks_to_groups(
            group_states,
            &primary_ranges,
            self.branch_range,
            self.primary_function_part_id,
        );

        self.process_primary_part_refs(
            group_states,
            &primary_ranges,
            &section_part_layouts,
            symbol_db,
            per_symbol_flags,
            &mut block_builders,
        );

        self.process_non_primary_part_refs(
            group_states,
            &primary_ranges,
            &section_part_layouts,
            symbol_db,
            per_symbol_flags,
            &mut block_builders,
        );

        block_builders
            .into_par_iter()
            .map(|block| block.build())
            .collect()
    }

    fn process_primary_part_refs<'data, P: Platform>(
        &self,
        group_states: &[layout::GroupState<'data, P>],
        primary_ranges: &[Vec<Option<(u64, u64)>>],
        section_part_layouts: &OutputSectionPartMap<layout::OutputRecordLayout>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, P>,
        per_symbol_flags: &crate::value_flags::PerSymbolFlags,
        block_builders: &mut [ThunkBlockBuilder],
    ) {
        verbose_timing_phase!("Process primary part refs");

        let primary_range_for_symbol = |definition_id: SymbolId| -> Option<(u64, u64)> {
            let definition_flags = per_symbol_flags.flags_for_symbol(definition_id);

            if definition_flags.contains(ValueFlags::IFUNC)
                || definition_flags.contains(ValueFlags::DYNAMIC)
                || symbol_db.part_id_for_symbol(definition_id) != self.primary_function_part_id
            {
                return None;
            }

            let fid = symbol_db.file_id_for_symbol(definition_id);
            primary_ranges[fid.group()][fid.file()]
        };

        let part_range = |part_id: PartId| -> Option<(u64, u64)> {
            let layout = section_part_layouts.get(part_id);
            (layout.mem_size > 0)
                .then_some((layout.mem_offset, layout.mem_offset + layout.mem_size))
        };

        // Returns true if a thunk can be skipped based on known source and definition positions.
        let provably_in_range = |src_start: u64, src_end: u64, definition_id: SymbolId| -> bool {
            let definition_flags = per_symbol_flags.flags_for_symbol(definition_id);

            let target_range = if definition_flags.needs_plt() || definition_flags.is_ifunc() {
                part_range(crate::part_id::PLT_GOT)
            } else if definition_flags.contains(ValueFlags::DYNAMIC) {
                None
            } else {
                let part_id = symbol_db.part_id_for_symbol(definition_id);
                if part_id == self.primary_function_part_id {
                    primary_range_for_symbol(definition_id)
                } else {
                    part_range(part_id)
                }
            };

            if let Some((def_start, def_end)) = target_range {
                let span_start = src_start.min(def_start);
                let span_end = src_end.max(def_end);
                return span_end.saturating_sub(span_start) < self.branch_range;
            }

            false
        };

        // Collect primary-section range-limited symbols by scanning each block's objects in
        // parallel, then reducing object-local symbol sets into one set per block.
        block_builders.into_par_iter().for_each(|block| {
            let symbols = block
                .objects
                .par_iter()
                .filter_map(|file_id| {
                    let FileLayoutState::Object(obj) =
                        &group_states[file_id.group()].files[file_id.file()]
                    else {
                        return None;
                    };

                    verbose_timing_phase!("Collect object primary part thunks");

                    let mut object_symbols = HashSet::new();
                    for (i, raw_flags) in per_symbol_flags
                        .raw_range(obj.symbol_id_range)
                        .iter()
                        .enumerate()
                    {
                        if !raw_flags.get().contains(ValueFlags::HAS_RANGE_LIMITED_REL) {
                            continue;
                        }

                        let local_symbol_id = obj.symbol_id_range.offset_to_id(i);
                        let definition_id = symbol_db.definition(local_symbol_id);
                        let Some((src_start, src_end)) =
                            primary_ranges[obj.file_id.group()][obj.file_id.file()]
                        else {
                            continue;
                        };

                        if !provably_in_range(src_start, src_end, definition_id) {
                            object_symbols.insert(definition_id);
                        }
                    }
                    Some(object_symbols)
                })
                .reduce(HashSet::new, |mut a, mut b| {
                    verbose_timing_phase!("Merge thunk block symbols");

                    if b.len() > a.len() {
                        std::mem::swap(&mut a, &mut b);
                    }
                    a.extend(b);
                    a
                });

            block.symbols.extend(symbols);
        });
    }

    fn process_non_primary_part_refs<'data, P: Platform>(
        &mut self,
        group_states: &mut [layout::GroupState<'data, P>],
        primary_ranges: &[Vec<Option<(u64, u64)>>],
        section_part_layouts: &OutputSectionPartMap<layout::OutputRecordLayout>,
        symbol_db: &crate::symbol_db::SymbolDb<'data, P>,
        per_symbol_flags: &crate::value_flags::PerSymbolFlags,
        block_builders: &mut Vec<ThunkBlockBuilder>,
    ) {
        verbose_timing_phase!("Process non-primary part refs");

        let part_range = |part_id: PartId| -> Option<(u64, u64)> {
            let layout = section_part_layouts.get(part_id);
            (layout.mem_size > 0)
                .then_some((layout.mem_offset, layout.mem_offset + layout.mem_size))
        };

        let primary_range_for_symbol = |definition_id: SymbolId| -> Option<(u64, u64)> {
            let definition_flags = per_symbol_flags.flags_for_symbol(definition_id);

            if definition_flags.contains(ValueFlags::IFUNC)
                || definition_flags.contains(ValueFlags::DYNAMIC)
                || symbol_db.part_id_for_symbol(definition_id) != self.primary_function_part_id
            {
                return None;
            }

            let fid = symbol_db.file_id_for_symbol(definition_id);
            primary_ranges[fid.group()][fid.file()]
        };

        let target_range_for_symbol = |definition_id: SymbolId| -> Option<(u64, u64)> {
            let definition_flags = per_symbol_flags.flags_for_symbol(definition_id);
            if definition_flags.needs_plt() || definition_flags.is_ifunc() {
                part_range(crate::part_id::PLT_GOT)
            } else if definition_flags.contains(ValueFlags::DYNAMIC) {
                None
            } else {
                let part_id = symbol_db.part_id_for_symbol(definition_id);
                if part_id == self.primary_function_part_id {
                    primary_range_for_symbol(definition_id)
                } else {
                    part_range(part_id)
                }
            }
        };

        let needs_thunk = |source_part_id: PartId, definition_id: SymbolId| -> bool {
            let Some((src_start, src_end)) = part_range(source_part_id) else {
                return false;
            };
            let Some((def_start, def_end)) = target_range_for_symbol(definition_id) else {
                return true;
            };
            let span_start = src_start.min(def_start);
            let span_end = src_end.max(def_end);
            span_end.saturating_sub(span_start) >= self.branch_range
        };

        let mut symbols_by_part: HashMap<PartId, HashSet<SymbolId>> = HashMap::new();
        for (source_part_id, symbol_id) in core::mem::take(&mut self.non_primary_referenced_symbols)
        {
            let definition_id = symbol_db.definition(symbol_id);
            if needs_thunk(source_part_id, definition_id) {
                symbols_by_part
                    .entry(source_part_id)
                    .or_default()
                    .insert(definition_id);
            }
        }

        for (source_part_id, symbols) in symbols_by_part
            .into_iter()
            .sorted_by_key(|(source_part_id, _)| *source_part_id)
        {
            let block_id = ThunkBlockId::from_usize(block_builders.len());
            let Some(owner) =
                assign_extra_thunk_block_owner(group_states, symbol_db, source_part_id, block_id)
            else {
                continue;
            };

            block_builders.push(ThunkBlockBuilder {
                part_id: source_part_id,
                objects: vec![owner],
                symbols: symbols.into_iter().collect(),
            });
        }
    }
}

fn has_fixed_executable_section_addresses<P: Platform>(
    groups: &[resolution::ResolvedGroup<P>],
    output_sections: &OutputSections<P>,
    section_part_ids: &[PartId],
) -> bool {
    groups.iter().any(|group| {
        group.files.iter().any(|file| {
            let resolution::ResolvedFile::Object(obj) = file else {
                return false;
            };

            obj.common
                .object
                .enumerate_sections()
                .any(|(section_index, section)| {
                    if !section.is_executable() {
                        return false;
                    }

                    let input_section_id = obj.section_id_range.input_to_id(section_index);
                    let part_id = section_part_ids[input_section_id.as_usize()];
                    output_sections
                        .output_info(part_id.output_section_id())
                        .location
                        .is_some()
                })
        })
    })
}

fn collect_primary_ranges<P: Platform>(
    group_states: &[layout::GroupState<P>],
    initial_offset: u64,
) -> Vec<Vec<Option<(u64, u64)>>> {
    let mut offset = initial_offset;
    group_states
        .iter()
        .map(|group| {
            group
                .files
                .iter()
                .map(|file| {
                    if let FileLayoutState::Object(obj) = file {
                        let start = offset;
                        let end = start + obj.post_gc_primary_bytes;
                        offset = end;
                        Some((start, end))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect()
}

fn assign_extra_thunk_block_owner<P: Platform>(
    group_states: &mut [layout::GroupState<P>],
    symbol_db: &crate::symbol_db::SymbolDb<P>,
    part_id: PartId,
    block_id: ThunkBlockId,
) -> Option<FileId> {
    for group in group_states {
        for file in &mut group.files {
            let FileLayoutState::Object(obj) = file else {
                continue;
            };

            let owns_part = obj.sections.iter().enumerate().any(|(index, section)| {
                matches!(section, resolution::SectionSlot::Loaded(_))
                    && obj.section_part_id(object::SectionIndex(index), &symbol_db.section_part_ids)
                        == part_id
            });

            if owns_part {
                obj.extra_thunk_block_ids.push(block_id);
                return Some(obj.file_id);
            }
        }
    }

    None
}

pub(crate) fn block_id_for_source_part<P: Platform>(
    object_layout: &layout::ObjectLayout<'_, P>,
    thunk_block_part_ids: &[PartId],
    source_part_id: PartId,
    primary_function_part_id: PartId,
) -> ThunkBlockId {
    if source_part_id == primary_function_part_id {
        return object_layout.thunk_block_id;
    }

    object_layout
        .extra_thunk_block_ids
        .iter()
        .copied()
        .find(|block_id| {
            thunk_block_part_ids
                .get(block_id.as_usize())
                .copied()
                .is_some_and(|part_id| part_id == source_part_id)
        })
        .or_else(|| {
            thunk_block_part_ids
                .iter()
                .position(|&part_id| part_id == source_part_id)
                .map(ThunkBlockId::from_usize)
        })
        .unwrap_or(ThunkBlockId::FIRST)
}

/// Records that a thunkable relocation was encountered during the GC phase. The actual decision
/// about whether a thunk is needed is deferred to `ThunkLayoutBuilder::build()`.
pub(crate) fn handle_thunk_extensions_for_relocation<A: Arch>(
    section_part_id: PartId,
    resources: &layout::GraphResources<'_, '_, A::Platform>,
    local_symbol_id: SymbolId,
    symbol_id: SymbolId,
    rel: <A::Platform as Platform>::RelocationInfo,
) {
    if resources.thunk_layout_builder.is_some()
        && let Some(config) = A::thunk_config()
        && let Some(rel_info) = A::relocation_from_raw(rel).ok()
        && rel_info.thunkable
    {
        if section_part_id == config.primary_function_part_id {
            resources
                .per_symbol_flags
                .get_atomic(local_symbol_id)
                .or_assign(ValueFlags::HAS_RANGE_LIMITED_REL);
        } else {
            let canonical_symbol_id = resources.symbol_db.definition(symbol_id);
            resources
                .thunk_layout_builder
                .as_ref()
                .unwrap()
                .non_primary_referenced_symbols
                .push((section_part_id, canonical_symbol_id));
        }
    }
}

fn assign_thunk_blocks_to_groups<'data, P: Platform>(
    group_states: &mut [layout::GroupState<'data, P>],
    primary_ranges: &[Vec<Option<(u64, u64)>>],
    max_branch_range: u64,
    primary_function_part_id: PartId,
) -> Vec<ThunkBlockBuilder> {
    verbose_timing_phase!("Assign thunk blocks");

    let post_gc_bounds = group_states
        .iter()
        .enumerate()
        .flat_map(|(group_id, group)| {
            group
                .files
                .iter()
                .enumerate()
                .filter_map(move |(file_id, file)| match file {
                    FileLayoutState::Object(obj) => {
                        let (start, end) = primary_ranges[group_id][file_id]?;
                        (end > start).then_some((obj.file_id, start, end))
                    }
                    _ => None,
                })
        })
        .collect_vec();

    let num_blocks = assign_thunk_blocks(
        post_gc_bounds.iter().copied(),
        max_branch_range,
        |fid, bid, is_owner| {
            if let FileLayoutState::Object(obj) = &mut group_states[fid.group()].files[fid.file()] {
                obj.thunk_block_id = bid;
                obj.owns_thunk_block = is_owner;
            }
        },
    );

    let mut block_builders = (0..num_blocks.max(1))
        .map(|_| ThunkBlockBuilder::new(primary_function_part_id))
        .collect_vec();

    for group in group_states.iter() {
        for file in &group.files {
            if let FileLayoutState::Object(obj) = file
                && obj.post_gc_primary_bytes > 0
            {
                block_builders[obj.thunk_block_id.as_usize()]
                    .objects
                    .push(obj.file_id);
            }
        }
    }

    block_builders
}

/// Assigns objects to thunk blocks based on their post-GC positions.
///
/// `objects` yields `(file_id, start, end)` for each object in order of increasing address.
/// `assign` is called for every object with `(file_id, block_id, is_owner)`.
/// Returns the number of blocks created.
fn assign_thunk_blocks(
    objects: impl Iterator<Item = (FileId, u64, u64)>,
    max_branch_range: u64,
    mut assign: impl FnMut(FileId, ThunkBlockId, bool),
) -> usize {
    let mut num_blocks: usize = 0;

    let mut iter = objects;

    let Some((first_file_id, _first_start, first_end)) = iter.next() else {
        return num_blocks;
    };

    // ThunkBlock::FIRST is always owned by the first object.
    num_blocks += 1;
    assign(first_file_id, ThunkBlockId::FIRST, true);

    // We alternate between "previous" mode (pending_next==None) and "next" mode. While in previous
    // mode, we assign objects to the previous thunk block. While in next mode, we assign objects to
    // the next block, which we haven't yet decided exactly where it will go. Whenever adding a new
    // object might put something out-of-range, we switch modes.
    let mut prev_block_id = ThunkBlockId::FIRST;
    let mut prev_block_pos = first_end;
    // Tracks an unplaced "next" block: (block_id, first_file_id_using_it, first_object_start).
    let mut pending_next: Option<(ThunkBlockId, FileId, u64)> = None;

    for (file_id, start, end) in iter {
        if let Some((next_id, first_file_id, first_object_start)) = pending_next {
            if end - first_object_start >= max_branch_range {
                // Block is placed on this object: it becomes the owner and switches to "previous".
                assign(first_file_id, next_id, false);
                assign(file_id, next_id, true);
                prev_block_id = next_id;
                prev_block_pos = end;
                pending_next = None;
            } else {
                assign(file_id, next_id, false);
                pending_next = Some((next_id, first_file_id, first_object_start));
            }
        } else if end - prev_block_pos >= max_branch_range {
            let next_id = ThunkBlockId(num_blocks as u32);
            num_blocks += 1;
            pending_next = Some((next_id, file_id, start));
        } else {
            assign(file_id, prev_block_id, false);
        }
    }

    // If the loop ended with a pending next block that never needed splitting, the first object
    // using it becomes the owner (block is effectively at the start of this group).
    if let Some((next_id, first_file_id, _)) = pending_next {
        assign(first_file_id, next_id, true);
    }

    num_blocks
}

impl ThunkBlockBuilder {
    fn build(mut self) -> ThunkBlock {
        verbose_timing_phase!("Build thunk block");
        // Sorting is needed for deterministic output, since the symbols came here in hashset
        // iteration order. Deduplication has mostly already occurred, but the non-primary hasn't
        // yet been deduplicated against other thunks for the first block.
        self.symbols.sort();
        self.symbols.dedup();
        ThunkBlock {
            part_id: self.part_id,
            symbols: self.symbols,
        }
    }
}

impl std::fmt::Debug for ThunkBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ThunkBlock with {} thunks", self.symbols.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_objects(offsets_and_sizes: &[(u64, u64)]) -> Vec<(FileId, u64, u64)> {
        offsets_and_sizes
            .iter()
            .enumerate()
            .map(|(i, &(start, size))| (FileId::new(0, i as u32), start, start + size))
            .collect()
    }

    #[test]
    fn test_assign_thunk_blocks_single_cluster() {
        // 3 objects all within max_range=1000: single ThunkBlock owned by first object.
        let mut assignments: HashMap<FileId, (ThunkBlockId, bool)> = HashMap::new();
        let num_blocks = assign_thunk_blocks(
            make_objects(&[(0, 100), (100, 100), (200, 100)]).into_iter(),
            1000,
            |fid, bid, is_owner| {
                assignments.insert(fid, (bid, is_owner));
            },
        );
        assert_eq!(num_blocks, 1);
        assert_eq!(assignments[&FileId::new(0, 0)], (ThunkBlockId(0), true));
        for f in 1..3 {
            assert_eq!(assignments[&FileId::new(0, f as u32)].0, ThunkBlockId(0));
        }
    }

    #[test]
    fn test_assign_thunk_blocks_placement() {
        // 5 objects. Objects 0,1 are in range of block #0. Object 2 goes out of range,
        // so we start block #1 (tentatively assigned to 2). Object 4's end goes out of range
        // of first_object_start (object 2's offset), so block #1 is placed on object 4.
        let mut assignments: HashMap<FileId, ThunkBlockId> = HashMap::new();
        let mut owners: HashMap<ThunkBlockId, FileId> = HashMap::new();
        let num_blocks = assign_thunk_blocks(
            make_objects(&[(0, 100), (300, 100), (600, 100), (900, 100), (1200, 100)]).into_iter(),
            500,
            |fid, bid, is_owner| {
                assignments.insert(fid, bid);
                if is_owner {
                    owners.insert(bid, fid);
                }
            },
        );
        assert_eq!(num_blocks, 2);
        assert_eq!(owners[&ThunkBlockId(0)], FileId::new(0, 0));
        assert_eq!(owners[&ThunkBlockId(1)], FileId::new(0, 4));
        assert_eq!(assignments[&FileId::new(0, 0)], ThunkBlockId(0));
        assert_eq!(assignments[&FileId::new(0, 1)], ThunkBlockId(0));
        assert_eq!(assignments[&FileId::new(0, 2)], ThunkBlockId(1));
        assert_eq!(assignments[&FileId::new(0, 3)], ThunkBlockId(1));
        assert_eq!(assignments[&FileId::new(0, 4)], ThunkBlockId(1));
    }
}
