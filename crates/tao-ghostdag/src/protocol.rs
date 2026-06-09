// Ported from rusty-kaspa, ISC, consensus/src/processes/ghostdag/{protocol,
// ordering,mergeset}.rs and model/stores/ghostdag.rs (GhostdagData). Adapted:
// BlueWork = u128 + WorkStore (instead of header bits/calc_work), Arc instead of
// kaspa_utils Refs, crate-local module paths and types, no let-chains.

use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use tao_reachability::{blockhash, BlockHashMap, BlockHashes, Hash};

use crate::{
    is_origin, BlueWork, GhostdagStoreReader, HashKTypeMap, KType, ReachabilityService,
    RelationsStoreReader, WorkStore,
};

/// A block sortable by (blue_work, hash) — the selected-parent / mergeset order.
#[derive(Eq, Clone)]
pub struct SortableBlock {
    pub hash: Hash,
    pub blue_work: BlueWork,
}

impl PartialEq for SortableBlock {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}
impl PartialOrd for SortableBlock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SortableBlock {
    fn cmp(&self, other: &Self) -> Ordering {
        self.blue_work.cmp(&other.blue_work).then_with(|| self.hash.cmp(&other.hash))
    }
}

/// Per-block GHOSTDAG data.
#[derive(Clone)]
pub struct GhostdagData {
    pub blue_score: u64,
    pub blue_work: BlueWork,
    pub selected_parent: Hash,
    pub mergeset_blues: BlockHashes,
    pub mergeset_reds: BlockHashes,
    pub blues_anticone_sizes: HashKTypeMap,
}

impl GhostdagData {
    pub fn new(
        blue_score: u64,
        blue_work: BlueWork,
        selected_parent: Hash,
        mergeset_blues: BlockHashes,
        mergeset_reds: BlockHashes,
        blues_anticone_sizes: HashKTypeMap,
    ) -> Self {
        Self { blue_score, blue_work, selected_parent, mergeset_blues, mergeset_reds, blues_anticone_sizes }
    }

    pub fn new_with_selected_parent(selected_parent: Hash, k: KType) -> Self {
        let mut mergeset_blues: Vec<Hash> = Vec::with_capacity((k + 1) as usize);
        let mut blues_anticone_sizes: BlockHashMap<KType> = BlockHashMap::with_capacity(k as usize);
        mergeset_blues.push(selected_parent);
        blues_anticone_sizes.insert(selected_parent, 0);
        Self {
            blue_score: 0,
            blue_work: 0,
            selected_parent,
            mergeset_blues: Arc::new(mergeset_blues),
            mergeset_reds: Arc::new(Vec::new()),
            blues_anticone_sizes: Arc::new(blues_anticone_sizes),
        }
    }

    /// Mergeset (excluding the selected parent), unordered.
    pub fn unordered_mergeset_without_selected_parent(&self) -> impl Iterator<Item = Hash> + '_ {
        self.mergeset_blues.iter().skip(1).cloned().chain(self.mergeset_reds.iter().cloned())
    }

    fn add_blue(&mut self, block: Hash, blue_anticone_size: KType, block_blues_anticone_sizes: &BlockHashMap<KType>) {
        Arc::make_mut(&mut self.mergeset_blues).push(block);
        let blues_anticone_sizes = Arc::make_mut(&mut self.blues_anticone_sizes);
        blues_anticone_sizes.insert(block, blue_anticone_size);
        for (blue, size) in block_blues_anticone_sizes {
            blues_anticone_sizes.insert(*blue, size + 1);
        }
    }

    fn add_red(&mut self, block: Hash) {
        Arc::make_mut(&mut self.mergeset_reds).push(block);
    }

    fn finalize_score_and_work(&mut self, blue_score: u64, blue_work: BlueWork) {
        self.blue_score = blue_score;
        self.blue_work = blue_work;
    }
}

/// The GHOSTDAG protocol manager (generic over the stores + reachability service).
#[derive(Clone)]
pub struct GhostdagManager<T: GhostdagStoreReader, S: RelationsStoreReader, U: ReachabilityService, W: WorkStore> {
    genesis_hash: Hash,
    k: KType,
    ghostdag_store: Arc<T>,
    relations_store: S,
    reachability_service: U,
    work_store: W,
}

