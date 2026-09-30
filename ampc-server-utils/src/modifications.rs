use eyre::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, fmt::Display, str::FromStr};

pub const MOD_STATUS_IN_PROGRESS: &str = "IN_PROGRESS";
pub const MOD_STATUS_COMPLETED: &str = "COMPLETED";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModificationStatus {
    InProgress,
    Completed,
}

impl Display for ModificationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModificationStatus::InProgress => write!(f, "{MOD_STATUS_IN_PROGRESS}"),
            ModificationStatus::Completed => write!(f, "{MOD_STATUS_COMPLETED}"),
        }
    }
}

impl FromStr for ModificationStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            MOD_STATUS_IN_PROGRESS => Ok(ModificationStatus::InProgress),
            MOD_STATUS_COMPLETED => Ok(ModificationStatus::Completed),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Modification<Id = i64> {
    pub id: Id,
    pub serial_id: Option<i64>,
    pub request_type: String,
    pub s3_url: Option<String>,
    pub status: String,
    pub persisted: bool,
    pub result_message_body: Option<String>,
}

impl<Id: PartialEq> PartialEq for Modification<Id> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.serial_id == other.serial_id
            && self.request_type == other.request_type
            && self.s3_url == other.s3_url
            && self.status == other.status
            && self.persisted == other.persisted
        // result_message_body is ignored since it differs across nodes
    }
}

impl<Id: Eq> Eq for Modification<Id> {}

impl<Id: fmt::Debug> fmt::Debug for Modification<Id> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let result_message_summary = match &self.result_message_body {
            Some(msg) => format!("Some([{} chars])", msg.chars().count()),
            None => "None".to_string(),
        };

        f.debug_struct("Modification")
            .field("id", &self.id)
            .field("serial_id", &self.serial_id)
            .field("request_type", &self.request_type)
            .field("s3_url", &self.s3_url)
            .field("status", &self.status)
            .field("persisted", &self.persisted)
            .field("result_message_body", &result_message_summary)
            .finish()
    }
}

impl<Id> Modification<Id> {
    /// Marks the modification as completed, setting the status to "COMPLETED", updating the result message body and persisted flag.
    ///
    /// If `updated_serial_id` is provided, it updates the serial_id field as well.
    /// It is used when the modification is a uniqueness request and the serial id is assigned after the protocol.
    pub fn mark_completed(
        &mut self,
        persisted: bool,
        result_message_body: &str,
        updated_serial_id: Option<u32>,
    ) {
        self.status = ModificationStatus::Completed.to_string();
        self.result_message_body = Some(result_message_body.to_string());
        self.persisted = persisted;
        if let Some(serial_id) = updated_serial_id {
            self.serial_id = Some(serial_id as i64);
        }
    }

    /// Updates the node_id field in the SNS message JSON to specified one
    pub fn update_result_message_node_id(&mut self, party_id: usize) -> Result<()> {
        if let Some(message) = &self.result_message_body {
            // Parse the JSON message
            match serde_json::from_str::<serde_json::Value>(message) {
                Ok(mut json_value) => {
                    // Update the node_id field if it exists
                    if let Some(obj) = json_value.as_object_mut() {
                        // Try to update node_id in the main object
                        if obj.contains_key("node_id") {
                            obj.insert(
                                "node_id".to_string(),
                                serde_json::Value::Number(serde_json::Number::from(party_id)),
                            );
                            self.result_message_body = Some(serde_json::to_string(&json_value)?);
                        } else {
                            return Err(eyre::eyre!("Message body does not contain node_id"));
                        }
                    } else {
                        return Err(eyre::eyre!("Result message body must be a JSON object"));
                    }
                }
                Err(_) => {
                    return Err(eyre::eyre!("Invalid JSON message"));
                }
            }
        } else {
            return Err(eyre::eyre!("Result message body is None"));
        }
        Ok(())
    }
}

/// Ordered modifications to update and delete, respectively.
pub type ModificationPlan<Id = i64> = (Vec<Modification<Id>>, Vec<Modification<Id>>);

