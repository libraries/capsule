use super::recipe::*;
use crate::config::{Cell, CellLocation, DepGroup, Deployment};
use crate::wallet::{cli_types::LiveCell, *};

use anyhow::Result;
use ckb_tool::ckb_chain_spec::consensus::TYPE_ID_CODE_HASH;
use ckb_tool::ckb_hash::new_blake2b;
use ckb_tool::ckb_types::{
    bytes::Bytes,
    core::{Capacity, ScriptHashType, TransactionBuilder, TransactionView},
    packed,
    prelude::*,
    H256,
};
use std::fs;
use std::io::Read;

pub struct DeploymentProcess {
    wallet: Wallet,
    tx_fee: Capacity,
    config: Deployment,
}

impl DeploymentProcess {
    pub fn new(config: Deployment, wallet: Wallet, tx_fee: Capacity) -> Self {
        DeploymentProcess {
            wallet,
            tx_fee,
            config,
        }
    }

    /// generate recipe and deploy
    pub fn prepare_recipe(
        &mut self,
        pre_inputs_cells: Vec<(String, LiveCell)>,
    ) -> Result<(DeploymentRecipe, Vec<TransactionView>)> {
        let cells: Vec<(Cell, Bytes)> = load_deployable_cells_data(&self.config.cells)?;
        let dep_groups = self.config.dep_groups.clone();
        Ok(self.build_recipe(cells, dep_groups, pre_inputs_cells))
    }

    fn build_cell_tx(
        &mut self,
        cell: Cell,
        data: Bytes,
        pre_input_cell: Option<LiveCell>,
    ) -> Result<TransactionView> {
        let lock: packed::Script = self.config.lock.to_owned().into();
        let mut inputs_cells = Vec::new();
        if let Some(live_cell) = pre_input_cell {
            self.wallet
                .lock_out_points(vec![live_cell.out_point()].into_iter());
            inputs_cells.push(live_cell);
        }
        // collect cells if inputs_cells is empty, type_id requires at least one input
        if cell.enable_type_id && inputs_cells.is_empty() {
            inputs_cells.extend(
                self.wallet
                    .collect_live_cells(Capacity::shannons(1))
                    .into_iter()
                    .map(|i| i.into()),
            );
            self.wallet
                .lock_out_points(inputs_cells.iter().map(|c| c.out_point()));
        }
        // build outputs
        let output = {
            let mut output = packed::CellOutput::new_builder().lock(lock.clone());
            if cell.enable_type_id {
                let input_cell = &inputs_cells[0];
                let tx: packed::Transaction = self
                    .wallet
                    .query_transaction(&input_cell.tx_hash)?
                    .expect("tx")
                    .transaction
                    .inner
                    .into();
                let tx: TransactionView = tx.into_view();
                let input_cell_output =
                    tx.outputs().get(input_cell.index as usize).expect("output");
                // inherit type id from input cell or create a new one
                let type_script = match input_cell_output.type_().to_opt() {
                    Some(script) if is_type_id_script(&script) => script,
                    _ => {
                        let output_index = 0;
                        build_type_id_script(&input_cell.input(), output_index)
                    }
                };
                output = output.type_(Some(type_script).pack());
            }
            output
                .build_exact_capacity(Capacity::bytes(data.len()).expect("bytes"))
                .expect("build")
        };
        let tx = TransactionBuilder::default()
            .inputs(inputs_cells.iter().map(|cell| cell.input()))
            .output(output)
            .output_data(data.pack())
            .build();
        let tx = self.wallet.complete_tx_lock_deps(tx);
        let inputs_capacity = inputs_cells.iter().map(|cell| cell.capacity).sum::<u64>();
        let tx =
            self.wallet
                .complete_tx_inputs(tx, Capacity::shannons(inputs_capacity), self.tx_fee);
        self.wallet.lock_tx_inputs(&tx);
        Ok(tx)
    }

