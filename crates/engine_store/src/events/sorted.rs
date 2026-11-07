//! Maps and sets as sorted lists, for what `state.json` holds: a map's key need
//! not be a JSON string, and two equal folds write the same bytes, which is how
//! tests compare them and what the digest of `STATE_VERSION` is taken of.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq as _};

fn sort_key<T: Serialize, E: serde::ser::Error>(key: &T) -> Result<Vec<u8>, E> {
    serde_json::to_vec(key).map_err(E::custom)
}

/// A `HashMap` as a list of its entries, in the order of their keys' JSON.
pub(crate) mod map {
    use super::*;

    pub(crate) fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut entries = map
            .iter()
            .map(|entry| Ok((sort_key(entry.0)?, entry)))
            .collect::<Result<Vec<_>, S::Error>>()?;
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let mut list = serializer.serialize_seq(Some(entries.len()))?;
        for (_, entry) in entries {
            list.serialize_element(&entry)?;
        }
        list.end()
    }

    pub(crate) fn deserialize<'de, K, V, D>(deserializer: D) -> Result<HashMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Ok(Vec::<(K, V)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

/// A `HashSet` as a list, in the order of its members' JSON.
pub(crate) mod set {
    use super::*;

    pub(crate) fn serialize<T: Serialize, S: Serializer>(
        set: &HashSet<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut members = set
            .iter()
            .map(|member| Ok((sort_key(member)?, member)))
            .collect::<Result<Vec<_>, S::Error>>()?;
        members.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let mut list = serializer.serialize_seq(Some(members.len()))?;
        for (_, member) in members {
            list.serialize_element(member)?;
        }
        list.end()
    }

    pub(crate) fn deserialize<'de, T, D>(deserializer: D) -> Result<HashSet<T>, D::Error>
    where
        T: Deserialize<'de> + Eq + Hash,
        D: Deserializer<'de>,
    {
        Ok(Vec::<T>::deserialize(deserializer)?.into_iter().collect())
    }
}
