use serde::{Deserialize, Serialize};

// TODO: add common config and modifications
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupSyncState {
    pub db_len: u64,
    pub next_sns_sequence_num: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSyncResult {
    pub my_state: StartupSyncState,
    pub all_states: Vec<StartupSyncState>,
}

impl StartupSyncResult {
    pub fn new(my_state: StartupSyncState, all_states: Vec<StartupSyncState>) -> Self {
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
}

#[cfg(test)]
mod tests {
    use crate::startup_sync::{StartupSyncResult, StartupSyncState};

    #[test]
    fn test_max_sns_sequence_num() {
        // 1. Test with all Some sequence values
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: Some(100),
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(200),
            },
            StartupSyncState {
                db_len: 30,
                next_sns_sequence_num: Some(150),
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        assert_eq!(sync_result.max_sns_sequence_num(), Some(200));

        // 2. Test with all None sequence values
        let state_with_none_sequence_num = StartupSyncState {
            db_len: 10,
            next_sns_sequence_num: None,
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
        // Test the edge case where some nodes have None while others have Some
        // This should panic to prevent the batch mismatch described in the issue
        let states = vec![
            StartupSyncState {
                db_len: 10,
                next_sns_sequence_num: None, // NodeX - advanced but empty queue
            },
            StartupSyncState {
                db_len: 20,
                next_sns_sequence_num: Some(123), // Other nodes still have messages
            },
            StartupSyncState {
                db_len: 30,
                next_sns_sequence_num: Some(123),
            },
        ];

        let sync_result = StartupSyncResult::new(states[0].clone(), states);
        // This should panic due to inconsistent sequence numbers
        sync_result.max_sns_sequence_num();
    }
}
