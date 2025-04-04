use alloy_primitives::Sealed;
use anyhow::anyhow;
use anyhow::Result;
use kona_driver::Driver;
use kona_executor::{KonaHandleRegister, TrieDBProvider};
use kona_preimage::CommsClient;
use kona_proof::executor::KonaExecutor;
use kona_proof::l1::OracleL1ChainProvider; //, OraclePipeline};
use kona_proof::l2::OracleL2ChainProvider;
use kona_proof::sync::new_pipeline_cursor;
use kona_proof::{BootInfo, FlushableCache};
use std::fmt::Debug;
use std::sync::Arc;
use tracing::info;

use crate::oracle::OPSuccinctOracleBlobProvider;
use hokulea_eigenda::EigenDABlobProvider;
use hokulea_proof::pipeline::OraclePipeline;
use crate::client::{fetch_safe_head_hash, advance_to_target};

pub async fn run_opsuccinct_core_client<O, E>(
    oracle: Arc<O>,    
    eigenda: E,    
    handle_register: Option<KonaHandleRegister<OracleL2ChainProvider<O>, OracleL2ChainProvider<O>>>,
) -> Result<BootInfo>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug + Clone,
    E: EigenDABlobProvider + Send + Sync + Debug + Clone,
{
    ////////////////////////////////////////////////////////////////
    //                          PROLOGUE                          //
    ////////////////////////////////////////////////////////////////

    let boot = match BootInfo::load(oracle.as_ref()).await {
        Ok(boot) => boot,
        Err(e) => {
            return Err(anyhow!("Failed to load boot info: {:?}", e));
        }
    };

    let boot_clone = boot.clone();

    let rollup_config = Arc::new(boot.rollup_config);
    let safe_head_hash = fetch_safe_head_hash(oracle.as_ref(), boot.agreed_l2_output_root).await?;

    let mut l1_provider = OracleL1ChainProvider::new(boot.l1_head, oracle.clone());
    let mut l2_provider =
        OracleL2ChainProvider::new(safe_head_hash, rollup_config.clone(), oracle.clone());
    let beacon = OPSuccinctOracleBlobProvider::new(oracle.clone());

    // Fetch the safe head's block header.
    let safe_head = l2_provider
        .header_by_hash(safe_head_hash)
        .map(|header| Sealed::new_unchecked(header, safe_head_hash))?;

    // If the claimed L2 block number is less than the safe head of the L2 chain, the claim is
    // invalid.
    if boot.claimed_l2_block_number < safe_head.number {
        return Err(anyhow!(
            "Claimed L2 block number {claimed} is less than the safe head {safe}",
            claimed = boot.claimed_l2_block_number,
            safe = safe_head.number
        ));
    }

    // In the case where the agreed upon L2 output root is the same as the claimed L2 output root,
    // trace extension is detected and we can skip the derivation and execution steps.
    if boot.agreed_l2_output_root == boot.claimed_l2_output_root {
        info!(
            target: "client",
            "Trace extension detected. State transition is already agreed upon.",
        );
        return Ok(boot_clone);
    }
    ////////////////////////////////////////////////////////////////
    //                   DERIVATION & EXECUTION                   //
    ////////////////////////////////////////////////////////////////

    // Create a new derivation driver with the given boot information and oracle.
    let cursor = new_pipeline_cursor(
        rollup_config.as_ref(),
        safe_head,
        &mut l1_provider,
        &mut l2_provider,
    )
    .await?;
    l2_provider.set_cursor(cursor.clone());

    let pipeline = OraclePipeline::new(
        rollup_config.clone(),
        cursor.clone(),
        oracle.clone(),
        beacon,
        l1_provider.clone(),
        l2_provider.clone(),
        eigenda,
    )
    .await?;

    let executor = KonaExecutor::new(
        &rollup_config,
        l2_provider.clone(),
        l2_provider,
        handle_register,
        None,
    );
    let mut driver = Driver::new(cursor, executor, pipeline);
    // Run the derivation pipeline until we are able to produce the output root of the claimed
    // L2 block.

    // Use custom advance to target with cycle tracking.
    #[cfg(target_os = "zkvm")]
    println!("cycle-tracker-report-start: block-execution-and-derivation");
    let (safe_head, output_root) = advance_to_target(
        &mut driver,
        rollup_config.as_ref(),
        Some(boot.claimed_l2_block_number),
    )
    .await?;
    #[cfg(target_os = "zkvm")]
    println!("cycle-tracker-report-end: block-execution-and-derivation");

    ////////////////////////////////////////////////////////////////
    //                          EPILOGUE                          //
    ////////////////////////////////////////////////////////////////

    if output_root != boot.claimed_l2_output_root {
        return Err(anyhow!(
            "Failed to validate L2 block #{number} with claimed output root {claimed_output_root}. Got {output_root} instead",
            number = safe_head.block_info.number,
            output_root = output_root,
            claimed_output_root = boot.claimed_l2_output_root,
        ));
    }

    info!(
        target: "client",
        "Successfully validated L2 block #{number} with output root {output_root}",
        number = safe_head.block_info.number,
        output_root = output_root
    );

    #[cfg(target_os = "zkvm")]
    {
        std::mem::forget(driver);
        std::mem::forget(l1_provider);
        std::mem::forget(oracle);
        std::mem::forget(rollup_config);
    }

    Ok(boot_clone)
}