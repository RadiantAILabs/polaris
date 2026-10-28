//! Count-bounded parsing of a persisted session document.
//!
//! [`SessionData`]'s derived `Deserialize` builds every checkpoint and
//! tombstone before any count can be checked. [`parse_session`] reads the same
//! JSON shape but stops at the first entry past the limit, skipping it unread,
//! so a hostile or corrupt file cannot make the loader allocate an arbitrarily
//! long vector.
//!
//! Every field of [`SessionData`] and [`CheckpointHistory`] is listed by hand
//! below, and the two values are rebuilt with struct literals, so adding a
//! field to either type fails to compile here until it is parsed too.

use super::{ResourceEntry, SessionData, TurnNumber};
use crate::checkpoint::{
    CheckpointHistory, CheckpointId, CheckpointTombstone, RevisionId, StoredCheckpoint,
};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;

/// The bounded collection that held more entries than allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overflow {
    /// `checkpoints`.
    Checkpoints,
    /// `checkpoint_history.tombstones`.
    Tombstones,
}

/// Why a session document could not be parsed.
#[derive(Debug)]
pub(super) enum ParseFailure {
    /// The bytes are not a valid session document.
    Malformed(serde_json::Error),
    /// A bounded collection held more entries than allowed.
    TooMany(Overflow),
}

/// Parses a session document, refusing it as soon as `checkpoints` or
/// `checkpoint_history.tombstones` holds more than `max_entries` entries.
pub(super) fn parse_session(bytes: &[u8], max_entries: usize) -> Result<SessionData, ParseFailure> {
    // serde reports a refusal as an opaque error; this records which
    // collection caused it so the caller can tell it from malformed input.
    let overflow = Cell::new(None);
    let seed = DocumentSeed(Limit {
        max_entries,
        overflow: &overflow,
    });
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let parsed = seed.deserialize(&mut deserializer).and_then(|data| {
        deserializer.end()?;
        Ok(data)
    });
    match (overflow.get(), parsed) {
        (Some(overflow), _) => Err(ParseFailure::TooMany(overflow)),
        (None, Ok(data)) => Ok(data),
        (None, Err(source)) => Err(ParseFailure::Malformed(source)),
    }
}

/// The entry limit and the place a refusal is recorded.
#[derive(Clone, Copy)]
struct Limit<'a> {
    max_entries: usize,
    overflow: &'a Cell<Option<Overflow>>,
}

/// Stores `next_value` in `slot`, rejecting a repeated key as `serde` derives do.
fn set_once<'de, A, S>(
    slot: &mut Option<S::Value>,
    name: &'static str,
    map: &mut A,
    seed: S,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    S: DeserializeSeed<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(name));
    }
    *slot = Some(map.next_value_seed(seed)?);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionData
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum DocumentField {
    AgentType,
    TurnNumber,
    CreatedAt,
    Resources,
    Checkpoints,
    NextCheckpoint,
    Revision,
    CheckpointHistory,
    #[serde(other)]
    Unknown,
}

struct DocumentSeed<'a>(Limit<'a>);