    fn build_dep_group_tx(
        &mut self,
        cell_recipes: &[CellRecipe],
        dep_group: DepGroup,
        pre_input_cell: Option<LiveCell>,
    ) -> TransactionView {
        fn find_cell(name: &str, cell_recipes: &[CellRecipe]) -> Option<(H256, CellRecipe)> {
            cell_recipes
                .into_iter()
                .find(|c| c.name == name)
                .map(|cell_recipe| (cell_recipe.tx_hash.to_owned(), cell_recipe.clone()))
        }

        let lock: packed::Script = self.config.lock.to_owned().into();
        let out_points: packed::OutPointVec = dep_group
            .cells
            .iter()
            .map(|name| {
                let cell = self
                    .config
                    .cells
                    .iter()
                    .find(|c| &c.name == name)
                    .expect("find cell");
                let (tx_hash, index) = match cell.location.clone() {
                    CellLocation::File { .. } => {
                        let (tx_hash, cell) = find_cell(name, cell_recipes).expect("must exists");
                        (tx_hash, cell.index)
                    }
                    CellLocation::OutPoint { tx_hash, index } => (tx_hash.into(), index),
                };
                packed::OutPoint::new_builder()
                    .tx_hash(tx_hash.pack())
                    .index(index.pack())
                    .build()
            })
            .pack();
        let data = out_points.as_bytes();
        let data_len = data.len();
        let output = packed::CellOutput::new_builder()
            .lock(lock.clone())
            .build_exact_capacity(Capacity::bytes(data_len).expect("bytes"))
            .expect("build");
        let (inputs, inputs_capacity) = match pre_input_cell {
            Some(cell) => (vec![cell.input()], cell.capacity),
            None => (vec![], 0),
        };
        let tx = TransactionBuilder::default()
            .inputs(inputs)
            .output(output)
            .output_data(data.pack())
            .build();
        let tx = self.wallet.complete_tx_lock_deps(tx);
        let tx =
            self.wallet
                .complete_tx_inputs(tx, Capacity::shannons(inputs_capacity), self.tx_fee);
        self.wallet.lock_tx_inputs(&tx);
        tx
    }

    fn build_recipe(
        &mut self,
        cells: Vec<(Cell, Bytes)>,
        dep_groups: Vec<DepGroup>,
        pre_inputs_cells: Vec<(String, LiveCell)>,
    ) -> (DeploymentRecipe, Vec<TransactionView>) {
        let mut txs = Vec::new();
        let mut cell_recipes = Vec::new();
        let mut dep_group_recipes = Vec::new();
        // build cells tx
        for (cell, data) in cells {
            let input_cell = pre_inputs_cells
                .iter()
                .find(|(name, _cell)| name == &cell.name)
                .map(|(_name, input_cell)| input_cell.clone());
            let tx = self
                .build_cell_tx(cell.clone(), data, input_cell)
                .expect("cell deployment tx");
            let cell_recipe = build_cell_recipe(&tx, cell);
            txs.push(tx);
            cell_recipes.push(cell_recipe);
        }
        // build dep_groups tx
        for dep_group in dep_groups {
            let input_cell = pre_inputs_cells
                .iter()
                .find(|(name, _cell)| name == &dep_group.name)
                .map(|(_name, input_cell)| input_cell.clone());
            let tx = self.build_dep_group_tx(&cell_recipes, dep_group.clone(), input_cell);
            let dep_group_recipe = build_dep_group_recipe(&tx, dep_group);
            txs.push(tx);
            dep_group_recipes.push(dep_group_recipe)
        }
        // construct deployment recipe
        let recipe = DeploymentRecipe {
            cell_recipes,
            dep_group_recipes,
        };
        (recipe, txs)
    }

    pub fn sign_txs(&self, txs: Vec<TransactionView>) -> Result<Vec<TransactionView>> {
        let password = self.wallet.read_password().expect("read password");
        txs.into_iter()
            .map(|tx| self.wallet.sign_tx(tx, password.clone()))
            .collect()
    }

