#![allow(dead_code)]

use async_trait::async_trait;
use ckb_sdk::{
    constants::TYPE_ID_CODE_HASH,
    traits::{LiveCell, PrimaryScriptType},
    Address,
};
use ckb_types::{
    core::{Capacity, DepType, ScriptHashType, TransactionView},
    packed,
    prelude::*,
    H256,
};
use eth_light_client_in_ckb_verification::types::packed::{
    Client as PackedClient, ClientInfo as PackedClientInfo,
    ClientInfoReader as PackedClientInfoReader, ClientReader as PackedClientReader,
    ClientTypeArgs as PackedClientTypeArgs, Hash as PackedHash, ProofUpdate as PackedProofUpdate,
};

use super::{
    prelude::{CellSearcher, TxCompleter},
    rpc_client::RpcClient,
    utils,
};
use crate::error::Error;

fn make_typeid_script(type_args: Vec<u8>) -> packed::Script {
    packed::Script::new_builder()
        .code_hash(TYPE_ID_CODE_HASH.0.pack())
        .hash_type(ScriptHashType::Type.into())
        .args(type_args.pack())
        .build()
}

fn make_lightclient_script(script_typehash: packed::Byte32, args: Vec<u8>) -> packed::Script {
    packed::Script::new_builder()
        .code_hash(script_typehash)
        .hash_type(ScriptHashType::Type.into())
        .args(args.pack())
        .build()
}

async fn search_contract_cell<S: CellSearcher + Sync + ?Sized>(
    searcher: &S,
    script: &packed::Script,
    typeid_args: &H256,
) -> Result<LiveCell, Error> {
    let contract = searcher
        .search_cell(script, PrimaryScriptType::Type)
        .await?;
    let cell = match contract {
        Some(cell) => cell,
        None => {
            return Err(Error::rpc_response(format!(
                "contract not found: {}",
                hex::encode(typeid_args)
            )));
        }
    };
    Ok(cell)
}

pub struct UpdateCells {
    pub oldest: LiveCell,
    pub latest: LiveCell,
    pub info: LiveCell,
}

#[async_trait]
pub trait TxAssembler: CellSearcher + TxCompleter {
    async fn fetch_update_cells(
        &self,
        contract_typeid_args: &H256,
        client_type_args: &PackedClientTypeArgs,
    ) -> Result<Option<UpdateCells>, Error> {
        let contract_typescript = make_typeid_script(contract_typeid_args.as_bytes().to_vec());
        let type_hash = contract_typescript.calc_script_hash();
        // There are at most 255 cells
        let cells_count = u8::from(client_type_args.cells_count().as_reader());
        let cells = self
            .search_cells_by_typescript(&type_hash, client_type_args.as_slice(), cells_count as u32)
            .await?;

        // As for the error handling here, the only "allowable" error is that user supply a wrong client type args,
        // and we can't find any cells for it on chain. Otherwise, it means the on-chain data is corrupted.
        if cells.len() == 0 {
            return Ok(None);
        } else if cells.len() != cells_count as usize {
            panic!(
                "fetched client cells count not match: expect {}, actual {}",
                cells_count,
                cells.len()
            );
        }

        let mut client_cells = vec![];
        let mut client_info_cell_opt = None;
        for cell in cells {
            if PackedClientReader::verify(&cell.output_data, false).is_ok() {
                client_cells.push(cell);
            } else if PackedClientInfoReader::verify(&cell.output_data, false).is_ok() {
                let prev = client_info_cell_opt.replace(cell.clone());
                if prev.is_some() {
                    panic!(
                        "multi client cell has more than one client info:\nfirst:\n{:?}\nsecond:\n{:?}",
                        PackedClientInfo::new_unchecked(prev.unwrap().output_data),
                        PackedClientInfo::new_unchecked(cell.output_data),
                    );
                }
            } else {
                panic!("multi client cell has invalid data: {:?}", cell.output_data);
            }
        }

        let Some(client_info_cell) = client_info_cell_opt else {
            panic!("on-chain data corrupted: client info cell not found");
        };
        let client_info = PackedClientInfo::new_unchecked(client_info_cell.output_data.clone());
        let latest_id = u8::from(client_info.last_id().as_reader());
        // -1 is for the client info cell
        let oldest_id = (latest_id + 1) % (cells_count - 1);

        let mut oldest = None;
        let mut latest = None;

        for cell in client_cells {
            let client = PackedClient::new_unchecked(cell.output_data.clone());
            let client_id = u8::from(client.id().as_reader());
            if client_id == latest_id {
                latest.replace(cell).expect("on-chain data corrupted");
            } else if client_id == oldest_id {
                oldest.replace(cell).expect("on-chain data corrupted");
            }
        }
        let (Some(oldest), Some(latest)) = (oldest, latest) else {
            panic!("on-chain data corrupted: oldest or latest client not found");
        };
        let update_cells = UpdateCells {
            oldest,
            latest,
            info: client_info_cell,
        };

        Ok(Some(update_cells))
    }