impl<'de> DeserializeSeed<'de> for DocumentSeed<'_> {
    type Value = SessionData;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<SessionData, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for DocumentSeed<'_> {
    type Value = SessionData;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a session document")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SessionData, A::Error> {
        let limit = self.0;
        let mut agent_type = None;
        let mut turn_number: Option<TurnNumber> = None;
        let mut created_at = None;
        let mut resources: Option<Vec<ResourceEntry>> = None;
        let mut checkpoints = None;
        let mut next_checkpoint: Option<u64> = None;
        let mut revision: Option<RevisionId> = None;
        let mut checkpoint_history = None;

        while let Some(field) = map.next_key()? {
            match field {
                DocumentField::AgentType => {
                    set_once(
                        &mut agent_type,
                        "agent_type",
                        &mut map,
                        PhantomData::<String>,
                    )?;
                }
                DocumentField::TurnNumber => {
                    set_once(&mut turn_number, "turn_number", &mut map, PhantomData)?;
                }
                DocumentField::CreatedAt => {
                    set_once(
                        &mut created_at,
                        "created_at",
                        &mut map,
                        PhantomData::<String>,
                    )?;
                }
                DocumentField::Resources => {
                    set_once(&mut resources, "resources", &mut map, PhantomData)?;
                }
                DocumentField::Checkpoints => {
                    let seed = BoundedSeq::<StoredCheckpoint>::new(limit, Overflow::Checkpoints);
                    set_once(&mut checkpoints, "checkpoints", &mut map, seed)?;
                }
                DocumentField::NextCheckpoint => {
                    set_once(
                        &mut next_checkpoint,
                        "next_checkpoint",
                        &mut map,
                        PhantomData,
                    )?;
                }
                DocumentField::Revision => {
                    set_once(&mut revision, "revision", &mut map, PhantomData)?;
                }
                DocumentField::CheckpointHistory => {
                    set_once(
                        &mut checkpoint_history,
                        "checkpoint_history",
                        &mut map,
                        HistorySeed(limit),
                    )?;
                }
                DocumentField::Unknown => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(SessionData {
            agent_type: agent_type.ok_or_else(|| de::Error::missing_field("agent_type"))?,
            turn_number: turn_number.ok_or_else(|| de::Error::missing_field("turn_number"))?,
            created_at: created_at.ok_or_else(|| de::Error::missing_field("created_at"))?,
            resources: resources.ok_or_else(|| de::Error::missing_field("resources"))?,
            checkpoints: checkpoints.unwrap_or_default(),
            next_checkpoint: next_checkpoint.unwrap_or_default(),
            revision: revision.unwrap_or_default(),
            checkpoint_history: checkpoint_history.unwrap_or_default(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CheckpointHistory
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HistoryField {
    Current,
    Tombstones,
    Floor,
    #[serde(other)]
    Unknown,
}

struct HistorySeed<'a>(Limit<'a>);

impl<'de> DeserializeSeed<'de> for HistorySeed<'_> {
    type Value = CheckpointHistory;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<CheckpointHistory, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for HistorySeed<'_> {
    type Value = CheckpointHistory;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("checkpoint history")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CheckpointHistory, A::Error> {
        let mut current: Option<Option<CheckpointId>> = None;
        let mut tombstones = None;
        let mut floor: Option<Option<CheckpointTombstone>> = None;

        while let Some(field) = map.next_key()? {
            match field {
                HistoryField::Current => {
                    set_once(&mut current, "current", &mut map, PhantomData)?;
                }
                HistoryField::Tombstones => {
                    let seed = BoundedSeq::<CheckpointTombstone>::new(self.0, Overflow::Tombstones);
                    set_once(&mut tombstones, "tombstones", &mut map, seed)?;
                }
                HistoryField::Floor => {
                    set_once(&mut floor, "floor", &mut map, PhantomData)?;
                }
                HistoryField::Unknown => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(CheckpointHistory {
            current: current.flatten(),
            tombstones: tombstones.unwrap_or_default(),
            floor: floor.flatten(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bounded sequence
// ─────────────────────────────────────────────────────────────────────────────

/// A JSON array read into a `Vec` of at most [`Limit::max_entries`] elements.
struct BoundedSeq<'a, T> {
    limit: Limit<'a>,
    kind: Overflow,
    element: PhantomData<fn() -> T>,
}

impl<'a, T> BoundedSeq<'a, T> {
    fn new(limit: Limit<'a>, kind: Overflow) -> Self {
        Self {
            limit,
            kind,
            element: PhantomData,
        }
    }
}

impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for BoundedSeq<'_, T> {
    type Value = Vec<T>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<T>, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for BoundedSeq<'_, T> {
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a sequence of at most {} entries",
            self.limit.max_entries
        )
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
        let mut entries = Vec::new();
        while entries.len() < self.limit.max_entries {
            match seq.next_element()? {
                Some(entry) => entries.push(entry),
                None => return Ok(entries),
            }
        }
        // Full. Any entry that follows is over the limit: skip it without
        // building it and stop reading the document.
        if seq.next_element::<IgnoredAny>()?.is_some() {
            self.limit.overflow.set(Some(self.kind));
            return Err(de::Error::custom("too many entries"));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static BUILT: AtomicUsize = AtomicUsize::new(0);

    /// An element that counts how many times it was actually built.
    #[derive(Debug)]
    struct Counted;

    impl<'de> Deserialize<'de> for Counted {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            IgnoredAny::deserialize(deserializer)?;
            BUILT.fetch_add(1, Ordering::SeqCst);
            Ok(Counted)
        }
    }

    fn bounded<T: for<'de> Deserialize<'de>>(
        json: &str,
        max_entries: usize,
    ) -> (Result<Vec<T>, serde_json::Error>, Option<Overflow>) {
        let overflow = Cell::new(None);
        let seed = BoundedSeq::<T>::new(
            Limit {
                max_entries,
                overflow: &overflow,
            },
            Overflow::Checkpoints,
        );
        let result = seed.deserialize(&mut serde_json::Deserializer::from_str(json));
        (result, overflow.get())
    }

    #[test]
    fn sequence_builds_no_more_than_the_limit_then_stops() {
        let json = format!("[{}]", vec!["0"; 10_000].join(","));

        let (result, overflow) = bounded::<Counted>(&json, 3);

        assert!(result.is_err());
        assert_eq!(overflow, Some(Overflow::Checkpoints));
        assert_eq!(BUILT.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn sequence_at_the_limit_is_accepted() {
        let (result, overflow) = bounded::<u32>("[1,2,3]", 3);

        assert_eq!(result.unwrap(), vec![1, 2, 3]);
        assert_eq!(overflow, None);
    }

    #[test]
    fn sequence_one_over_the_limit_is_refused() {
        let (result, overflow) = bounded::<u32>("[1,2,3,4]", 3);

        assert!(result.is_err());
        assert_eq!(overflow, Some(Overflow::Checkpoints));
    }

    #[test]
    fn malformed_input_is_not_reported_as_an_overflow() {
        let (result, overflow) = bounded::<u32>("[1,2,", 3);

        assert!(result.is_err());
        assert_eq!(overflow, None);
    }

    #[test]
    fn document_with_unknown_and_repeated_keys_follows_derive_behaviour() {
        let ok = br#"{"agent_type":"A","turn_number":1,"created_at":"t","resources":[],"extra":{"x":[1]}}"#;
        let data = parse_session(ok, 4).unwrap();
        assert_eq!(data.agent_type, "A");
        assert_eq!(data.turn_number, 1);
        assert!(data.checkpoints.is_empty());

        let repeated = br#"{"agent_type":"A","agent_type":"B","turn_number":1,"created_at":"t","resources":[]}"#;
        let ParseFailure::Malformed(source) = parse_session(repeated, 4).unwrap_err() else {
            panic!("expected a malformed document");
        };
        assert!(source.to_string().contains("duplicate field"), "{source}");

        let missing = br#"{"agent_type":"A","turn_number":1,"created_at":"t"}"#;
        let ParseFailure::Malformed(source) = parse_session(missing, 4).unwrap_err() else {
            panic!("expected a malformed document");
        };
        assert!(source.to_string().contains("missing field"), "{source}");

        let trailing = br#"{"agent_type":"A","turn_number":1,"created_at":"t","resources":[]} x"#;
        assert!(matches!(
            parse_session(trailing, 4),
            Err(ParseFailure::Malformed(_))
        ));
    }
}