    pub fn execute_recipe(
        &mut self,
        recipe: DeploymentRecipe,
        txs: Vec<TransactionView>,
    ) -> Result<()> {
        for cell_recipe in recipe.cell_recipes {
            if self
                .wallet
                .query_transaction(&cell_recipe.tx_hash)?
                .is_some()
            {
                continue;
            }
            let tx = txs
                .iter()
                .find(|tx| {
                    let tx_hash = tx.hash().unpack();
                    cell_recipe.tx_hash == tx_hash
                })
                .expect("missing recipe tx");
            let tx_hash = self.wallet.send_transaction(tx.to_owned())?;
            println!("send cell_tx {}", tx_hash);
        }
        for dep_group_recipe in recipe.dep_group_recipes {
            if self
                .wallet
                .query_transaction(&dep_group_recipe.tx_hash)?
                .is_some()
            {
                continue;
            }
            let tx = txs
                .iter()
                .find(|tx| {
                    let tx_hash = tx.hash().unpack();
                    dep_group_recipe.tx_hash == tx_hash
                })
                .expect("missing recipe tx");
            let tx_hash = self.wallet.send_transaction(tx.to_owned())?;
            println!("send dep_group_tx {}", tx_hash);
        }
        Ok(())
    }
}

fn build_cell_recipe(tx: &TransactionView, cell: Cell) -> CellRecipe {
    let index = 0;
    let cell_output = tx.outputs().get(index).expect("get cell");
    let data: Bytes = tx.outputs_data().get(index).expect("get data").unpack();
    let occupied_capacity = cell_output
        .occupied_capacity(Capacity::bytes(data.len()).expect("capacity"))
        .expect("capacity")
        .as_u64();
    let type_id = if cell.enable_type_id {
        Some(
            cell_output
                .type_()
                .to_opt()
                .expect("type id")
                .calc_script_hash()
                .unpack(),
        )
    } else {
        None
    };
    CellRecipe {
        index: index as u32,
        name: cell.name.to_owned(),
        data_hash: packed::CellOutput::calc_data_hash(&data).unpack(),
        occupied_capacity,
        tx_hash: tx.hash().unpack(),
        type_id,
    }
}

fn build_dep_group_recipe(tx: &TransactionView, dep_group: DepGroup) -> DepGroupRecipe {
    let index = 0;
    let data: Bytes = tx.outputs_data().get(index).expect("get data").unpack();
    let occupied_capacity = tx
        .outputs()
        .get(index)
        .expect("get cell")
        .occupied_capacity(Capacity::bytes(data.len()).expect("capacity"))
        .expect("capacity")
        .as_u64();
    DepGroupRecipe {
        index: index as u32,
        name: dep_group.name.to_owned(),
        occupied_capacity,
        tx_hash: tx.hash().unpack(),
    }
}

fn is_type_id_script(script: &packed::Script) -> bool {
    script.code_hash() == TYPE_ID_CODE_HASH.pack()
        && script.hash_type() == ScriptHashType::Type.into()
}

fn build_type_id_script(input: &packed::CellInput, output_index: u64) -> packed::Script {
    let mut blake2b = new_blake2b();
    blake2b.update(&input.as_slice());
    blake2b.update(&output_index.to_le_bytes());
    let mut ret = [0; 32];
    blake2b.finalize(&mut ret);
    let script_arg = Bytes::from(ret.to_vec());
    packed::Script::new_builder()
        .code_hash(TYPE_ID_CODE_HASH.pack())
        .hash_type(ScriptHashType::Type.into())
        .args(script_arg.pack())
        .build()
}

fn load_deployable_cells_data(cells: &[Cell]) -> Result<Vec<(Cell, Bytes)>> {
    let mut cells_data: Vec<(Cell, Bytes)> = Vec::new();
    for cell in cells {
        match cell.location.to_owned() {
            CellLocation::OutPoint { .. } => {}
            CellLocation::File { file } => {
                let mut data = Vec::new();
                match fs::File::open(&file).and_then(|mut f| f.read_to_end(&mut data)) {
                    Ok(_) => {}
                    Err(err) => {
                        eprintln!("failed to read cell data from '{}', err: {}", file, &err);
                        return Err(err.into());
                    }
                }
                cells_data.push((cell.to_owned(), data.into()));
            }
        }
    }
    Ok(cells_data)
}