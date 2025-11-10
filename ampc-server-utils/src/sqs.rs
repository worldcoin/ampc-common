//! SQS synchronization utilities for AMPC servers
//!
//! This module provides functions to synchronize SQS queues across nodes by:
//! 1. Reading the next SNS sequence number from the top of the queue
//! 2. Sharing sequence numbers during startup sync
//! 3. Deleting messages until reaching the maximum common sequence number

use aws_sdk_sqs::error::ProvideErrorMetadata;
use aws_sdk_sqs::Client;
use eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// SQS message structure for parsing SNS messages from SQS.
/// When SNS publishes to SQS with raw message delivery disabled, the message body
/// contains a JSON structure with metadata including the sequence number.
#[derive(Serialize, Deserialize, Debug)]
pub struct SQSMessage {
    #[serde(rename = "Type")]
    pub notification_type: String,
    #[serde(rename = "MessageId")]
    pub message_id: String,
    #[serde(rename = "SequenceNumber")]
    pub sequence_number: String,
    #[serde(rename = "TopicArn")]
    pub topic_arn: String,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Timestamp")]
    pub timestamp: String,
    #[serde(rename = "UnsubscribeURL")]
    pub unsubscribe_url: String,
    #[serde(rename = "MessageAttributes")]
    pub message_attributes: HashMap<String, serde_json::Value>,
}

/// Reads the top of the requests SQS queue and returns its sequence number.
///
/// This function performs a long poll on the SQS queue to peek at the next message
/// without consuming it. It sets visibility timeout to 0 to ensure the message
/// remains visible to other consumers.
///
/// # Arguments
/// * `sqs_client` - AWS SQS client
/// * `queue_url` - URL of the SQS queue
/// * `long_poll_seconds` - Duration for long polling (typically 20 seconds)
///
/// # Returns
/// * `Ok(Some(sequence_number))` - If a message with sequence number is found
/// * `Ok(None)` - If the queue is empty (timeout during long poll)
/// * `Err(...)` - If there's an error reading or parsing the message
pub async fn get_next_sns_seq_num(
    sqs_client: &Client,
    queue_url: &str,
    long_poll_seconds: i32,
) -> Result<Option<u128>> {
    let sqs_snoop_response = sqs_client
        .receive_message()
        .wait_time_seconds(long_poll_seconds)
        .max_number_of_messages(1)
        .set_visibility_timeout(Some(0))
        .queue_url(queue_url)
        .send()
        .await
        .context("while reading from SQS to snoop on the next message")?;

    if let Some(msgs) = sqs_snoop_response.messages {
        if msgs.len() != 1 {
            return Err(eyre!(
                "Expected exactly one message in the queue, but found {}",
                msgs.len()
            ));
        }
        let sqs_message = msgs.first().expect("first sqs message is empty");
        match serde_json::from_str::<SQSMessage>(sqs_message.body().unwrap_or("")) {
            Ok(message) => {
                // Found a valid message originating from SNS --> get its sequence number
                let sequence_number: u128 = str::parse(&message.sequence_number)
                    .map_err(|e| eyre!("sequence number is not a number: {}", e))?;
                tracing::info!("Next SNS sequence number in queue: {:?}", sequence_number);
                Ok(Some(sequence_number))
            }
            Err(err) => {
                tracing::error!(
                    "Found corrupt message in queue while parsing SNS message from body. The error is: '{}'. The SQS message body is '{}'",
                    err,
                    sqs_message.body().unwrap_or("None")
                );
                Err(err)
                    .context("Found corrupt message in queue while parsing SNS message from body")
            }
        }
    } else {
        tracing::info!("Timeout while waiting for a message in the queue. Queue is clean.");
        Ok(None)
    }
}

