//! Reconcile ordered public operation metadata without exchanging party-local shares.
use eyre::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// An immutable input identity and optional durable outcome, ordered by SNS FIFO sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEntry {
    pub sequence: String,
    pub job_id: String,
    pub intent: Value,
    pub outcome: Option<Value>,
    pub previous_sequence: Option<String>,
    pub previous_hash: String,
    pub outcome_hash: Option<String>,
}

/// Return agreed outcomes to roll forward. Intent-only operations remain queued for recomputation.
/// Missing inputs for a completed operation and contradictory outcomes fail closed.
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn outcome_hash(entry: &RecoveryEntry) -> Result<String> {
    let bytes = serde_json::to_vec(&(
        entry.sequence.clone(),
        entry.job_id.clone(),
        entry.intent.clone(),
        entry.previous_sequence.clone(),
        entry.previous_hash.clone(),
        entry.outcome.clone(),
    ))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn reconcile(states: &[Vec<RecoveryEntry>]) -> Result<Vec<RecoveryEntry>> {
    ensure!(
        states.len() == 3,
        "Recovery requires exactly three party snapshots"
    );
    let mut identities = BTreeMap::new();
    let mut jobs = BTreeMap::<u128, Vec<Option<&RecoveryEntry>>>::new();
    for (party, state) in states.iter().enumerate() {
        let mut previous: Option<&RecoveryEntry> = None;
        for entry in state {
            let sequence = entry.sequence.parse::<u128>()?;
            ensure!(
                sequence.to_string() == entry.sequence,
                "Noncanonical recovery sequence"
            );
            match &entry.previous_sequence {
                Some(predecessor) => {
                    let value = predecessor.parse::<u128>()?;
                    ensure!(
                        value.to_string() == *predecessor && value < sequence,
                        "Invalid recovery predecessor sequence"
                    );
                    ensure!(
                        valid_hash(&entry.previous_hash),
                        "Invalid recovery predecessor hash"
                    );
                }
                None => ensure!(
                    entry.previous_hash == "genesis",
                    "Invalid recovery genesis marker"
                ),
            }
            if let Some(hash) = &entry.outcome_hash {
                ensure!(valid_hash(hash), "Invalid recovery outcome hash encoding");
            }
            if let Some(previous) = previous {
                ensure!(
                    previous.sequence.parse::<u128>()? < sequence,
                    "Recovery history is not strictly ordered"
                );
                ensure!(
                    entry.previous_sequence.as_ref() == Some(&previous.sequence)
                        && previous.outcome_hash.as_ref() == Some(&entry.previous_hash),
                    "Recovery history chain is broken"
                );
            }
            if entry.outcome.is_some() {
                ensure!(
                    entry.outcome_hash.as_ref() == Some(&outcome_hash(entry)?),
                    "Invalid recovery outcome history hash"
                );
            } else {
                ensure!(
                    entry.outcome_hash.is_none(),
                    "Prepared entry has a completed history hash"
                );
            }
            if let Some(existing) = identities.insert(entry.job_id.clone(), sequence) {
                ensure!(
                    existing == sequence,
                    "Recovery job appears at multiple sequences"
                );
            }
            previous = Some(entry);
            let slots = jobs.entry(sequence).or_insert_with(|| vec![None; 3]);
            ensure!(
                slots[party].replace(entry).is_none(),
                "Duplicate recovery sequence"
            );
        }
    }
    // Every pair must overlap with matching history markers, unless both start at genesis.
    for left in 0..states.len() {
        for right in left + 1..states.len() {
            let ids: BTreeSet<_> = states[left].iter().map(|e| &e.sequence).collect();
            let overlap = states[right].iter().any(|e| ids.contains(&e.sequence));
            let genesis = states[left]
                .first()
                .is_none_or(|e| e.previous_sequence.is_none())
                && states[right]
                    .first()
                    .is_none_or(|e| e.previous_sequence.is_none());
            ensure!(
                overlap || genesis,
                "Recovery divergence exceeds retained history horizon"
            );
        }
    }
    let mut agreed = Vec::new();
    let mut unfinished = false;
    for slots in jobs.values() {
        let first = slots.iter().flatten().next().unwrap();
        for entry in slots.iter().flatten() {
            ensure!(
                entry.job_id == first.job_id
                    && entry.intent == first.intent
                    && entry.previous_sequence == first.previous_sequence
                    && entry.previous_hash == first.previous_hash,
                "Recovery input identity conflict"
            );
        }
        let outcomes: Vec<_> = slots
            .iter()
            .flatten()
            .filter_map(|e| e.outcome.as_ref())
            .collect();
        if let Some(outcome) = outcomes.first() {
            ensure!(
                !unfinished,
                "Completed operation follows an unfinished operation"
            );
            for (party, slot) in slots.iter().enumerate() {
                if slot.is_none() {
                    let older_than_suffix = states[party].first().is_some_and(|e| {
                        e.sequence.parse::<u128>().is_ok_and(|seq| {
                            first
                                .sequence
                                .parse::<u128>()
                                .is_ok_and(|current| current < seq)
                        })
                    });
                    ensure!(
                        older_than_suffix,
                        "Completed operation is missing party-local recovery input"
                    );
                }
            }
            ensure!(
                outcomes.iter().all(|o| o == outcome),
                "Conflicting durable recovery outcomes"
            );
            let mut entry = (*first).clone();
            entry.outcome = Some((*outcome).clone());
            entry.outcome_hash = Some(outcome_hash(&entry)?);
            agreed.push(entry);
        } else {
            unfinished = true;
        }
    }
    Ok(agreed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(outcome: Option<Value>) -> RecoveryEntry {
        let mut entry = RecoveryEntry {
            sequence: "10".into(),
            job_id: "job".into(),
            intent: serde_json::json!({"kind":"enroll"}),
            outcome,
            previous_sequence: None,
            previous_hash: "genesis".into(),
            outcome_hash: None,
        };
        if entry.outcome.is_some() {
            entry.outcome_hash = Some(outcome_hash(&entry).unwrap());
        }
        entry
    }
    #[test]
    fn adopts_one_completed_party_without_recomputation() {
        let done = entry(Some(serde_json::json!({"serial_id":1})));
        let prepared = entry(None);
        assert_eq!(
            reconcile(&[vec![done.clone()], vec![prepared.clone()], vec![prepared]]).unwrap(),
            vec![done]
        );
    }
    #[test]
    fn rejects_missing_party_input_for_completed_operation() {
        assert!(reconcile(&[
            vec![entry(Some(serde_json::json!(1)))],
            vec![],
            vec![entry(None)]
        ])
        .is_err());
    }
    #[test]
    fn rejects_conflicting_completed_outcomes() {
        assert!(reconcile(&[
            vec![entry(Some(serde_json::json!(1)))],
            vec![entry(Some(serde_json::json!(2)))],
            vec![entry(None)]
        ])
        .is_err());
    }
    #[test]
    fn rejects_noncanonical_sequence_and_duplicate_job_identity() {
        let mut invalid = entry(None);
        invalid.sequence = "010".into();
        assert!(reconcile(&[vec![invalid], vec![], vec![]]).is_err());
        let first = entry(Some(serde_json::json!(1)));
        let mut later = entry(None);
        later.sequence = "20".into();
        later.previous_sequence = Some(first.sequence.clone());
        later.previous_hash = first.outcome_hash.clone().unwrap();
        assert!(reconcile(&[vec![first, later], vec![], vec![]]).is_err());
    }
    #[test]
    fn rejects_invalid_history_boundary() {
        let mut invalid = entry(None);
        invalid.previous_sequence = Some("20".into());
        invalid.previous_hash = "a".repeat(64);
        assert!(reconcile(&[vec![invalid], vec![], vec![]]).is_err());
    }
    #[test]
    fn accepts_overlapping_bounded_suffixes() {
        let first = entry(Some(serde_json::json!(1)));
        let mut later = entry(Some(serde_json::json!(2)));
        later.sequence = "20".into();
        later.job_id = "second".into();
        later.previous_sequence = Some(first.sequence.clone());
        later.previous_hash = first.outcome_hash.clone().unwrap();
        later.outcome_hash = Some(outcome_hash(&later).unwrap());
        assert_eq!(
            reconcile(&[
                vec![first.clone(), later.clone()],
                vec![later.clone()],
                vec![first, later]
            ])
            .unwrap()
            .len(),
            2
        );
    }
    #[test]
    fn rejects_histories_outside_overlap_horizon() {
        let first = entry(Some(serde_json::json!(1)));
        let mut later = entry(None);
        later.sequence = "20".into();
        later.job_id = "second".into();
        later.previous_sequence = Some(first.sequence.clone());
        later.previous_hash = first.outcome_hash.clone().unwrap();
        assert!(reconcile(&[vec![first.clone()], vec![later], vec![first]]).is_err());
    }
    #[test]
    fn prepared_only_work_remains_for_collective_recomputation() {
        assert!(reconcile(&[vec![entry(None)], vec![], vec![]])
            .unwrap()
            .is_empty());
    }
}
