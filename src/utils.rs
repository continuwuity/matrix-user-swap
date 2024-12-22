use std::{
    collections::{btree_map, BTreeMap},
    fmt,
};

use ruma::{serde::Raw, OwnedRoomAliasId, OwnedRoomId};
use serde::Serialize;
#[cfg(test)]
use serde_json::json;
use thiserror::Error;

pub(crate) type JsonMap = BTreeMap<String, Raw<serde_json::Value>>;

#[derive(Error, Debug, Serialize, Eq, PartialEq)]
#[error(
    "key {key} cannot be merged because the old and new maps have different \
     values. Old value {old_value}, new value {new_value}"
)]
pub(crate) struct JsonMergeError {
    pub(crate) key: String,
    pub(crate) old_value: serde_json::Value,
    pub(crate) new_value: serde_json::Value,
}

/// Copy key/value pairs in a json map from `old` to `new`.
///
/// If no keys were copied, returns `None`.
///
/// Returns the merged map and any error from keys that weren't able to be
/// merged. If the same key is present in both `old` and `new` with different
/// values, an error will be emitted and the key will be left with it's `new`
/// value.
pub(crate) fn merge_json(
    old: JsonMap,
    mut new: JsonMap,
) -> (Option<JsonMap>, Vec<JsonMergeError>) {
    let mut errors = vec![];
    let mut changed = false;

    for (key, value) in old {
        match new.entry(key) {
            btree_map::Entry::Vacant(e) => {
                e.insert(value);
                changed = true;
            }
            btree_map::Entry::Occupied(e) => {
                let old_value = value
                    .deserialize_as::<serde_json::Value>()
                    .expect("deserializing Raw to Value should never fail");
                let new_value = e
                    .get()
                    .deserialize_as::<serde_json::Value>()
                    .expect("deserializing Raw to Value should never fail");
                if old_value != new_value {
                    errors.push(JsonMergeError {
                        key: e.key().to_owned(),
                        old_value,
                        new_value,
                    });
                }
            }
        }
    }

    let new = if changed {
        Some(new)
    } else {
        None
    };

    (new, errors)
}

/// Room id along with the rooms canonical alias, if known.
///
/// This is used to identify rooms in a more human-readable way in logs.
#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct RoomIdentity {
    pub(crate) id: OwnedRoomId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) alias: Option<OwnedRoomAliasId>,
}

impl fmt::Display for RoomIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(alias) = &self.alias {
            write!(f, "{} ({})", alias, self.id)
        } else {
            write!(f, "{}", self.id)
        }
    }
}

#[cfg(test)]
fn assert_merged(
    old: serde_json::Value,
    new: serde_json::Value,
    expected: Option<serde_json::Value>,
    expected_errors: &[JsonMergeError],
) {
    let old = serde_json::from_value(old).unwrap();
    let new = serde_json::from_value(new).unwrap();

    let (merged, errors) = merge_json(old, new);
    let merged = merged.map(|merged| serde_json::to_value(merged).unwrap());
    assert_eq!(merged, expected);
    assert_eq!(errors, expected_errors);
}

#[test]
fn test_merge_empty() {
    let old = json!({});
    let new = json!({});
    assert_merged(old, new, None, &[]);
}

#[test]
fn test_merge_empty_new() {
    let old = json!({
        "key": "value"
    });
    let new = json!({});
    let expected = json!({
        "key": "value"
    });
    assert_merged(old, new, Some(expected), &[]);
}

#[test]
fn test_merge_empty_old() {
    let old = json!({});
    let new = json!({
        "key": "value"
    });
    assert_merged(old, new, None, &[]);
}

#[test]
fn test_merge_simple() {
    let old = json!({
        "key1": [ "value1" ]
    });
    let new = json!({
        "key2": "value2",
    });
    let expected = json!({
        "key1": [ "value1" ],
        "key2": "value2",
    });
    assert_merged(old, new, Some(expected), &[]);
}

#[test]
fn test_merge_conflict() {
    let old = json!({
        "shared_key": "value1",
        "conflicted_key": "value2",
        "new_key": "value3",
    });
    let new = json!({
        "shared_key": "value1",
        "conflicted_key": "value4",
    });
    let expected = json!({
        "shared_key": "value1",
        "conflicted_key": "value4",
        "new_key": "value3",
    });
    let errors = [JsonMergeError {
        key: "conflicted_key".to_owned(),
        old_value: json!("value2"),
        new_value: json!("value4"),
    }];
    assert_merged(old, new, Some(expected), &errors);
}