    async fn fetch_packed_client(
        &self,
        contract_typeid_args: &H256,
        client_id: &String,
    ) -> Result<Option<PackedClient>, Error> {
        let contract_typescript = make_typeid_script(contract_typeid_args.as_bytes().to_vec());
        let type_hash = contract_typescript.calc_script_hash();
        let lightclient_cell_opt = self
            .search_cell_by_typescript(&type_hash, &client_id.as_bytes().to_vec())
            .await?;
        match lightclient_cell_opt {
            Some(cell) => {
                if let Err(err) = PackedClientReader::verify(&cell.output_data, false) {
                    Err(Error::rpc_response(format!("client format error: {}", err)))
                } else {
                    Ok(Some(PackedClient::new_unchecked(cell.output_data)))
                }
            }
            None => Ok(None),
        }
    }

    async fn assemble_create_multi_client_transaction(
        &self,
        address: &Address,
        clients: Vec<PackedClient>,
        client_info: PackedClientInfo,
        lock_typeid_args: &H256,
        contract_typeid_args: &H256,
        packed_proof_update: PackedProofUpdate,
    ) -> Result<(TransactionView, Vec<packed::CellOutput>), Error> {
        let cells_count = (clients.len() + 1) as u8;

        let contract_script = make_typeid_script(contract_typeid_args.as_bytes().to_vec());
        let contract_script_hash = contract_script.calc_script_hash();
        let contract_celldep = {
            let contract_cell =
                search_contract_cell(self, &contract_script, contract_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(contract_cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };
        let lock_script = make_typeid_script(lock_typeid_args.as_bytes().to_vec());
        let lock_celldep = {
            let cell = search_contract_cell(self, &lock_script, lock_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };

        // We have to get one input cell to calculate the type id for those new cells.
        let mut _excessive_capacity = 0;
        let input_cells = self
            .search_cells_by_address_and_capacity(address, 1, &mut _excessive_capacity)
            .await?;
        let inputs_capacity: u64 = input_cells
            .iter()
            .map(|c| Unpack::<u64>::unpack(&c.output.capacity()))
            .sum();
        let (inputs, mut inputs_as_cell_outputs): (Vec<packed::CellInput>, Vec<packed::CellOutput>) =
            input_cells
                .into_iter()
                .map(|cell| {
                    let input = packed::CellInput::new(cell.out_point, 0);
                    let input_as_cell_output = cell.output;
                    (input, input_as_cell_output)
                })
                .unzip();

        // TODO: how to avoid those tedious type conversions?
        let type_script: packed::Script = {
            let first = inputs.first().expect("input cell not found");
            let type_id = {
                let type_id = utils::calculate_type_id(first, cells_count as usize);
                PackedHash::from_slice(type_id.as_slice()).expect("build type id")
            };
            let client_type_args = PackedClientTypeArgs::new_builder()
                .cells_count(packed::Byte::new(cells_count))
                .type_id(type_id)
                .build();
            let args = packed::Bytes::from_slice(client_type_args.as_slice())
                .expect("build type script args");
            packed::Script::new_builder()
                .code_hash(contract_script_hash)
                .hash_type(ScriptHashType::Type.into())
                .args(args)
                .build()
        };

        let mut outputs_data = vec![client_info.as_slice().pack()];
        outputs_data.extend(clients.into_iter().map(|client| client.as_slice().pack()));
        let outputs = outputs_data
            .iter()
            .map(|data| {
                packed::CellOutput::new_builder()
                    .lock(lock_script.clone())
                    .type_(Some(type_script.clone()).pack())
                    .build_exact_capacity(Capacity::bytes(data.len()).unwrap())
                    .expect("build ibc contract output")
            })
            .collect::<Vec<_>>();

        let witness = {
            let input_type_args = packed::BytesOpt::new_builder()
                .set(Some(packed_proof_update.as_slice().pack()))
                .build();
            let witness_args = packed::WitnessArgs::new_builder()
                .input_type(input_type_args)
                .build();
            witness_args.as_bytes().pack()
        };
        let tx = TransactionView::new_advanced_builder()
            .inputs(inputs)
            .outputs(outputs)
            .outputs_data(outputs_data)
            .witness(witness)
            .cell_dep(contract_celldep)
            .cell_dep(lock_celldep)
            .build();

        let fee_rate = 3000;
        let (tx, mut new_inputs_as_cell_outputs) = self
            .complete_tx_with_secp256k1_change(tx, address, inputs_capacity, fee_rate)
            .await?;
        inputs_as_cell_outputs.append(&mut new_inputs_as_cell_outputs);
        Ok((tx, inputs_as_cell_outputs))
    }

    async fn assemble_update_multi_client_transaction(
        &self,
        address: &Address,
        oldest_cell: LiveCell,
        info_cell: LiveCell,
        updated_client: PackedClient,
        client_type_args: &PackedClientTypeArgs,
        lock_typeid_args: &H256,
        contract_typeid_args: &H256,
        packed_proof_update: PackedProofUpdate,
    ) -> Result<(TransactionView, Vec<packed::CellOutput>), Error> {
        let contract_script = make_typeid_script(contract_typeid_args.as_bytes().to_vec());
        let contract_script_hash = contract_script.calc_script_hash();
        let contract_celldep = {
            let contract_cell =
                search_contract_cell(self, &contract_script, contract_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(contract_cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };

        let lock_script = make_typeid_script(lock_typeid_args.as_bytes().to_vec());
        let lock_celldep = {
            let cell = search_contract_cell(self, &lock_script, lock_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };
        let type_script: packed::Script = {
            let args = packed::Bytes::from_slice(client_type_args.as_slice())
                .expect("build type script args");
            packed::Script::new_builder()
                .code_hash(contract_script_hash)
                .hash_type(ScriptHashType::Type.into())
                .args(args)
                .build()
        };

        let (new_info_output, new_info_output_data) = {
            let last_id = {
                let oldest_client = PackedClient::new_unchecked(oldest_cell.output_data.clone());
                u8::from(oldest_client.id().as_reader())
            };

            let info = PackedClientInfo::new_unchecked(info_cell.output_data.clone())
                .as_builder()
                .last_id(last_id.into())
                .build();
            let output_data = info.as_slice().pack();
            let output = packed::CellOutput::new_builder()
                .lock(lock_script.clone())
                .type_(Some(type_script.clone()).pack())
                .build_exact_capacity(Capacity::bytes(output_data.len()).unwrap())
                .expect("build ibc contract output");
            (output, output_data)
        };

        let (new_client_output, new_client_output_data) = {
            let output_data = updated_client.as_slice().pack();
            let output = packed::CellOutput::new_builder()
                .lock(lock_script.clone())
                .type_(Some(type_script.clone()).pack())
                .build_exact_capacity(Capacity::bytes(output_data.len()).unwrap())
                .expect("build ibc contract output");
            (output, output_data)
        };

        // Later handling outside requires the CellOutput form of inputs.
        let input_cells = [oldest_cell, info_cell];
        let inputs_capacity: u64 = input_cells
            .iter()
            .map(|c| Unpack::<u64>::unpack(&c.output.capacity()))
            .sum();
        let (inputs, mut inputs_as_cell_outputs): (Vec<packed::CellInput>, Vec<packed::CellOutput>) =
            input_cells
                .into_iter()
                .map(|cell| {
                    let input = packed::CellInput::new(cell.out_point, 0);
                    let input_as_cell_output = cell.output;
                    (input, input_as_cell_output)
                })
                .unzip();

        let witness = {
            let input_type_args = packed::BytesOpt::new_builder()
                .set(Some(packed_proof_update.as_slice().pack()))
                .build();
            let witness_args = packed::WitnessArgs::new_builder()
                .input_type(input_type_args)
                .build();
            witness_args.as_bytes().pack()
        };
        let tx = TransactionView::new_advanced_builder()
            .inputs(inputs)
            .outputs([new_info_output, new_client_output])
            .outputs_data([new_info_output_data, new_client_output_data])
            .witness(witness)
            .cell_dep(contract_celldep)
            .cell_dep(lock_celldep)
            .build();

        let fee_rate = 3000;
        let (tx, mut new_inputs_as_cell_outputs) = self
            .complete_tx_with_secp256k1_change(tx, address, inputs_capacity, fee_rate)
            .await?;
        inputs_as_cell_outputs.append(&mut new_inputs_as_cell_outputs);
        Ok((tx, inputs_as_cell_outputs))
    }

    async fn assemble_updates_into_transaction(
        &self,
        address: &Address,
        packed_client: PackedClient,
        packed_proof_update: PackedProofUpdate,
        lock_typeid_args: &H256,
        contract_typeid_args: &H256,
        client_id: &String,
    ) -> Result<(TransactionView, Vec<packed::CellOutput>), Error> {
        // find celldeps by searching live cells according typeid_args
        let contract_typescript = make_typeid_script(contract_typeid_args.as_bytes().to_vec());
        let contract_cell_dep = {
            let contract_cell =
                search_contract_cell(self, &contract_typescript, contract_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(contract_cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };
        let mock_lockscript = make_typeid_script(lock_typeid_args.as_bytes().to_vec());
        let mock_lock_celldep = {
            let mock_cell = search_contract_cell(self, &mock_lockscript, lock_typeid_args).await?;
            packed::CellDep::new_builder()
                .out_point(mock_cell.out_point)
                .dep_type(DepType::Code.into())
                .build()
        };
        // search light-client cell by lightclient contract type_id hash
        let contract_typehash = contract_typescript.calc_script_hash();
        let lightclient_cell_opt = self
            .search_cell_by_typescript(&contract_typehash, &client_id.as_bytes().to_vec())
            .await?;
        // build Lightclient Lockscript and Typescript
        let pubkey_hash = address.payload().args();
        let lightclient_lock =
            make_lightclient_script(mock_lockscript.calc_script_hash(), pubkey_hash.to_vec());
        let lightclient_type =
            make_lightclient_script(contract_typehash, client_id.clone().into_bytes());
        // assemble Lightclient output cell
        let output_data = packed_client.as_slice().pack();
        let output_cell = packed::CellOutput::new_builder()
            .lock(lightclient_lock)
            .type_(Some(lightclient_type).pack())
            .build_exact_capacity(Capacity::bytes(output_data.len()).unwrap())
            .expect("build ibc contract output");
        let mut inputs_cell_as_output = vec![];
        let mut inputs_cell = vec![];
        let mut inputs_capacity: u64 = 0;
        if let Some(lightclient_cell) = lightclient_cell_opt {
            inputs_cell.push(packed::CellInput::new(lightclient_cell.out_point, 0));
            inputs_capacity += Unpack::<u64>::unpack(&lightclient_cell.output.capacity());
            inputs_cell_as_output.push(lightclient_cell.output);
        }
        // assemble Lightclient witness
        let witness = {
            let input_type_args = packed::BytesOpt::new_builder()
                .set(Some(packed_proof_update.as_slice().pack()))
                .build();
            let witness_args = packed::WitnessArgs::new_builder()
                .input_type(input_type_args)
                .build();
            witness_args.as_bytes()
        };
        // assemble transaction
        let tx = TransactionView::new_advanced_builder()
            .inputs(inputs_cell)
            .output(output_cell)
            .output_data(output_data)
            .witness(witness.pack())
            .cell_dep(contract_cell_dep)
            .cell_dep(mock_lock_celldep)
            .build();
        let fee_rate = 3000;
        let (tx, mut new_inputs) = self
            .complete_tx_with_secp256k1_change(tx, address, inputs_capacity, fee_rate)
            .await?;
        // collect input cells to support signing process (calculating input group)
        inputs_cell_as_output.append(&mut new_inputs);
        Ok((tx, inputs_cell_as_output))
    }
}

impl TxAssembler for RpcClient {}