/// Compare local modifications against party snapshots, returning (to_update, to_delete).
/// Updates and deletions are ordered by operation ID. Completed operations missing locally
/// are skipped because bounded Iris snapshots can contain older operations from lagging peers.
/// Callers requiring complete history must validate missing local records before reconciliation.
pub fn compare_modifications<Id: Ord + Clone + fmt::Debug>(
    my: &[Modification<Id>],
    all: &[Vec<Modification<Id>>],
) -> Result<ModificationPlan<Id>> {
    // 1. Group all modifications by id => Vec<Modification> (from different nodes)
    let mut grouped: BTreeMap<Id, Vec<Modification<Id>>> = BTreeMap::new();
    for m in all.iter().flat_map(|s| s.iter().cloned()) {
        grouped.entry(m.id.clone()).or_default().push(m);
    }

    tracing::debug!("Grouped modifications: {}", grouped.len());

    // Store the results here
    let mut to_update = Vec::new();
    let mut to_delete = Vec::new();

    // 2. Analyze each modification group
    for (id, group_mods) in &grouped {
        check_modifications_consistency(group_mods)?;

        // Find local node's copy, if any
        let local_copy = my.iter().find(|m| &m.id == id);

        // Evaluate the global state across all nodes:
        let any_completed = group_mods
            .iter()
            .any(|m| m.status == ModificationStatus::Completed.to_string());
        let all_in_progress = group_mods
            .iter()
            .all(|m| m.status == ModificationStatus::InProgress.to_string());
        let any_persisted = group_mods.iter().any(|m| m.persisted);

        if all_in_progress {
            // If they're all in-progress => ignore the modification by deleting it
            if let Some(local_m) = local_copy {
                to_delete.push(local_m.clone());
            }
        } else if any_completed {
            // If any node completed => unify to COMPLETED
            let serial_id = group_mods.iter().find_map(|m| m.serial_id);
            let mut completed = group_mods
                .iter()
                .filter(|m| m.status == MOD_STATUS_COMPLETED);
            let first_completed = completed
                .clone()
                .find(|m| m.serial_id == serial_id)
                .or_else(|| completed.next())
                .expect("At least one completed modification");
            match local_copy {
                None => {
                    // If an item is completed for a party, it should at least exist in the
                    // local state because it should have been added during receive_batch.
                    // This can only happen when other party misses an in_progress mod.
                    // Local party will fetch until modification id X while the other party will
                    // fetch until mod id X-1. In this case, local party won't find X-1.
                    // We log and skip updating to avoid rolling back to an older share in local.
                    tracing::debug!("Skip missing completed modification: {:?}", id);
                }
                Some(local_m) => {
                    if local_m.status != ModificationStatus::Completed.to_string()
                        || local_m.persisted != any_persisted
                        || local_m.serial_id != serial_id
                    {
                        // If local is not "completed" or doesn't match the final persisted
                        // We'll roll forward local_m
                        let mut roll_forward = first_completed.clone();
                        roll_forward.status = ModificationStatus::Completed.to_string();
                        roll_forward.persisted = any_persisted;
                        roll_forward.serial_id = serial_id;
                        tracing::debug!("Planning to update modification: {:?}", id);
                        to_update.push(roll_forward);
                    } else {
                        tracing::debug!("Local modification is already in sync: {:?}", id);
                    }
                }
            }
        } else {
            eyre::bail!("Unexpected modification state for ID {:?}", id);
        }
    }

    Ok((to_update, to_delete))
}

