use std::{error::Error, str::FromStr, sync::Arc, time::Duration};

use ethers::{
    addressbook::Address,
    middleware::MiddlewareError,
    prelude::{BlockNumber, JsonRpcError, ValueOrArray, H256, U64},
};
use regex::Regex;
use tokio::{
    sync::{mpsc, Semaphore},
    time::Instant,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn};

use crate::{
    event::{config::EventProcessingConfig, RindexerEventFilter},
    indexer::{log_helpers::is_relevant_block, IndexingEventProgressStatus},
    provider::{JsonRpcCachedProvider, WrappedLog},
};

pub struct FetchLogsResult {
    pub logs: Vec<WrappedLog>,
    pub from_block: U64,
    pub to_block: U64,
}

pub fn fetch_logs_stream(
    config: Arc<EventProcessingConfig>,
    force_no_live_indexing: bool,
) -> impl tokio_stream::Stream<Item = Result<FetchLogsResult, Box<dyn Error + Send>>> + Send + Unpin
{
    let (tx, rx) = mpsc::unbounded_channel();

    let initial_filter = config.to_event_filter().unwrap();
    let contract_address = initial_filter.contract_address();

    tokio::spawn(async move {
        let snapshot_to_block = initial_filter.get_to_block();
        let from_block = initial_filter.get_from_block();
        let mut current_filter = initial_filter;

        // add any max block range limitation before we start processing
        let mut max_block_range_limitation =
            config.network_contract.cached_provider.max_block_range;
        if max_block_range_limitation.is_some() {
            current_filter = current_filter.set_to_block(calculate_process_historic_log_to_block(
                &from_block,
                &snapshot_to_block,
                &max_block_range_limitation,
            ));
            debug!(
                indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                log_name = %config.info_log_name,
                max_block_range_limit = ?max_block_range_limitation,
                "max block range limitation applied - block range indexing will be slower then RPC providers supplying the optimal ranges - https://rindexer.xyz/docs/references/rpc-node-providers#rpc-node-providers"
            );
        }
        while current_filter.get_from_block() <= snapshot_to_block {
            let semaphore_client = Arc::clone(&config.semaphore);
            let permit = semaphore_client.acquire_owned().await;

            match permit {
                Ok(permit) => {
                    let result = fetch_historic_logs_stream(
                        &config.network_contract.cached_provider,
                        &tx,
                        &config.topic_id,
                        current_filter.clone(),
                        max_block_range_limitation,
                        snapshot_to_block,
                        &config.info_log_name,
                    )
                    .await;

                    drop(permit);

                    // slow indexing warn user
                    if let Some(range) = max_block_range_limitation {
                        debug!(
                            log_name = %config.info_log_name,
                            indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                            block_range_limit = %range,
                            "RPC PROVIDER IS SLOW - Slow indexing mode enabled, max block range limitation - we advise using a faster provider who can predict the next block ranges."
                        );
                    }

                    if let Some(result) = result {
                        current_filter = result.next;
                        max_block_range_limitation = result.max_block_range_limitation;
                    } else {
                        break;
                    }
                }
                Err(e) => {
                    error!(
                        log_name = %config.info_log_name,
                        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                        error = %e,
                        "Semaphore error"
                    );
                    continue;
                }
            }
        }

        info!(
            log_name = %config.info_log_name,
            indexing_status = %IndexingEventProgressStatus::Completed.log(),
            "Finished indexing historic events"
        );

        // Live indexing mode
        if config.live_indexing && !force_no_live_indexing {
            live_indexing_stream(
                &config.network_contract.cached_provider,
                &tx,
                &contract_address,
                &config.topic_id,
                &config.indexing_distance_from_head,
                current_filter,
                &config.info_log_name,
                &config.semaphore,
                config.network_contract.disable_logs_bloom_checks,
            )
            .await;
        }
    });

    UnboundedReceiverStream::new(rx)
}

struct ProcessHistoricLogsStreamResult {
    pub next: RindexerEventFilter,
    pub max_block_range_limitation: Option<U64>,
}

