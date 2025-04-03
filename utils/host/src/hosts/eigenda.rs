use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use kona_preimage::BidirectionalChannel;
use op_succinct_client_utils::{InMemoryOracle, StoreOracle};
use serde::Serialize;

use crate::fetcher::OPSuccinctDataFetcher;
use crate::hosts::OPSuccinctHost;
use anyhow::Result;

use hokulea_host_bin::cfg::SingleChainHostWithEigenDA;
use op_succinct_client_utils::precompiles::zkvm_handle_register;
use kona_preimage::{HintWriter, NativeChannel, OracleReader};

//use op_succinct_client_utils::client::run_opsuccinct_client;
use op_succinct_client_utils::core_client::run_opsuccinct_core_client;
use hokulea_client_bin::witness::OracleEigenDAWitnessProvider;
use hokulea_proof::eigenda_blob_witness::{self, EigenDABlobWitnessData};
use hokulea_proof::eigenda_provider::OracleEigenDAProvider;
use std::sync::Mutex;
use std::ops::DerefMut;

#[derive(Clone)]
pub struct EigenDAOPSuccinctHost {
    pub fetcher: Arc<OPSuccinctDataFetcher>,    
}

#[async_trait]
impl OPSuccinctHost for EigenDAOPSuccinctHost {
    type Args = SingleChainHostWithEigenDA;

    async fn run(&self, args: &Self::Args) -> Result<(InMemoryOracle, Option<Vec<u8>>)> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = args.start_server(hint.host, preimage.host).await?;        
        let (in_memory_oracle, eigenda_wit) = Self::run_eigenda_witnessgen_client(preimage.client, hint.client).await?;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both tasks to complete.
        server_task.abort();

        let eigenda_wit_bytes= serde_json::to_vec(&eigenda_wit)?;

        Ok((in_memory_oracle, Some(eigenda_wit_bytes)))
    }

    async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
        safe_db_fallback: Option<bool>,
    ) -> Result<SingleChainHostWithEigenDA> {
        let host = self
            .fetcher
            .get_host_args(
                l2_start_block,
                l2_end_block,
                l1_head_hash,
                safe_db_fallback.expect("`safe_db_fallback` must be set"),
            )
            .await?;

        let eigenda_proxy_address = std::env::var("EIGENDA_PROXY_ADDRESS").ok();
        Ok(SingleChainHostWithEigenDA{
            kona_cfg: host,
            eigenda_proxy_address: eigenda_proxy_address,
        })
    }

    fn get_l1_head_hash(&self, args: &Self::Args) -> Option<B256> {
        Some(args.kona_cfg.l1_head)
    }
}

impl EigenDAOPSuccinctHost {
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        Self { fetcher }
    }

    /// Run the witness generation client.
    async fn run_eigenda_witnessgen_client(        
        preimage_chan: NativeChannel,
        hint_chan: NativeChannel,
    ) -> Result<(InMemoryOracle, EigenDABlobWitnessData)> {
        let oracle = Arc::new(StoreOracle::new(
            OracleReader::new(preimage_chan),
            HintWriter::new(hint_chan),
        ));

        let eigenda_blob_provider = OracleEigenDAProvider::new(oracle.clone());
        let eigenda_blobs_witness = Arc::new(Mutex::new(EigenDABlobWitnessData::default()));

        let eigenda_blob_and_witness_provider = OracleEigenDAWitnessProvider {
            provider: eigenda_blob_provider,
            witness: eigenda_blobs_witness.clone(),
        };

        run_opsuccinct_core_client(oracle.clone(), eigenda_blob_and_witness_provider, Some(zkvm_handle_register)).await?;

        let wit = core::mem::take(eigenda_blobs_witness.lock().unwrap().deref_mut());

        let in_memory_oracle = InMemoryOracle::populate_from_store(oracle.as_ref())?;
        Ok((in_memory_oracle, wit))
    }
}