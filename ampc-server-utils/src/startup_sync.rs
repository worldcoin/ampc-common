use eyre::{ensure, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

// TODO: add modifications

/// Trait that defines the requirements for a common config that can be synchronized across nodes.
/// Each service (iris-mpc, face-ampc, etc.) can implement their own CommonConfig struct.
pub trait CommonConfig: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug + Clone {}

/// Default implementation of CommonConfig for services that want basic common config fields.
/// Services can implement their own structs with additional fields by implementing the CommonConfig trait.
///
/// Example for iris-mpc (future use):
/// ```ignore
/// #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// pub struct IrisMpcCommonConfig {
///     pub environment: String,
///     pub results_topic_arn: String,
///     pub input_queue_url: String,
///     pub database_url: String,
///     // Additional iris-mpc specific fields:
///     pub max_modifications_lookback: usize,
///     pub luc_enabled: bool,
///     pub hawk_server_deletions_enabled: bool,
///     // ... other fields
/// }
///
/// impl CommonConfig for IrisMpcCommonConfig {}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultCommonConfig {
    pub environment: String,
    pub results_topic_arn: String,
    pub input_queue_url: String,
    pub database_url: String,
}

impl CommonConfig for DefaultCommonConfig {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSyncState<C: CommonConfig> {
    pub db_len: u64,
    pub next_sns_sequence_num: Option<u128>,
    pub common_config: C,
}

impl<C: CommonConfig> Serialize for StartupSyncState<C> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("StartupSyncState", 3)?;
        state.serialize_field("db_len", &self.db_len)?;
        state.serialize_field("next_sns_sequence_num", &self.next_sns_sequence_num)?;
        state.serialize_field("common_config", &self.common_config)?;
        state.end()
    }
}

impl<'de, C: CommonConfig> Deserialize<'de> for StartupSyncState<C> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StartupSyncStateHelper<C> {
            db_len: u64,
            next_sns_sequence_num: Option<u128>,
            common_config: C,
        }

        let helper = StartupSyncStateHelper::<C>::deserialize(deserializer)?;
        Ok(StartupSyncState {
            db_len: helper.db_len,
            next_sns_sequence_num: helper.next_sns_sequence_num,
            common_config: helper.common_config,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSyncResult<C: CommonConfig> {
    pub my_state: StartupSyncState<C>,
    pub all_states: Vec<StartupSyncState<C>>,
}

impl<C: CommonConfig> StartupSyncResult<C> {
    pub fn new(my_state: StartupSyncState<C>, all_states: Vec<StartupSyncState<C>>) -> Self {
        Self {
            my_state,
            all_states,
        }
    }

    pub fn max_sns_sequence_num(&self) -> Option<u128> {
        let sequence_nums: Vec<Option<u128>> = self
            .all_states
            .iter()
            .map(|s| s.next_sns_sequence_num)
            .collect();

        // All nodes should either have an empty queue or filled with some items.
        // Otherwise, we can not conclude queue sync and proceed safely.
        // More info: https://linear.app/worldcoin/issue/POP-2577/cover-edge-case-in-sqs-sync
        let any_empty_queues = sequence_nums.iter().any(|seq| seq.is_none());
        let any_non_empty_queues = sequence_nums.iter().any(|seq| seq.is_some());
        if any_empty_queues && any_non_empty_queues {
            panic!(
                "Can not deduce max SNS sequence number safely out of {:?}. Restarting...",
                sequence_nums
            );
        }

        sequence_nums
            .into_iter()
            .max()
            .expect("can get max u128 value")
    }

    /// Check if the common part of the config is the same across all nodes.
    pub fn check_common_config(&self) -> Result<()> {
        let my_config = &self.my_state.common_config;
        for StartupSyncState {
            common_config: other_config,
            ..
        } in self.all_states.iter()
        {
            ensure!(
                my_config == other_config,
                "Inconsistent common config!\nhave: {:?}\ngot: {:?}",
                my_config,
                other_config
            );
        }
        tracing::info!("Common config is consistent across all nodes");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::startup_sync::{DefaultCommonConfig, StartupSyncResult, StartupSyncState};

    #[test]
    fn test_max_sns_sequence_num() {
        let common_config = DefaultCommonConfig {
            environment: "test".to_string(),
            results_topic_arn: "arn:aws:sns:us-east-1:123456789012:test-topic".to_string(),
            input_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/test-queue"
                .to_string(),
            database_url: "postgres://user:pass@localhost/test".to_string(),
        };

        // 1. Test with all Some sequence values
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: Some(100),
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(200),
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 30,
                next_sns_sequence_num: Some(150),
                common_config: common_config.clone(),
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        assert_eq!(sync_result.max_sns_sequence_num(), Some(200));

        // 2. Test with all None sequence values
        let state_with_none_sequence_num = StartupSyncState {
            db_len: 10,
            next_sns_sequence_num: None,
            common_config: common_config.clone(),
        };
        let all_states = vec![
            state_with_none_sequence_num.clone(),
            state_with_none_sequence_num.clone(),
            state_with_none_sequence_num.clone(),
        ];

        let sync_result_none = StartupSyncResult::new(state_with_none_sequence_num, all_states);
        assert_eq!(sync_result_none.max_sns_sequence_num(), None);
    }

    #[test]
    #[should_panic(expected = "Can not deduce max SNS sequence number safely")]
    fn test_max_sns_sequence_num_mixed_panic() {
        let common_config = DefaultCommonConfig {
            environment: "test".to_string(),
            results_topic_arn: "arn:aws:sns:us-east-1:123456789012:test-topic".to_string(),
            input_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/test-queue"
                .to_string(),
            database_url: "postgres://user:pass@localhost/test".to_string(),
        };

        // Test the edge case where some nodes have None while others have Some
        // This should panic to prevent the batch mismatch described in the issue
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: None, // NodeX - advanced but empty queue
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(123), // Other nodes still have messages
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 30,
                next_sns_sequence_num: Some(123),
                common_config: common_config.clone(),
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        // This should panic due to inconsistent sequence numbers
        sync_result.max_sns_sequence_num();
    }

    #[test]
    fn test_check_common_config() {
        let common_config = DefaultCommonConfig {
            environment: "test".to_string(),
            results_topic_arn: "arn:aws:sns:us-east-1:123456789012:test-topic".to_string(),
            input_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/test-queue"
                .to_string(),
            database_url: "postgres://user:pass@localhost/test".to_string(),
        };

        // Test with consistent configs
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: Some(100),
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(100),
                common_config: common_config.clone(),
            },
            StartupSyncState {
                db_len: 30,
                next_sns_sequence_num: Some(100),
                common_config: common_config.clone(),
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        assert!(sync_result.check_common_config().is_ok());
    }

    #[test]
    fn test_check_common_config_inconsistent() {
        let common_config1 = DefaultCommonConfig {
            environment: "test".to_string(),
            results_topic_arn: "arn:aws:sns:us-east-1:123456789012:test-topic".to_string(),
            input_queue_url: "https://sqs.us-east-1.amazonaws.com/123456789012/test-queue"
                .to_string(),
            database_url: "postgres://user:pass@localhost/test".to_string(),
        };

        let mut common_config2 = common_config1.clone();
        common_config2.environment = "different".to_string();

        // Test with inconsistent configs
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: Some(100),
                common_config: common_config1,
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(100),
                common_config: common_config2,
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        assert!(sync_result.check_common_config().is_err());
    }
}