async fn fetch_historic_logs_stream(
    cached_provider: &Arc<JsonRpcCachedProvider>,
    tx: &mpsc::UnboundedSender<Result<FetchLogsResult, Box<dyn Error + Send>>>,
    topic_id: &H256,
    current_filter: RindexerEventFilter,
    mut max_block_range_limitation: Option<U64>,
    snapshot_to_block: U64,
    info_log_name: &str,
) -> Option<ProcessHistoricLogsStreamResult> {
    let from_block = current_filter.get_from_block();
    let to_block = current_filter.get_to_block();
    let block_range = to_block - from_block + 1u64;

    debug!(
        log_name = %info_log_name,
        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
        from_block = %from_block,
        to_block = %to_block,
        block_range = %block_range,
        "Process historic events - blocks"
    );

    if from_block > to_block {
        debug!(
            log_name = %info_log_name,
            indexing_status = %IndexingEventProgressStatus::Syncing.log(),
            from_block = %from_block,
            to_block = %to_block,
            "from_block > to_block, skipping range"
        );
        return Some(ProcessHistoricLogsStreamResult {
            next: current_filter.set_from_block(to_block),
            max_block_range_limitation,
        });
    }

    debug!(
        log_name = %info_log_name,
        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
        filter = ?current_filter,
        "Processing filter"
    );

    match cached_provider.get_logs(&current_filter).await {
        Ok(logs) => {
            debug!(
                log_name = %info_log_name,
                indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                topic_id = %topic_id,
                log_count = logs.len(),
                from_block = %from_block,
                to_block = %to_block,
                "Fetched logs for block range"
            );

            info!(
                log_name = %info_log_name,
                indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                log_count = logs.len(),
                from_block = %from_block,
                to_block = %to_block,
                block_range = %block_range,
                "Fetched event logs"
            );

            let logs_empty = logs.is_empty();
            // clone here over the full logs way less overhead
            let last_log = logs.last().cloned();

            if tx.send(Ok(FetchLogsResult { logs, from_block, to_block })).is_err() {
                error!(
                    indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                    log_name = %info_log_name,
                    "Failed to send logs to stream consumer!"
                );
                return None;
            }

            max_block_range_limitation = increase_max_block_range(max_block_range_limitation);

            if logs_empty {
                info!(
                    log_name = %info_log_name,
                    from_block = %from_block,
                    to_block = %to_block,
                    "No events found between blocks"
                );
                let next_from_block = to_block + 1;
                return if next_from_block > snapshot_to_block {
                    None
                } else {
                    let new_to_block = calculate_process_historic_log_to_block(
                        &next_from_block,
                        &snapshot_to_block,
                        &max_block_range_limitation,
                    );

                    debug!(
                        log_name = %info_log_name,
                        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                        new_from_block = %next_from_block,
                        new_to_block = %new_to_block,
                        "Moving to next block range"
                    );

                    Some(ProcessHistoricLogsStreamResult {
                        next: current_filter
                            .set_from_block(next_from_block)
                            .set_to_block(new_to_block),
                        max_block_range_limitation,
                    })
                };
            }

            if let Some(last_log) = last_log {
                let next_from_block = last_log
                    .inner
                    .block_number
                    .expect("block number should always be present in a log") +
                    U64::from(1);
                debug!(
                    log_name = %info_log_name,
                    indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                    next_block = %next_from_block,
                    "Next block to start from"
                );

                return if next_from_block > snapshot_to_block {
                    None
                } else {
                    let new_to_block = calculate_process_historic_log_to_block(
                        &next_from_block,
                        &snapshot_to_block,
                        &max_block_range_limitation,
                    );

                    debug!(
                        "{} - {} - new_from_block {:?} new_to_block {:?}",
                        info_log_name,
                        IndexingEventProgressStatus::Syncing.log(),
                        next_from_block,
                        new_to_block
                    );

                    Some(ProcessHistoricLogsStreamResult {
                        next: current_filter
                            .set_from_block(next_from_block)
                            .set_to_block(new_to_block),
                        max_block_range_limitation,
                    })
                };
            }
        }
        Err(err) => {
            return handle_get_logs_error(
                err,
                tx,
                current_filter,
                max_block_range_limitation,
                info_log_name,
            );
        }
    }

    None
}

