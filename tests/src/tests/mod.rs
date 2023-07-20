use ckb_chain_spec::consensus::{ConsensusBuilder, TYPE_ID_CODE_HASH};
use ckb_hash::{blake2b_256, new_blake2b};
use ckb_script::{TransactionScriptsVerifier, TxVerifyEnv};
use ckb_traits::{CellDataProvider, ExtensionProvider, HeaderProvider};
use ckb_types::{
    bytes::Bytes,
    core::{
        cell::{CellMeta, CellMetaBuilder, ResolvedTransaction},
        hardfork::{HardForks, CKB2021, CKB2023},
        Capacity, DepType, EpochExt, HeaderBuilder, HeaderView, ScriptHashType, TransactionBuilder,
    },
    packed::{self, Byte32, CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
};
use lazy_static::lazy_static;
use merkle_cbt::{merkle_tree::Merge, MerkleTree, CBMT};
use rand::{thread_rng, Rng};
use std::collections::HashMap;
use std::sync::Arc;

lazy_static! {
    pub static ref ZERO_LOCK_PATH: String = std::env::var("ZERO_LOCK_PATH").unwrap_or_else(|_| {
        "../target/riscv64imac_zba_zbb_zbc_zbs-unknown-ckb-elf/release/ckb-zero-lock".to_string()
    });
    pub static ref ZERO_LOCK_BIN: Bytes =
        Bytes::from(std::fs::read(&*ZERO_LOCK_PATH).expect("read"));
    pub static ref ALWAYS_SUCCESS_BIN: Bytes =
        Bytes::from(ckb_always_success_script::ALWAYS_SUCCESS.to_vec());
}

#[test]
fn test_zero_lock_exists() {
    assert!(ZERO_LOCK_BIN.len() > 0);
}

#[derive(Default, Clone)]
pub struct DummyDataLoader {
    pub cells: HashMap<OutPoint, (CellOutput, Bytes)>,
    pub headers: HashMap<Byte32, HeaderView>,
    pub extensions: HashMap<Byte32, Bytes>,
}

impl CellDataProvider for DummyDataLoader {
    fn get_cell_data(&self, out_point: &OutPoint) -> Option<Bytes> {
        self.cells.get(out_point).map(|(_, data)| data.clone())
    }

    fn get_cell_data_hash(&self, out_point: &OutPoint) -> Option<Byte32> {
        self.cells
            .get(out_point)
            .map(|(_, data)| CellOutput::calc_data_hash(data))
    }
}

impl HeaderProvider for DummyDataLoader {
    fn get_header(&self, block_hash: &Byte32) -> Option<HeaderView> {
        self.headers.get(block_hash).cloned()
    }
}

impl ExtensionProvider for DummyDataLoader {
    fn get_block_extension(&self, hash: &Byte32) -> Option<packed::Bytes> {
        self.extensions.get(hash).map(|data| data.pack())
    }
}

fn random_out_point() -> OutPoint {
    let tx_hash = {
        let mut rng = thread_rng();
        let mut buf = [0u8; 32];
        rng.fill(&mut buf);
        buf.pack()
    };
    OutPoint::new(tx_hash, 0)
}

fn random_type_id_script() -> Script {
    let mut rng = thread_rng();
    let args = {
        let mut buf = vec![0u8; 32];
        rng.fill(&mut buf[..]);
        buf.pack()
    };
    Script::new_builder()
        .code_hash(TYPE_ID_CODE_HASH.pack())
        .hash_type(ScriptHashType::Type.into())
        .args(args)
        .build()
}

fn insert_cell(dummy: &mut DummyDataLoader, cell_meta: &CellMeta) {
    dummy.cells.insert(
        cell_meta.out_point.clone(),
        (
            cell_meta.cell_output.clone(),
            cell_meta.mem_cell_data.clone().unwrap(),
        ),
    );
}

fn script_cell(dummy: &mut DummyDataLoader, script_data: &Bytes) -> CellMeta {
    let out_point = random_out_point();
    let cell = CellOutput::new_builder()
        .capacity(
            Capacity::bytes(script_data.len())
                .expect("script capacity")
                .pack(),
        )
        .build();
    let cell_meta = CellMetaBuilder::from_cell_output(cell, script_data.clone())
        .out_point(out_point)
        .build();
    insert_cell(dummy, &cell_meta);
    cell_meta
}

fn zero_lock_cell(
    dummy: &mut DummyDataLoader,
    data: &Bytes,
    type_script: Option<Script>,
) -> CellMeta {
    let out_point = random_out_point();
    let lock = Script::new_builder()
        .code_hash(CellOutput::calc_data_hash(&ZERO_LOCK_BIN))
        .hash_type(ScriptHashType::Data2.into())
        .build();
    let cell = CellOutput::new_builder()
        .lock(lock)
        .type_(type_script.pack())
        .capacity(Capacity::bytes(data.len()).expect("script capacity").pack())
        .build();
    let cell_meta = CellMetaBuilder::from_cell_output(cell, data.clone())
        .out_point(out_point)
        .build();
    insert_cell(dummy, &cell_meta);
    cell_meta
}

fn complete_tx(
    mut dummy: DummyDataLoader,
    builder: TransactionBuilder,
    input_cells: Vec<CellMeta>,
) -> TransactionScriptsVerifier<DummyDataLoader> {
    let rtx: Arc<ResolvedTransaction> = {
        let zero_lock_cell_meta = script_cell(&mut dummy, &ZERO_LOCK_BIN);
        let always_success_cell_meta = script_cell(&mut dummy, &ALWAYS_SUCCESS_BIN);

        let tx = builder
            .cell_dep(
                CellDep::new_builder()
                    .out_point(zero_lock_cell_meta.out_point.clone())
                    .dep_type(DepType::Code.into())
                    .build(),
            )
            .cell_dep(
                CellDep::new_builder()
                    .out_point(always_success_cell_meta.out_point.clone())
                    .dep_type(DepType::Code.into())
                    .build(),
            )
            .inputs(
                input_cells
                    .iter()
                    .map(|input| CellInput::new(input.out_point.clone(), 0)),
            )
            .build();

        Arc::new(ResolvedTransaction {
            transaction: tx,
            resolved_inputs: input_cells.clone(),
            resolved_cell_deps: vec![zero_lock_cell_meta, always_success_cell_meta],
            resolved_dep_groups: vec![],
        })
    };

    let consensus = Arc::new(
        ConsensusBuilder::default()
            .hardfork_switch(HardForks {
                ckb2021: CKB2021::new_dev_default(),
                ckb2023: CKB2023::new_dev_default(),
            })
            .build(),
    );
    let tip = HeaderBuilder::default().number(0.pack()).build();
    let tx_verify_env = Arc::new(TxVerifyEnv::new_submit(&tip));

    let mut groups = HashMap::new();
    for (i, input_cell) in input_cells.iter().enumerate() {
        let lock_hash = input_cell.cell_output.lock().calc_script_hash();
        groups
            .entry(lock_hash)
            .or_insert(format!("Lock script of input cell {}", i));
        if let Some(type_script) = input_cell.cell_output.type_().to_opt() {
            let type_hash = type_script.calc_script_hash();
            groups
                .entry(type_hash)
                .or_insert(format!("Type script of input cell {}", i));
        }
    }
    for (i, output_cell) in rtx
        .transaction
        .data()
        .raw()
        .outputs()
        .into_iter()
        .enumerate()
    {
        if let Some(type_script) = output_cell.type_().to_opt() {
            let type_hash = type_script.calc_script_hash();
            groups
                .entry(type_hash)
                .or_insert(format!("Type script of output cell {}", i));
        }
    }

    let mut verifier = TransactionScriptsVerifier::new(rtx, dummy, consensus, tx_verify_env);
    verifier.set_debug_printer(move |hash: &Byte32, message: &str| {
        let prefix = match groups.get(hash) {
            Some(text) => text.clone(),
            None => format!("Script group: {:x}", hash),
        };
        eprintln!("{} DEBUG OUTPUT: {}", prefix, message);
    });
    verifier
}

#[derive(Debug)]
struct Blake2bHash;

impl Merge for Blake2bHash {
    type Item = Byte32;

    fn merge(lhs: &Self::Item, rhs: &Self::Item) -> Self::Item {
        let mut hasher = new_blake2b();
        hasher.update(&lhs.as_bytes());
        hasher.update(&rhs.as_bytes());
        let mut hash = [0u8; 32];
        hasher.finalize(&mut hash[..]);
        Byte32::new(hash)
    }
}

fn hash_upgrade_data(old_contract: &Bytes, new_contract: &Bytes, new_cell: &CellOutput) -> Byte32 {
    let mut hasher = new_blake2b();
    hasher.update(&[1u8]);
    hasher.update(&blake2b_256(old_contract)[..]);
    hasher.update(&blake2b_256(new_contract)[..]);
    hasher.update(new_cell.as_slice());
    let mut hash = [0u8; 32];
    hasher.finalize(&mut hash[..]);
    Byte32::new(hash)
}

fn build_merkle_root_n_proof(all_leaves: &[Byte32], selected: u32) -> (Byte32, Bytes) {
    let tree: MerkleTree<Byte32, Blake2bHash> = CBMT::build_merkle_tree(all_leaves);
    let proof = tree.build_proof(&[selected]).expect("build merkle proof");

    let mut data = vec![];
    data.extend(
        TryInto::<u32>::try_into(proof.indices().len())
            .unwrap()
            .to_le_bytes(),
    );
    for index in proof.indices() {
        data.extend(index.to_le_bytes());
    }
    data.extend(
        TryInto::<u32>::try_into(proof.lemmas().len())
            .unwrap()
            .to_le_bytes(),
    );
    for lemma in proof.lemmas() {
        data.extend(lemma.as_slice());
    }

    let witness = WitnessArgs::new_builder()
        .lock(Some(Bytes::from(data)).pack())
        .build();

    (tree.root(), witness.as_bytes())
}

fn header(dummy: &mut DummyDataLoader, merkle_root: &Byte32) -> Byte32 {
    let mut rng = thread_rng();
    let epoch_ext = EpochExt::new_builder()
        .number(10)
        .start_number(9500)
        .length(1010)
        .build();
    let header = HeaderBuilder::default()
        .number(10000.pack())
        .epoch(epoch_ext.number_with_fraction(10000).pack())
        .transactions_root({
            let mut d = [0u8; 32];
            rng.fill(&mut d);
            Byte32::new(d)
        })
        .build();
    let mut extension = vec![0u8; 180];
    rng.fill(&mut extension[..]);
    extension[128..160].copy_from_slice(&merkle_root.as_bytes());
    let hash = header.hash();
    dummy.headers.insert(hash.clone(), header);
    dummy
        .extensions
        .insert(hash.clone(), Bytes::from(extension));
    hash
}

#[test]
fn test_single_zero_lock_upgrade() {
    let mut dummy_loader = DummyDataLoader::default();
    let type_id = random_type_id_script();
    let old_contract = vec![1u8; 100].into();
    let input_cell_meta = zero_lock_cell(&mut dummy_loader, &old_contract, Some(type_id.clone()));
    let new_contract = vec![2u8; 100].into();
    let output_cell_meta = zero_lock_cell(&mut dummy_loader, &new_contract, Some(type_id));

    let upgrade_hash = hash_upgrade_data(
        input_cell_meta.mem_cell_data.as_ref().unwrap(),
        output_cell_meta.mem_cell_data.as_ref().unwrap(),
        &output_cell_meta.cell_output,
    );

    let (root, proof_witness) = build_merkle_root_n_proof(&[upgrade_hash], 0);
    let header_dep = header(&mut dummy_loader, &root);

    let builder = TransactionBuilder::default()
        .output(output_cell_meta.cell_output.clone())
        .output_data(output_cell_meta.mem_cell_data.clone().unwrap().pack())
        .header_dep(header_dep)
        .witness(proof_witness.pack());

    let verifier = complete_tx(dummy_loader, builder, vec![input_cell_meta]);

    let verify_result = verifier.verify(u64::MAX);
    verify_result.expect("pass verification");
}