impl<T: GhostdagStoreReader, S: RelationsStoreReader, U: ReachabilityService, W: WorkStore>
    GhostdagManager<T, S, U, W>
{
    pub fn new(
        genesis_hash: Hash,
        k: KType,
        ghostdag_store: Arc<T>,
        relations_store: S,
        reachability_service: U,
        work_store: W,
    ) -> Self {
        Self { genesis_hash, k, ghostdag_store, relations_store, reachability_service, work_store }
    }

    /// GHOSTDAG data for the ORIGIN sentinel (parent of genesis).
    pub fn origin_ghostdag_data(&self) -> Arc<GhostdagData> {
        Arc::new(GhostdagData::new(
            0,
            0,
            blockhash::NONE,
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            Arc::new(BlockHashMap::new()),
        ))
    }

    pub fn find_selected_parent(&self, parents: impl IntoIterator<Item = Hash>) -> Hash {
        parents
            .into_iter()
            .map(|parent| SortableBlock { hash: parent, blue_work: self.ghostdag_store.get_blue_work(parent).unwrap() })
            .max()
            .unwrap()
            .hash
    }

    /// Run GHOSTDAG for a block with the given `parents`, producing its data.
    pub fn ghostdag(&self, parents: &[Hash]) -> GhostdagData {
        assert!(!parents.is_empty(), "genesis must be added via init");

        let selected_parent = self.find_selected_parent(parents.iter().copied());
        if is_origin(selected_parent) {
            // ORIGIN is always a single parent; blue score/work remain zero.
            return GhostdagData::new_with_selected_parent(selected_parent, 1);
        }
        let k = self.k;
        let mut new_block_data = GhostdagData::new_with_selected_parent(selected_parent, k);
        let ordered_mergeset = self.ordered_mergeset_without_selected_parent(selected_parent, parents);

        for blue_candidate in ordered_mergeset.iter().cloned() {
            match self.check_blue_candidate(&new_block_data, blue_candidate, k) {
                ColoringOutput::Blue(blue_anticone_size, blues_anticone_sizes) => {
                    new_block_data.add_blue(blue_candidate, blue_anticone_size, &blues_anticone_sizes);
                }
                ColoringOutput::Red => new_block_data.add_red(blue_candidate),
            }
        }

        let blue_score =
            self.ghostdag_store.get_blue_score(selected_parent).unwrap() + new_block_data.mergeset_blues.len() as u64;
        let added_blue_work: BlueWork =
            new_block_data.mergeset_blues.iter().cloned().map(|h| self.work_store.get_work(h)).sum();
        let blue_work = self.ghostdag_store.get_blue_work(selected_parent).unwrap() + added_blue_work;

        new_block_data.finalize_score_and_work(blue_score, blue_work);
        new_block_data
    }

    fn ordered_mergeset_without_selected_parent(&self, selected_parent: Hash, parents: &[Hash]) -> Vec<Hash> {
        self.sort_blocks(self.unordered_mergeset_without_selected_parent(selected_parent, parents))
    }

    fn unordered_mergeset_without_selected_parent(&self, selected_parent: Hash, parents: &[Hash]) -> Vec<Hash> {
        let mut queue: VecDeque<Hash> = parents.iter().copied().filter(|p| *p != selected_parent).collect();
        let mut mergeset: HashSet<Hash> = queue.iter().copied().collect();
        let mut past: HashSet<Hash> = HashSet::new();

        while let Some(current) = queue.pop_front() {
            let current_parents = self.relations_store.get_parents(current).unwrap();
            for parent in current_parents.iter() {
                if mergeset.contains(parent) || past.contains(parent) {
                    continue;
                }
                if self.reachability_service.is_dag_ancestor_of(*parent, selected_parent) {
                    past.insert(*parent);
                    continue;
                }
                mergeset.insert(*parent);
                queue.push_back(*parent);
            }
        }
        mergeset.into_iter().collect()
    }

    fn sort_blocks(&self, blocks: impl IntoIterator<Item = Hash>) -> Vec<Hash> {
        let mut sorted: Vec<Hash> = blocks.into_iter().collect();
        sorted.sort_by_cached_key(|block| SortableBlock {
            hash: *block,
            blue_work: self.ghostdag_store.get_blue_work(*block).unwrap(),
        });
        sorted
    }

    fn check_blue_candidate_with_chain_block(
        &self,
        new_block_data: &GhostdagData,
        chain_block: &ChainBlock,
        blue_candidate: Hash,
        candidate_blues_anticone_sizes: &mut BlockHashMap<KType>,
        candidate_blue_anticone_size: &mut KType,
        k: KType,
    ) -> ColoringState {
        // If blue_candidate is in the future of chain_block, all remaining blues
        // are in its past — mark blue.
        if let Some(hash) = chain_block.hash {
            if self.reachability_service.is_dag_ancestor_of(hash, blue_candidate) {
                return ColoringState::Blue;
            }
        }

        for &peer in chain_block.data.mergeset_blues.iter() {
            if self.reachability_service.is_dag_ancestor_of(peer, blue_candidate) {
                continue; // peer is in the past of the candidate, not its anticone
            }

            let peer_blue_anticone_size = self.blue_anticone_size(peer, new_block_data);
            candidate_blues_anticone_sizes.insert(peer, peer_blue_anticone_size);

            *candidate_blue_anticone_size += 1;
            if *candidate_blue_anticone_size > k {
                return ColoringState::Red;
            }
            if peer_blue_anticone_size == k {
                return ColoringState::Red;
            }
            assert!(peer_blue_anticone_size <= k, "found blue anticone larger than K");
        }

        ColoringState::Pending
    }

    /// Blue anticone size of `block` from the worldview of `context`.
    fn blue_anticone_size(&self, block: Hash, context: &GhostdagData) -> KType {
        let mut current_blues_anticone_sizes = Arc::clone(&context.blues_anticone_sizes);
        let mut current_selected_parent = context.selected_parent;
        loop {
            if let Some(size) = current_blues_anticone_sizes.get(&block) {
                return *size;
            }
            if current_selected_parent == self.genesis_hash || current_selected_parent == blockhash::ORIGIN {
                panic!("block {block} is not in blue set of the given context");
            }
            current_blues_anticone_sizes = self.ghostdag_store.get_blues_anticone_sizes(current_selected_parent).unwrap();
            current_selected_parent = self.ghostdag_store.get_selected_parent(current_selected_parent).unwrap();
        }
    }

    fn check_blue_candidate(&self, new_block_data: &GhostdagData, blue_candidate: Hash, k: KType) -> ColoringOutput {
        // mergeset_blues can be at most K+1 (it includes the selected parent).
        if new_block_data.mergeset_blues.len() as KType == k + 1 {
            return ColoringOutput::Red;
        }

        let mut candidate_blues_anticone_sizes: BlockHashMap<KType> = BlockHashMap::with_capacity(k as usize);
        let mut chain_block = ChainBlock { hash: None, data: Arc::new(new_block_data.clone()) };
        let mut candidate_blue_anticone_size: KType = 0;

        loop {
            let state = self.check_blue_candidate_with_chain_block(
                new_block_data,
                &chain_block,
                blue_candidate,
                &mut candidate_blues_anticone_sizes,
                &mut candidate_blue_anticone_size,
                k,
            );
            match state {
                ColoringState::Blue => {
                    return ColoringOutput::Blue(candidate_blue_anticone_size, candidate_blues_anticone_sizes)
                }
                ColoringState::Red => return ColoringOutput::Red,
                ColoringState::Pending => (),
            }
            let sp = chain_block.data.selected_parent;
            chain_block = ChainBlock { hash: Some(sp), data: self.ghostdag_store.get_data(sp).unwrap() };
        }
    }
}

struct ChainBlock {
    /// `None` signals the new block itself.
    hash: Option<Hash>,
    data: Arc<GhostdagData>,
}

enum ColoringState {
    Blue,
    Red,
    Pending,
}

enum ColoringOutput {
    Blue(KType, BlockHashMap<KType>),
    Red,
}