/// Handles live indexing mode, continuously checking for new blocks, ensuring they are
/// within a safe range, updating the filter, and sending the logs to the provided channel.
#[allow(clippy::too_many_arguments)]
async fn live_indexing_stream(
    cached_provider: &Arc<JsonRpcCachedProvider>,
    tx: &mpsc::UnboundedSender<Result<FetchLogsResult, Box<dyn Error + Send>>>,
    contract_address: &Option<ValueOrArray<Address>>,
    topic_id: &H256,
    reorg_safe_distance: &U64,
    mut current_filter: RindexerEventFilter,
    info_log_name: &str,
    semaphore: &Arc<Semaphore>,
    disable_logs_bloom_checks: bool,
) {
    let mut last_seen_block_number = U64::from(0);

    // this is used for less busy chains to make sure they know rindexer is still alive
    let mut last_no_new_block_log_time = Instant::now();
    let log_no_new_block_interval = Duration::from_secs(300);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let latest_block = cached_provider.get_latest_block().await;
        match latest_block {
            Ok(latest_block) => {
                if let Some(latest_block) = latest_block {
                    if let Some(latest_block_number) = latest_block.number {
                        if last_seen_block_number == latest_block_number {
                            debug!(
                                log_name = %info_log_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                "No new blocks to process..."
                            );
                            if last_no_new_block_log_time.elapsed() >= log_no_new_block_interval {
                                info!(
                                    log_name = %info_log_name,
                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                    latest_block_number = %last_seen_block_number,
                                    "No new blocks published in the last 5 minutes - latest block number"
                                );
                                last_no_new_block_log_time = Instant::now();
                            }
                            continue;
                        }
                        debug!(
                            log_name = %info_log_name,
                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                            latest_block = %latest_block_number,
                            last_seen_block = %last_seen_block_number,
                            "New block seen - Last seen block"
                        );

                        let safe_block_number = latest_block_number - reorg_safe_distance;
                        let from_block = current_filter.get_from_block();
                        // check reorg distance and skip if not safe
                        if from_block > safe_block_number {
                            info!(
                                log_name = %info_log_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                current_block = %from_block,
                                safe_block_range = %safe_block_number,
                                "Not in safe reorg block range yet - current block > safe reorg range"
                            );
                            continue;
                        }

                        let to_block = safe_block_number;
                        if from_block == to_block &&
                            !disable_logs_bloom_checks &&
                            !is_relevant_block(contract_address, topic_id, &latest_block)
                        {
                            debug!(
                                log_name = %info_log_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                block_number = %from_block,
                                "Skipping block as it's not relevant"
                            );
                            debug!(
                                log_name = %info_log_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                block_number = %from_block,
                                "Did not need to hit RPC as no events in block - LogsBloom for block checked"
                            );
                            current_filter = current_filter.set_from_block(to_block + 1);
                            last_seen_block_number = to_block;
                            continue;
                        }

                        current_filter = current_filter.set_to_block(to_block);
                        let block_range = to_block - from_block + 1u64;

                        debug!(
                            log_name = %info_log_name,
                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                            filter = ?current_filter,
                            from_block = %from_block,
                            to_block = %to_block,
                            block_range = %block_range,
                            "Processing live filter"
                        );

                        let semaphore_client = Arc::clone(semaphore);
                        let permit = semaphore_client.acquire_owned().await;

                        if let Ok(permit) = permit {
                            match cached_provider.get_logs(&current_filter).await {
                                Ok(logs) => {
                                    debug!(
                                        log_name = %info_log_name,
                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                        topic_id = %topic_id,
                                        log_count = logs.len(),
                                        from_block = %from_block,
                                        to_block = %to_block,
                                        "Fetched logs for live block range"
                                    );
                                    info!(
                                        log_name = %info_log_name,
                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                        log_count = logs.len(),
                                        from_block = %from_block,
                                        to_block = %to_block,
                                        block_range = %block_range,
                                        "Fetched event logs for live block range"
                                    );

                                    last_seen_block_number = to_block;

                                    let logs_empty = logs.is_empty();
                                    // clone here over the full logs way less overhead
                                    let last_log = logs.last().cloned();

                                    if tx
                                        .send(Ok(FetchLogsResult { logs, from_block, to_block }))
                                        .is_err()
                                    {
                                        error!(
                                            log_name = %info_log_name,
                                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                                            "Failed to send logs to stream consumer!"
                                        );
                                        drop(permit);
                                        break;
                                    }

                                    if logs_empty {
                                        current_filter =
                                            current_filter.set_from_block(to_block + 1);
                                        info!(
                                            log_name = %info_log_name,
                                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                                            from_block = %from_block,
                                            to_block = %to_block,
                                            "No events found between blocks in live indexing"
                                        );
                                    } else if let Some(last_log) = last_log {
                                        if let Some(last_log_block_number) =
                                            last_log.inner.block_number
                                        {
                                            current_filter = current_filter.set_from_block(
                                                last_log_block_number + U64::from(1),
                                            );
                                        } else {
                                            error!("Failed to get last log block number the provider returned null (should never happen) - try again in 200ms");
                                        }
                                    }

                                    drop(permit);
                                }
                                Err(err) => {
                                    error!(
                                        log_name = %info_log_name,
                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                        error = %err,
                                        from_block = %from_block,
                                        to_block = %to_block,
                                        "Error fetching logs for live indexing"
                                    );
                                    drop(permit);
                                }
                            }
                        }
                    } else {
                        info!("WARNING - empty latest block returned from provider, will try again in 200ms");
                    }
                } else {
                    info!("WARNING - empty latest block returned from provider, will try again in 200ms");
                }
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Error getting latest block, will try again in 1 seconds"
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
    }
}

#[derive(Debug)]
struct RetryWithBlockRangeResult {
    from: BlockNumber,
    to: BlockNumber,
    // This is only populated if you are using an RPC provider
    // who doesn't give block ranges, this tends to be providers
    // which are a lot slower than others, expect these providers
    // to be slow
    max_block_range: Option<U64>,
}

/// Attempts to retry with a new block range based on the error message.
fn retry_with_block_range(
    error: &JsonRpcError,
    from_block: U64,
    to_block: U64,
) -> Option<RetryWithBlockRangeResult> {
    let error_message = &error.message;
    // some providers put the data in the data field
    let error_data_binding = error.data.as_ref().map(|data| data.to_string());
    let empty_string = String::from("");
    let error_data = match &error_data_binding {
        Some(data) => data,
        None => &empty_string,
    };

    fn compile_regex(pattern: &str) -> Result<Regex, regex::Error> {
        Regex::new(pattern)
    }

    // Thanks Ponder for the regex patterns - https://github.com/ponder-sh/ponder/blob/889096a3ef5f54a0c5a06df82b0da9cf9a113996/packages/utils/src/getLogsRetryHelper.ts#L34

    // Alchemy
    if let Ok(re) =
        compile_regex(r"this block range should work: \[(0x[0-9a-fA-F]+),\s*(0x[0-9a-fA-F]+)]")
    {
        if let Some(captures) = re.captures(error_message).or_else(|| re.captures(error_data)) {
            if let (Some(start_block), Some(end_block)) = (captures.get(1), captures.get(2)) {
                let start_block_str = start_block.as_str();
                let end_block_str = end_block.as_str();
                if let (Ok(from), Ok(to)) =
                    (BlockNumber::from_str(start_block_str), BlockNumber::from_str(end_block_str))
                {
                    return Some(RetryWithBlockRangeResult { from, to, max_block_range: None });
                }
            }
        }
    }

    // Infura, Thirdweb, zkSync, Tenderly
    if let Ok(re) =
        compile_regex(r"Try with this block range \[0x([0-9a-fA-F]+),\s*0x([0-9a-fA-F]+)\]")
    {
        if let Some(captures) = re.captures(error_message).or_else(|| {
            let blah = re.captures(error_data);
            blah
        }) {
            if let (Some(start_block), Some(end_block)) = (captures.get(1), captures.get(2)) {
                let start_block_str = format!("0x{}", start_block.as_str());
                let end_block_str = format!("0x{}", end_block.as_str());
                if let (Ok(from), Ok(to)) =
                    (BlockNumber::from_str(&start_block_str), BlockNumber::from_str(&end_block_str))
                {
                    return Some(RetryWithBlockRangeResult { from, to, max_block_range: None });
                }
            }
        }
    }

    // Ankr
    if error_message.contains("block range is too wide") && error.code == -32600 {
        return Some(RetryWithBlockRangeResult {
            from: BlockNumber::from(from_block),
            to: BlockNumber::from(from_block + 3000),
            max_block_range: Some(3000.into()),
        });
    }

    // QuickNode, 1RPC, zkEVM, Blast, BlockPI
    if let Ok(re) = compile_regex(r"limited to a ([\d,.]+)") {
        if let Some(captures) = re.captures(error_message).or_else(|| re.captures(error_data)) {
            if let Some(range_str_match) = captures.get(1) {
                let range_str = range_str_match.as_str().replace(&['.', ','][..], "");
                if let Ok(range) = U64::from_dec_str(&range_str) {
                    return Some(RetryWithBlockRangeResult {
                        from: BlockNumber::from(from_block),
                        to: BlockNumber::from(from_block + range),
                        max_block_range: Some(range),
                    });
                }
            }
        }
    }

    // Base
    if error_message.contains("block range too large") {
        return Some(RetryWithBlockRangeResult {
            from: BlockNumber::from(from_block),
            to: BlockNumber::from(from_block + 2000),
            max_block_range: Some(2000.into()),
        });
    }

    // Fallback range
    if to_block > from_block {
        let fallback_range = (to_block - from_block) / 2;
        return Some(RetryWithBlockRangeResult {
            from: BlockNumber::from(from_block),
            to: BlockNumber::from(from_block + fallback_range),
            max_block_range: Some(fallback_range),
        });
    }

    None
}

fn calculate_process_historic_log_to_block(
    new_from_block: &U64,
    snapshot_to_block: &U64,
    max_block_range_limitation: &Option<U64>,
) -> U64 {
    if let Some(max_block_range_limitation) = max_block_range_limitation {
        let to_block = new_from_block + max_block_range_limitation;
        if to_block > *snapshot_to_block {
            *snapshot_to_block
        } else {
            to_block
        }
    } else {
        *snapshot_to_block
    }
}

fn handle_get_logs_error(
    err: ethers::providers::ProviderError,
    tx: &mpsc::UnboundedSender<Result<FetchLogsResult, Box<dyn Error + Send>>>,
    current_filter: RindexerEventFilter,
    mut max_block_range_limitation: Option<U64>,
    info_log_name: &str,
) -> Option<ProcessHistoricLogsStreamResult> {
    let from_block = current_filter.get_from_block();
    let to_block = current_filter.get_to_block();
    let block_range = to_block - from_block + 1u64;

    if let Some(json_rpc_error) = err.as_error_response() {
        if let Some(retry_result) = retry_with_block_range(json_rpc_error, from_block, to_block) {
            warn!(
                log_name = %info_log_name,
                indexing_status = %IndexingEventProgressStatus::Syncing.log(),
                retry_from_block = ?retry_result.from,
                retry_to_block = ?retry_result.to,
                retry_max_block_range = ?retry_result.max_block_range,
                "Retrying with block range"
            );

            max_block_range_limitation = decrease_max_block_range(max_block_range_limitation);

            return Some(ProcessHistoricLogsStreamResult {
                next: current_filter
                    .set_from_block(retry_result.from)
                    .set_to_block(retry_result.to),
                max_block_range_limitation,
            });
        }
    }
    warn!(
        log_name = %info_log_name,
        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
        error = %err,
        from_block = %from_block,
        to_block = %to_block,
        block_range = %block_range,
        "Error encountered, retrying with adjusted block range."
    );

    // Retry with half the max_block_range_limitation, if available.
    if let Some(max_range) = max_block_range_limitation {
        let new_range = max_range / U64::from(2);
        let new_max_block_range_limitation = decrease_max_block_range(Some(new_range));

        let new_to_block = std::cmp::min(from_block + new_range, to_block); // Ensure we don't exceed original to_block

        // if the new range is 0 then use the same filter
        if new_range == U64::from(0) {
            return Some(ProcessHistoricLogsStreamResult {
                next: current_filter.clone(), // Retry with the same filter
                max_block_range_limitation: new_max_block_range_limitation,
            });
        }

        return Some(ProcessHistoricLogsStreamResult {
            next: current_filter.set_to_block(new_to_block),
            max_block_range_limitation: new_max_block_range_limitation,
        });
    } else {
        // if no max range is set, then retry with the same filter again
        return Some(ProcessHistoricLogsStreamResult {
            next: current_filter.clone(), // Retry with the same filter
            max_block_range_limitation,
        });
    }

    error!(
        log_name = %info_log_name,
        indexing_status = %IndexingEventProgressStatus::Syncing.log(),
        error = %err,
        from_block = %from_block,
        to_block = %to_block,
        "Error fetching logs"
    );
    let _ = tx.send(Err(Box::new(err)));
    None
}

fn increase_max_block_range(current_max_block_range: Option<U64>) -> Option<U64> {
    let current_range = current_max_block_range.unwrap_or(U64::from(100)); // Default starting range if None
    let increased_range = std::cmp::min(current_range * U64::from(2), U64::from(10000));
    Some(std::cmp::max(increased_range, U64::from(10))) // Ensure minimum range is 10
}

fn decrease_max_block_range(current_max_block_range: Option<U64>) -> Option<U64> {
    match current_max_block_range {
        Some(current_range) => {
            let decreased_range = std::cmp::max(current_range / U64::from(2), U64::from(10));
            Some(decreased_range)
        }
        None => None, // If no limit was initially set, we don't introduce one on failure.
    }
}
