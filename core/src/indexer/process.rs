use std::{collections::HashMap, sync::Arc, time::Duration};

use async_std::prelude::StreamExt;
use ethers::{
    prelude::ProviderError,
    types::{H256, U64},
};
use futures::future::join_all;
use tokio::{
    sync::{Mutex, MutexGuard},
    task::{JoinError, JoinHandle},
    time::{sleep, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    event::{
        callback_registry::EventResult, config::EventProcessingConfig, BuildRindexerFilterError,
        RindexerEventFilter,
    },
    indexer::{
        dependency::{ContractEventsDependenciesConfig, EventDependencies},
        fetch_logs::{fetch_logs_stream, FetchLogsResult},
        last_synced::update_progress_and_last_synced_task,
        log_helpers::is_relevant_block,
        progress::IndexingEventProgressStatus,
        task_tracker::{indexing_event_processed, indexing_event_processing},
    },
    is_running,
};

#[derive(thiserror::Error, Debug)]
pub enum ProcessEventError {
    #[error("Could not process logs: {0}")]
    ProcessLogs(#[from] Box<ProviderError>),

    #[error("Could not build filter: {0}")]
    BuildFilterError(#[from] BuildRindexerFilterError),
}

pub async fn process_event(
    config: EventProcessingConfig,
    block_until_indexed: bool,
) -> Result<(), ProcessEventError> {
    info!(
        event_name = %config.event_name,
        contract_name = %config.contract_name,
        "Processing events"
    );

    process_event_logs(Arc::new(config), false, block_until_indexed).await?;

    Ok(())
}

/// note block_until_indexed:
/// Whether to wait for all indexing tasks to complete for an event before returning
//  (needed for dependency indexing)
async fn process_event_logs(
    config: Arc<EventProcessingConfig>,
    force_no_live_indexing: bool,
    block_until_indexed: bool,
) -> Result<(), Box<ProviderError>> {
    let mut logs_stream = fetch_logs_stream(Arc::clone(&config), force_no_live_indexing);
    let mut tasks = Vec::new();

    while let Some(result) = logs_stream.next().await {
        let task = handle_logs_result(Arc::clone(&config), result)
            .await
            .map_err(|e| Box::new(ProviderError::CustomError(e.to_string())))?;

        tasks.push(task);
    }

    if block_until_indexed {
        // Wait for all tasks in parallel
        futures::future::try_join_all(tasks)
            .await
            .map_err(|e| Box::new(ProviderError::CustomError(e.to_string())))?;
    }

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum ProcessContractsEventsWithDependenciesError {
    #[error("{0}")]
    ProcessContractEventsWithDependenciesError(#[from] ProcessContractEventsWithDependenciesError),

    #[error("{0}")]
    JoinError(#[from] JoinError),
}

pub async fn process_contracts_events_with_dependencies(
    contracts_events_config: Vec<ContractEventsDependenciesConfig>,
) -> Result<(), ProcessContractsEventsWithDependenciesError> {
    let mut handles: Vec<JoinHandle<Result<(), ProcessContractEventsWithDependenciesError>>> =
        Vec::new();

    for contract_events in contracts_events_config {
        let handle = tokio::spawn(async move {
            process_contract_events_with_dependencies(
                contract_events.event_dependencies,
                Arc::new(contract_events.events_config),
            )
            .await
        });
        handles.push(handle);
    }

    let results = join_all(handles).await;

    for result in results {
        match result {
            Ok(inner_result) => inner_result?,
            Err(join_error) => {
                return Err(ProcessContractsEventsWithDependenciesError::JoinError(join_error))
            }
        }
    }

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum ProcessContractEventsWithDependenciesError {
    #[error("Could not process logs: {0}")]
    ProcessLogs(#[from] Box<ProviderError>),

    #[error("Could not build filter: {0}")]
    BuildFilterError(#[from] BuildRindexerFilterError),

    #[error("Event config not found for contract: {0} and event: {1}")]
    EventConfigNotFound(String, String),

    #[error("Could not run all the logs processes {0}")]
    JoinError(#[from] JoinError),
}

#[derive(Debug, Clone)]
pub struct OrderedLiveIndexingDetails {
    pub filter: RindexerEventFilter,
    pub last_seen_block_number: U64,
    pub last_no_new_block_log_time: Instant,
}

async fn process_contract_events_with_dependencies(
    dependencies: EventDependencies,
    events_processing_config: Arc<Vec<Arc<EventProcessingConfig>>>,
) -> Result<(), ProcessContractEventsWithDependenciesError> {
    let mut stack = vec![dependencies.tree];

    let live_indexing_events =
        Arc::new(Mutex::new(Vec::<(Arc<EventProcessingConfig>, RindexerEventFilter)>::new()));

    while let Some(current_tree) = stack.pop() {
        let mut tasks = vec![];

        for dependency in &current_tree.contract_events {
            let event_processing_config = Arc::clone(&events_processing_config);
            let dependency = dependency.clone();
            let live_indexing_events = Arc::clone(&live_indexing_events);

            let task = tokio::spawn(async move {
                let event_processing_config = event_processing_config
                    .iter()
                    .find(|e| {
                        // TODO - this is a hacky way to check if it's a filter event
                        (e.contract_name == dependency.contract_name ||
                            e.contract_name.replace("Filter", "") == dependency.contract_name) &&
                            e.event_name == dependency.event_name
                    })
                    .ok_or(ProcessContractEventsWithDependenciesError::EventConfigNotFound(
                        dependency.contract_name,
                        dependency.event_name,
                    ))?;

                // forces live indexing off as it has to handle it a bit differently
                process_event_logs(Arc::clone(event_processing_config), true, true).await?;

                if event_processing_config.live_indexing {
                    let rindexer_event_filter = event_processing_config.to_event_filter()?;
                    live_indexing_events
                        .lock()
                        .await
                        .push((Arc::clone(event_processing_config), rindexer_event_filter));
                }

                Ok::<(), ProcessContractEventsWithDependenciesError>(())
            });

            tasks.push(task);
        }

        let results = join_all(tasks).await;
        for result in results {
            match result {
                Ok(result) => match result {
                    Ok(_) => {}
                    Err(e) => {
                        error!(error = %e, "Error processing logs due to dependencies error");
                        return Err(e);
                    }
                },
                Err(e) => {
                    error!(error = %e, "Error processing logs");
                    return Err(ProcessContractEventsWithDependenciesError::JoinError(e));
                }
            }
        }

        // If there are more dependencies to process, push the next level onto the stack
        if let Some(next_tree) = &*current_tree.then {
            stack.push(Arc::clone(next_tree));
        }
    }

    let live_indexing_events = live_indexing_events.lock().await;
    if live_indexing_events.is_empty() {
        return Ok(());
    }

    live_indexing_for_contract_event_dependencies(&live_indexing_events).await;

    Ok(())
}

// TODO - this is a similar to live_indexing_stream but has to be a bit different we should merge
// code
#[allow(clippy::type_complexity)]
async fn live_indexing_for_contract_event_dependencies<'a>(
    live_indexing_events: &'a MutexGuard<
        'a,
        Vec<(Arc<EventProcessingConfig>, RindexerEventFilter)>,
    >,
) {
    let mut ordering_live_indexing_details_map: HashMap<
        H256,
        Arc<Mutex<OrderedLiveIndexingDetails>>,
    > = HashMap::new();

    for (config, event_filter) in live_indexing_events.iter() {
        let mut filter = event_filter.clone();
        let last_seen_block_number = filter.get_to_block();
        let next_block_number = last_seen_block_number + 1;

        filter = filter.set_from_block(next_block_number).set_to_block(next_block_number);

        ordering_live_indexing_details_map.insert(
            config.topic_id,
            Arc::new(Mutex::new(OrderedLiveIndexingDetails {
                filter,
                last_seen_block_number,
                last_no_new_block_log_time: Instant::now(),
            })),
        );
    }

    // this is used for less busy chains to make sure they know rindexer is still alive
    let log_no_new_block_interval = Duration::from_secs(300);
    let mut error_retry_delay = Duration::from_millis(200); // Initial retry delay
    let max_retry_delay = Duration::from_secs(60); // Maximum retry delay

    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        for (config, _) in live_indexing_events.iter() {
            let mut ordering_live_indexing_details = ordering_live_indexing_details_map
                .get(&config.topic_id)
                .expect("Failed to get ordering_live_indexing_details_map")
                .lock()
                .await
                .clone();

            let latest_block = &config.network_contract.cached_provider.get_latest_block().await;

            match latest_block {
                Ok(latest_block) => {
                    if let Some(latest_block) = latest_block {
                        if let Some(latest_block_number) = latest_block.number {
                            if ordering_live_indexing_details.last_seen_block_number ==
                                latest_block_number
                            {
                                debug!(
                                    event_name = %config.event_name,
                                    contract_name = %config.contract_name,
                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                    "No new blocks to process..."
                                );
                                if ordering_live_indexing_details
                                    .last_no_new_block_log_time
                                    .elapsed() >=
                                    log_no_new_block_interval
                                {
                                    info!(
                                        event_name = %config.event_name,
                                        contract_name = %config.contract_name,
                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                        latest_block_number = %latest_block_number,
                                        "No new blocks published in the last 5 minutes"
                                    );
                                    ordering_live_indexing_details.last_no_new_block_log_time =
                                        Instant::now();
                                    *ordering_live_indexing_details_map
                                        .get(&config.topic_id)
                                        .expect("Failed to get ordering_live_indexing_details_map")
                                        .lock()
                                        .await = ordering_live_indexing_details;
                                }
                                continue;
                            }
                            debug!(
                                event_name = %config.event_name,
                                contract_name = %config.contract_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                latest_block_number = %latest_block_number,
                                last_seen_block_number = %ordering_live_indexing_details.last_seen_block_number,
                                "New block seen"
                            );
                            let reorg_safe_distance = &config.indexing_distance_from_head;
                            let safe_block_number = latest_block_number - reorg_safe_distance;
                            let from_block = ordering_live_indexing_details.filter.get_from_block();
                            // check reorg distance and skip if not safe
                            if from_block > safe_block_number {
                                info!(
                                    event_name = %config.event_name,
                                    contract_name = %config.contract_name,
                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                    from_block = %from_block,
                                    safe_block_number = %safe_block_number,
                                    "not in safe reorg block range yet"
                                );
                                continue;
                            }

                            let mut to_block = safe_block_number;
                            if from_block == to_block &&
                                !config.network_contract.disable_logs_bloom_checks &&
                                !is_relevant_block(
                                    &ordering_live_indexing_details.filter.raw_filter().address,
                                    &config.topic_id,
                                    latest_block,
                                )
                            {
                                debug!(
                                    event_name = %config.event_name,
                                    contract_name = %config.contract_name,
                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                    block_number = %from_block,
                                    "Skipping block as it's not relevant"
                                );
                                debug!(
                                    event_name = %config.event_name,
                                    contract_name = %config.contract_name,
                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                    block_number = %from_block,
                                    "Did not need to hit RPC as no events in block - LogsBloom for block checked"
                                );

                                ordering_live_indexing_details.filter =
                                    ordering_live_indexing_details
                                        .filter
                                        .set_from_block(to_block + 1);

                                ordering_live_indexing_details.last_seen_block_number = to_block;
                                *ordering_live_indexing_details_map
                                    .get(&config.topic_id)
                                    .expect("Failed to get ordering_live_indexing_details_map")
                                    .lock()
                                    .await = ordering_live_indexing_details;
                                continue;
                            }

                            // Enforce max range if max_block_range is defined
                            if let Some(max_block_range) =
                                config.network_contract.cached_provider.max_block_range
                            {
                                let block_range = to_block - from_block + 1;
                                if block_range > max_block_range {
                                    to_block = from_block + max_block_range - 1;

                                    debug!(
                                        event_name = %config.event_name,
                                        contract_name = %config.contract_name,
                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                        max_block_range=  % max_block_range,
                                        from_block=%from_block,
                                        to_block=%to_block,
                                        "Block range exceeded max limit",
                                    );
                                }
                            }

                            ordering_live_indexing_details.filter =
                                ordering_live_indexing_details.filter.set_to_block(to_block);

                            debug!(
                                event_name = %config.event_name,
                                contract_name = %config.contract_name,
                                indexing_status = %IndexingEventProgressStatus::Live.log(),
                                filter = ?ordering_live_indexing_details.filter,
                                "Processing live filter"
                            );

                            let semaphore_client = Arc::clone(&config.semaphore);
                            let permit = semaphore_client.acquire_owned().await;

                            if let Ok(permit) = permit {
                                let get_logs_result = config
                                    .network_contract
                                    .cached_provider
                                    .get_logs(&ordering_live_indexing_details.filter)
                                    .await;

                                match get_logs_result {
                                    Ok(logs) => {
                                        error_retry_delay = Duration::from_millis(200); // reset delay on success
                                        debug!(
                                            event_name = %config.event_name,
                                            contract_name = %config.contract_name,
                                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                                            topic_id = %config.topic_id,
                                            logs_count = logs.len(),
                                            from_block = %from_block,
                                            to_block = %to_block,
                                            "Live topic logs fetched"
                                        );

                                        debug!(
                                            event_name = %config.event_name,
                                            contract_name = %config.contract_name,
                                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                                            logs_count = logs.len(),
                                            from_block = %from_block,
                                            to_block = %to_block,
                                            "Fetched event logs"
                                        );

                                        let logs_empty = logs.is_empty();
                                        // clone here over the full logs way less overhead
                                        let last_log = logs.last().cloned();

                                        let fetched_logs =
                                            Ok(FetchLogsResult { logs, from_block, to_block });

                                        let result =
                                            handle_logs_result(Arc::clone(config), fetched_logs)
                                                .await;

                                        match result {
                                            Ok(task) => {
                                                let complete = task.await;
                                                if let Err(e) = complete {
                                                    error!(
                                                        event_name = %config.event_name,
                                                        contract_name = %config.contract_name,
                                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                                        error = %e,
                                                        "Error indexing task - will try again in after delay"
                                                    );
                                                    drop(permit);
                                                    sleep(error_retry_delay).await; // Introduce delay before retry
                                                    error_retry_delay = (error_retry_delay * 2)
                                                        .min(max_retry_delay); // Exponential backoff
                                                    continue; // continue to next iteration with
                                                              // same block range
                                                }
                                                ordering_live_indexing_details
                                                    .last_seen_block_number = to_block;
                                                if logs_empty {
                                                    ordering_live_indexing_details.filter =
                                                        ordering_live_indexing_details
                                                            .filter
                                                            .set_from_block(to_block + 1);
                                                    info!(
                                                        event_name = %config.event_name,
                                                        contract_name = %config.contract_name,
                                                        indexing_status = %IndexingEventProgressStatus::Live.log(),
                                                        from_block = %from_block,
                                                        to_block = %to_block,
                                                        "No events found between blocks"
                                                    );
                                                } else if let Some(last_log) = last_log {
                                                    if let Some(last_log_block_number) =
                                                        last_log.inner.block_number
                                                    {
                                                        ordering_live_indexing_details.filter =
                                                            ordering_live_indexing_details
                                                                .filter
                                                                .set_from_block(
                                                                    last_log_block_number + 1,
                                                                );
                                                    } else {
                                                        error!("Failed to get last log block number the provider returned null (should never happen) - try again in after delay");
                                                        drop(permit);
                                                        sleep(error_retry_delay).await; // Introduce delay before retry
                                                        error_retry_delay = (error_retry_delay * 2)
                                                            .min(max_retry_delay); // Exponential backoff
                                                        continue; // continue to next iteration with
                                                                  // same block range
                                                    }
                                                }

                                                *ordering_live_indexing_details_map
                                                    .get(&config.topic_id)
                                                    .expect("Failed to get ordering_live_indexing_details_map")
                                                    .lock()
                                                    .await = ordering_live_indexing_details;

                                                drop(permit);
                                            }
                                            Err(err) => {
                                                error!(
                                                    event_name = %config.event_name,
                                                    contract_name = %config.contract_name,
                                                    indexing_status = %IndexingEventProgressStatus::Live.log(),
                                                    error = %err,
                                                    "Error fetching logs - will try again in after delay"
                                                );
                                                drop(permit);
                                                sleep(error_retry_delay).await; // Introduce delay before retry
                                                error_retry_delay =
                                                    (error_retry_delay * 2).min(max_retry_delay); // Exponential backoff
                                                continue; // continue to next iteration with same
                                                          // block range
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        error!(
                                            event_name = %config.event_name,
                                            contract_name = %config.contract_name,
                                            indexing_status = %IndexingEventProgressStatus::Live.log(),
                                            error = %err,
                                            "Error fetching logs - will try again in after delay"
                                        );
                                        drop(permit);
                                        sleep(error_retry_delay).await; // Introduce delay before retry
                                        error_retry_delay =
                                            (error_retry_delay * 2).min(max_retry_delay); // Exponential backoff
                                        continue; // continue to next iteration with same block
                                                  // range
                                    }
                                }
                            }
                        } else {
                            warn!("WARNING - empty latest block returned from provider, will try again in after delay");
                            sleep(error_retry_delay).await; // Introduce delay before retry
                            error_retry_delay = (error_retry_delay * 2).min(max_retry_delay); // Exponential backoff
                        }
                    } else {
                        warn!("WARNING - empty latest block returned from provider, will try again in after delay");
                        sleep(error_retry_delay).await; // Introduce delay before retry
                        error_retry_delay = (error_retry_delay * 2).min(max_retry_delay); // Exponential backoff
                    }
                }
                Err(error) => {
                    error!(
                        error = %error,
                        "Failed to get latest block, will try again in after delay"
                    );
                    sleep(error_retry_delay).await; // Introduce delay before retry
                    error_retry_delay = (error_retry_delay * 2).min(max_retry_delay); // Exponential
                                                                                      // backoff
                }
            }
        }
    }
}

async fn trigger_event(
    config: Arc<EventProcessingConfig>,
    fn_data: Vec<EventResult>,
    to_block: U64,
) {
    indexing_event_processing();
    config.trigger_event(fn_data).await;
    update_progress_and_last_synced_task(config, to_block, indexing_event_processed);
}

async fn handle_logs_result(
    config: Arc<EventProcessingConfig>,
    result: Result<FetchLogsResult, Box<dyn std::error::Error + Send>>,
) -> Result<JoinHandle<()>, Box<dyn std::error::Error + Send>> {
    match result {
        Ok(result) => {
            debug!(
                event_name = %config.event_name,
                contract_name = %config.contract_name,
                logs_count = result.logs.len(),
                "Processing logs"
            );

            let fn_data = result
                .logs
                .into_iter()
                .map(|log| {
                    EventResult::new(
                        Arc::clone(&config.network_contract),
                        log,
                        result.from_block,
                        result.to_block,
                    )
                })
                .collect::<Vec<_>>();

            // if shutting down so do not process anymore event
            while !is_running() {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }

            if !fn_data.is_empty() {
                return if config.index_event_in_order {
                    trigger_event(config, fn_data, result.to_block).await;
                    Ok(tokio::spawn(async {}))
                } else {
                    let task = tokio::spawn(async move {
                        trigger_event(config, fn_data, result.to_block).await;
                    });
                    Ok(task)
                }
            }

            Ok(tokio::spawn(async {})) // Return a completed task
        }
        Err(e) => {
            error!(error = %e, "Error fetching logs");
            Err(e)
        }
    }
}
