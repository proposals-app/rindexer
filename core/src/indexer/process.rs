use std::{sync::Arc, time::Duration};

use async_std::prelude::StreamExt;
use ethers::{prelude::ProviderError, types::U64};
use futures::future::join_all;
use tokio::task::{JoinError, JoinHandle};
use tracing::{debug, error, info};

use crate::{
    event::{
        callback_registry::EventResult, config::EventProcessingConfig, BuildRindexerFilterError,
    },
    indexer::{
        dependency::{ContractEventsDependenciesConfig, EventDependencies},
        fetch_logs::{fetch_logs_stream, live_indexing_stream, FetchLogsResult},
        last_synced::update_progress_and_last_synced_task,
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

async fn process_contract_events_with_dependencies(
    dependencies: EventDependencies,
    events_processing_config: Arc<Vec<Arc<EventProcessingConfig>>>,
) -> Result<(), ProcessContractEventsWithDependenciesError> {
    let mut stack = vec![dependencies.tree];

    let mut live_indexing_configs: Vec<Arc<EventProcessingConfig>> = Vec::new();

    while let Some(current_tree) = stack.pop() {
        let mut tasks = vec![];

        for dependency in &current_tree.contract_events {
            let event_processing_config = Arc::clone(&events_processing_config);
            let dependency = dependency.clone();

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

                // Forces live indexing off as it has to handle it a bit differently
                process_event_logs(Arc::clone(event_processing_config), true, true).await?;

                Ok::<Arc<EventProcessingConfig>, ProcessContractEventsWithDependenciesError>(
                    Arc::clone(event_processing_config),
                )
            });

            tasks.push(task);
        }

        let results = join_all(tasks).await;
        for result in results {
            match result {
                Ok(result) => match result {
                    Ok(config) => {
                        if config.live_indexing {
                            live_indexing_configs.push(config);
                        }
                    }
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

    if !live_indexing_configs.is_empty() {
        // Start live indexing for all configs that require it
        live_indexing_stream(&live_indexing_configs, true).await;
    }

    Ok(())
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

pub async fn handle_logs_result(
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
