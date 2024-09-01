use avalanche_types::hash::sha256;
use derivative::Derivative;
use serde::{Deserialize, Serialize};
use solana_sdk::bpf_loader_upgradeable::UpgradeableLoaderState;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{
    account::{AccountSharedData, WritableAccount},
    bpf_loader_upgradeable,
};

use crate::processor::EscobarDB;

#[derive(Serialize, Deserialize, Clone, Derivative)]
#[derivative(Debug, PartialEq, Eq)]
pub struct DeployTx {
    pub program_data: Vec<u8>,
    pub deployment_slot: u64,
}

impl DeployTx {
    pub fn new(program_data: Vec<u8>, deployment_slot: u64) -> Self {
        Self {
            program_data,
            deployment_slot,
        }
    }

    pub fn get_prog_pubkey(&self) -> Pubkey {
        let hash = sha256(&self.program_data.clone());
        let pubkey = Pubkey::try_from(hash).unwrap();
        Pubkey::create_program_address(&[b"program"], &pubkey).unwrap()
    }
}

// program account and program data account are hash of the program_data and deployment_slot over x.
pub fn deploy_program(deploy_txs: &[DeployTx], db: &EscobarDB) {
    for deploy_tx in deploy_txs {
        let hash = sha256(&deploy_tx.program_data.clone());
        let pubkey = Pubkey::try_from(hash).unwrap();
        let program_account = Pubkey::create_program_address(&[b"program"], &pubkey).unwrap();
        let program_data_account =
            Pubkey::create_program_address(&[b"program_data"], &program_account).unwrap();

        let state = UpgradeableLoaderState::Program {
            programdata_address: program_data_account,
        };

        // The program account must have funds and hold the executable binary
        let mut account_data = AccountSharedData::default();
        account_data.set_data_from_slice(&bincode::serialize(&state).unwrap());
        account_data.set_lamports(25);
        account_data.set_owner(bpf_loader_upgradeable::id());

        db.write(program_account, account_data);

        let mut account_data = AccountSharedData::default();
        let state = UpgradeableLoaderState::ProgramData {
            slot: deploy_tx.deployment_slot,
            upgrade_authority_address: None,
        };
        let mut header = bincode::serialize(&state).unwrap();
        let mut complement = vec![
            0;
            std::cmp::max(
                0,
                UpgradeableLoaderState::size_of_programdata_metadata().saturating_sub(header.len())
            )
        ];
        header.append(&mut complement);
        header.append(&mut deploy_tx.program_data.clone());
        account_data.set_data_from_slice(&header);
        db.write(program_data_account, account_data);
    }
}

#[cfg(test)]
mod test_deploy_tx {
    #[test]
    fn test_deploy_tx() {
        let deploy_tx = super::DeployTx::new(vec![1, 2, 3], 0);
        // assert_eq!(deploy_tx.get_prog_pubkey(), super::Pubkey::new_unique());
        println!("{:?}", deploy_tx.get_prog_pubkey());
    }
}