/// Deletes all messages in the requests SQS queue until the sequence number is reached.
/// Leaves the message with given sequence number on top of the queue.
///
/// This function is used during startup synchronization to ensure all nodes start
/// processing from the same point in the queue. The node that is behind will delete
/// messages until it reaches the maximum sequence number shared by all nodes.
///
/// # Arguments
/// * `sqs_client` - AWS SQS client
/// * `queue_url` - URL of the SQS queue
/// * `my_sequence_num` - Current sequence number of this node (None if queue is empty)
/// * `target_sequence_num` - Target sequence number to sync to (None if all queues are empty)
/// * `long_poll_seconds` - Duration for long polling
///
/// # Returns
/// * `Ok(())` - If synchronization completed successfully
/// * `Err(...)` - If there's an error during synchronization
///
/// # Reference
/// <https://docs.aws.amazon.com/sns/latest/dg/fifo-topic-message-ordering.html>
pub async fn delete_messages_until_sequence_num(
    sqs_client: &Client,
    queue_url: &str,
    my_sequence_num: Option<u128>,
    target_sequence_num: Option<u128>,
    long_poll_seconds: i32,
) -> Result<()> {
    tracing::info!(
        "Syncing queues. my_sequence_num: {:?}, target_sequence_num: {:?}.",
        my_sequence_num,
        target_sequence_num
    );

    // Exit early if there is no need to delete messages
    if target_sequence_num.is_none() {
        return if my_sequence_num.is_some() {
            Err(eyre!(
                "SQS target sequence number is None, but my sequence number is Some. This should not happen."
            ))
        } else {
            tracing::info!("Target sequence number is None. Queues are in clean state.");
            Ok(())
        };
    }

    let target_sequence_num = target_sequence_num.expect("could not unwrap target sequence number");
    if let Some(my_sequence_num) = my_sequence_num {
        if my_sequence_num == target_sequence_num {
            tracing::info!(
                "My sequence number is already equal to target sequence number. No need to delete messages."
            );
            return Ok(());
        }
    } else {
        tracing::info!(
            "My sequence number is None. Deleting all messages until target sequence number."
        );
    }

    // Delete messages until the target sequence number is reached
    loop {
        let sqs_snoop_response = sqs_client
            .receive_message()
            .wait_time_seconds(long_poll_seconds)
            .max_number_of_messages(1)
            .queue_url(queue_url)
            .send()
            .await
            .context("while reading from SQS to delete messages")?;

        if let Some(msgs) = sqs_snoop_response.messages {
            if msgs.len() != 1 {
                return Err(eyre!(
                    "Expected exactly one message in the queue, but found {}",
                    msgs.len()
                ));
            }
            let msg = msgs.first().expect("first sqs message is empty");
            let sequence_num: u128 = str::parse(
                &serde_json::from_str::<SQSMessage>(msg.body().unwrap_or(""))
                    .map_err(|e| eyre!("message is not a valid SQS message: {}", e))?
                    .sequence_number,
            )
            .map_err(|e| eyre!("sequence number is not a number: {}", e))?;

            if sequence_num < target_sequence_num {
                tracing::warn!(
                    "Deleting message with sequence number: {}, body: {:?}",
                    sequence_num,
                    msg.body
                );
                sqs_client
                    .delete_message()
                    .queue_url(queue_url)
                    .receipt_handle(msg.receipt_handle.clone().unwrap_or_default())
                    .send()
                    .await
                    .context("while deleting message from SQS")?;
            } else {
                tracing::info!(
                    "Reached target sequence number. Top of queue has sequence num: {}",
                    sequence_num
                );
                // Leave the message with target sequence number on top of the queue
                sqs_client
                    .change_message_visibility()
                    .queue_url(queue_url)
                    .receipt_handle(msg.receipt_handle.clone().unwrap_or_default())
                    .visibility_timeout(0)
                    .send()
                    .await
                    .context("while changing message visibility in SQS")?;
                break;
            }
        } else {
            tracing::info!("Could not find more messages in the queue. Retrying.");
        }
    }

    Ok(())
}

/// Fetches the approximate number of visible messages in the SQS queue.
///
/// # Arguments
/// * `sqs_client` - AWS SQS client
/// * `queue_url` - URL of the SQS queue
///
/// # Returns
/// * `Ok(count)` - Approximate number of visible messages
/// * `Err(...)` - If there's an error querying the queue attributes
pub async fn get_approximate_number_of_messages(
    sqs_client: &Client,
    queue_url: &str,
) -> Result<u32> {
    let attributes_response = sqs_client
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(aws_sdk_sqs::types::QueueAttributeName::ApproximateNumberOfMessages)
        .send()
        .await
        .map_err(|sdk_err| {
            tracing::error!(
                "SQS GetQueueAttributes failed. SDK error: {:?},  Message: {:?}",
                sdk_err,
                sdk_err.message()
            );
            eyre!("Failed to get queue attributes from SQS")
        })?;

    let num_messages_str = attributes_response
        .attributes
        .ok_or_else(|| eyre!("No attributes returned from GetQueueAttributes"))?
        .get(&aws_sdk_sqs::types::QueueAttributeName::ApproximateNumberOfMessages)
        .ok_or_else(|| eyre!("ApproximateNumberOfMessages attribute not found"))?
        .clone();

    num_messages_str
        .parse::<u32>()
        .context("Failed to parse ApproximateNumberOfMessages")
}