fn check_modifications_consistency<Id: Eq + fmt::Debug>(
    modifications: &[Modification<Id>],
) -> Result<()> {
    let first = modifications.first().expect("Empty modifications");
    let mut serial_id = None;
    for m in modifications {
        ensure!(first.id == m.id, "Inconsistent modification IDs");
        ensure!(
            first.request_type == m.request_type,
            "Inconsistent request types for ID {:?}",
            first.id
        );
        ensure!(
            first.s3_url == m.s3_url,
            "Inconsistent input references for ID {:?}",
            first.id
        );
        ensure!(
            m.status == MOD_STATUS_IN_PROGRESS || m.status == MOD_STATUS_COMPLETED,
            "Unexpected modification state for ID {:?}",
            first.id
        );
        if let Some(current) = m.serial_id {
            ensure!(
                serial_id.is_none_or(|previous| previous == current),
                "Inconsistent serial IDs for ID {:?}",
                first.id
            );
            serial_id = Some(current);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modification(id: i64, status: &str, persisted: bool) -> Modification {
        Modification {
            id,
            serial_id: Some(id),
            request_type: "reauth".into(),
            s3_url: Some(format!("input/{id}")),
            status: status.into(),
            persisted,
            result_message_body: Some(r#"{"node_id":0}"#.into()),
        }
    }

    #[test]
    fn rolls_forward_outdated_local_operations_in_id_order() {
        let mut assigned = modification(3, MOD_STATUS_IN_PROGRESS, false);
        assigned.serial_id = None;
        let local = vec![
            assigned,
            modification(2, MOD_STATUS_IN_PROGRESS, false),
            modification(1, MOD_STATUS_COMPLETED, false),
        ];
        let completed = vec![
            modification(3, MOD_STATUS_COMPLETED, true),
            modification(2, MOD_STATUS_COMPLETED, false),
            modification(1, MOD_STATUS_COMPLETED, false),
        ];
        let (updates, deletes) = compare_modifications(
            &local,
            &[local.clone(), completed.clone(), completed.clone()],
        )
        .unwrap();
        assert_eq!(updates, vec![completed[1].clone(), completed[0].clone()]);
        assert!(deletes.is_empty());
    }

    #[test]
    fn completed_local_operations_need_no_changes() {
        let local = vec![modification(1, MOD_STATUS_COMPLETED, true)];
        let pending = vec![modification(1, MOD_STATUS_IN_PROGRESS, false)];
        assert_eq!(
            compare_modifications(&local, &[local.clone(), pending]).unwrap(),
            (vec![], vec![])
        );
    }

    #[test]
    fn discards_uncompleted_operations_and_skips_older_missing_operations() {
        let local = vec![
            modification(2, MOD_STATUS_COMPLETED, true),
            modification(3, MOD_STATUS_IN_PROGRESS, false),
        ];
        let other = vec![
            modification(1, MOD_STATUS_COMPLETED, true),
            local[0].clone(),
        ];
        let (updates, deletes) = compare_modifications(&local, &[local.clone(), other]).unwrap();
        assert!(updates.is_empty());
        assert_eq!(deletes, vec![local[1].clone()]);
    }

    #[test]
    fn rejects_conflicting_serials_when_first_party_has_none() {
        let mut pending = modification(1, MOD_STATUS_IN_PROGRESS, false);
        pending.serial_id = None;
        let complete = modification(1, MOD_STATUS_COMPLETED, true);
        let mut conflicting = complete.clone();
        conflicting.serial_id = Some(2);
        let error = compare_modifications(
            &[pending.clone()],
            &[vec![pending], vec![complete], vec![conflicting]],
        )
        .unwrap_err();
        assert!(error.to_string().contains("Inconsistent serial IDs"));
    }

    #[test]
    fn rejects_inconsistent_inputs_and_unknown_status() {
        let local = modification(1, MOD_STATUS_IN_PROGRESS, false);
        let mut other = local.clone();
        other.s3_url = Some("another-input".into());
        assert!(compare_modifications(
            std::slice::from_ref(&local),
            &[vec![local.clone()], vec![other]]
        )
        .is_err());
        let mut invalid = local.clone();
        invalid.status = "UNKNOWN".into();
        assert!(compare_modifications(
            std::slice::from_ref(&local),
            &[vec![local.clone()], vec![invalid]]
        )
        .is_err());
    }

    #[test]
    fn accepts_full_width_sequence_ids() {
        let pending = Modification {
            id: u128::MAX,
            status: MOD_STATUS_IN_PROGRESS.into(),
            ..Default::default()
        };
        let complete = Modification {
            status: MOD_STATUS_COMPLETED.into(),
            persisted: true,
            ..pending.clone()
        };
        assert_eq!(
            compare_modifications(
                std::slice::from_ref(&pending),
                &[vec![pending.clone()], vec![complete.clone()]]
            )
            .unwrap()
            .0,
            vec![complete]
        );
    }

    #[test]
    fn preserves_metadata_wire_format_and_party_specific_results() {
        let mut local = modification(1, MOD_STATUS_IN_PROGRESS, false);
        local.mark_completed(true, r#"{"node_id":0}"#, Some(2));
        let json = serde_json::to_value(&local).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": 1, "serial_id": 2, "request_type": "reauth", "s3_url": "input/1", "status": "COMPLETED", "persisted": true, "result_message_body": r#"{"node_id":0}"#})
        );
        let mut other: Modification = serde_json::from_value(json).unwrap();
        other.update_result_message_node_id(1).unwrap();
        assert_eq!(local, other);
        assert_ne!(local.result_message_body, other.result_message_body);
        assert!(!format!("{other:?}").contains("node_id"));
    }

    #[test]
    fn repairs_completed_local_serial_from_matching_completed_peer() {
        for persisted in [false, true] {
            let mut local = modification(1, MOD_STATUS_COMPLETED, persisted);
            local.serial_id = None;
            let mut peer = modification(1, MOD_STATUS_COMPLETED, persisted);
            peer.result_message_body = Some(r#"{"node_id":1,"serial_id":1}"#.into());
            let (updates, deletes) = compare_modifications(
                std::slice::from_ref(&local),
                &[vec![local.clone()], vec![peer.clone()]],
            )
            .unwrap();
            assert_eq!(updates, vec![peer.clone()]);
            assert_eq!(updates[0].result_message_body, peer.result_message_body);
            assert!(deletes.is_empty());
        }
    }

    #[test]
    fn retains_agreed_serial_when_completed_record_has_no_serial() {
        let mut local = modification(1, MOD_STATUS_IN_PROGRESS, false);
        local.serial_id = None;
        let mut complete = modification(1, MOD_STATUS_COMPLETED, false);
        complete.serial_id = None;
        let assigned = modification(1, MOD_STATUS_IN_PROGRESS, false);
        let (updates, deletes) = compare_modifications(
            std::slice::from_ref(&local),
            &[vec![local.clone()], vec![complete.clone()], vec![assigned]],
        )
        .unwrap();
        assert_eq!(updates[0].serial_id, Some(1));
        assert_eq!(updates[0].result_message_body, complete.result_message_body);
        assert!(deletes.is_empty());
    }

    #[test]
    fn rejects_nonobject_result_bodies_without_changing_them() {
        for body in ["null", "[]", r#""message""#, "42", "true"] {
            let mut modification = modification(1, MOD_STATUS_COMPLETED, true);
            modification.result_message_body = Some(body.into());
            let error = modification.update_result_message_node_id(2).unwrap_err();
            assert!(error.to_string().contains("must be a JSON object"));
            assert_eq!(modification.result_message_body.as_deref(), Some(body));
        }
    }
}
