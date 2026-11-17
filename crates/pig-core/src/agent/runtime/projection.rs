use std::collections::BTreeMap;

use super::{RuntimeControlEvent, RuntimeSnapshotView};

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionValue {
    pub version: u32,
    pub sequence: u64,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeProjectionSnapshot {
    pub consistent_sequence: u64,
    pub values: BTreeMap<String, ProjectionValue>,
}

#[derive(Debug)]
pub enum ProjectionError {
    DuplicateKey(String),
    SessionMismatch,
    Gap { expected: u64, actual: u64 },
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateKey(key) => write!(formatter, "projection {key} is already registered"),
            Self::SessionMismatch => {
                formatter.write_str("projection event belongs to another session")
            }
            Self::Gap { expected, actual } => {
                write!(
                    formatter,
                    "projection event gap: expected sequence {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

pub trait RuntimeProjectionDefinition: Send {
    fn key(&self) -> &str;
    fn version(&self) -> u32;
    fn initial(&self, snapshot: &RuntimeSnapshotView) -> serde_json::Value;
    fn apply(&self, state: &mut serde_json::Value, event: &RuntimeControlEvent);
}

struct RegisteredProjection {
    definition: Box<dyn RuntimeProjectionDefinition>,
    state: serde_json::Value,
    sequence: u64,
}

pub struct RuntimeProjectionRegistry {
    session_id: String,
    watermark: u64,
    projections: BTreeMap<String, RegisteredProjection>,
}

impl Default for RuntimeProjectionRegistry {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            watermark: 0,
            projections: BTreeMap::new(),
        }
    }
}

impl RuntimeProjectionRegistry {
    pub fn new(snapshot: &RuntimeSnapshotView) -> Self {
        Self {
            session_id: snapshot.session_id.clone(),
            watermark: snapshot.latest_sequence,
            projections: BTreeMap::new(),
        }
    }

    pub fn register(
        &mut self,
        definition: Box<dyn RuntimeProjectionDefinition>,
        snapshot: &RuntimeSnapshotView,
    ) -> Result<(), ProjectionError> {
        if snapshot.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch);
        }
        let key = definition.key().to_string();
        if self.projections.contains_key(&key) {
            return Err(ProjectionError::DuplicateKey(key));
        }
        self.projections.insert(
            key,
            RegisteredProjection {
                state: definition.initial(snapshot),
                sequence: snapshot.latest_sequence,
                definition,
            },
        );
        Ok(())
    }

    pub fn resync(
        &mut self,
        key: &str,
        snapshot: &RuntimeSnapshotView,
    ) -> Result<(), ProjectionError> {
        if snapshot.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch);
        }
        let projection = self
            .projections
            .get_mut(key)
            .ok_or_else(|| ProjectionError::DuplicateKey(key.to_string()))?;
        projection.state = projection.definition.initial(snapshot);
        projection.sequence = snapshot.latest_sequence;
        self.watermark = self.watermark.max(snapshot.latest_sequence);
        Ok(())
    }

    pub fn resync_all(&mut self, snapshot: &RuntimeSnapshotView) -> Result<(), ProjectionError> {
        if snapshot.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch);
        }
        for projection in self.projections.values_mut() {
            projection.state = projection.definition.initial(snapshot);
            projection.sequence = snapshot.latest_sequence;
        }
        self.watermark = snapshot.latest_sequence;
        Ok(())
    }

    pub fn apply(&mut self, event: &RuntimeControlEvent) -> Result<bool, ProjectionError> {
        if event.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch);
        }
        if event.sequence <= self.watermark {
            for projection in self.projections.values_mut() {
                if event.sequence > projection.sequence {
                    let expected = projection.sequence + 1;
                    if event.sequence != expected {
                        return Err(ProjectionError::Gap {
                            expected,
                            actual: event.sequence,
                        });
                    }
                    projection.definition.apply(&mut projection.state, event);
                    projection.sequence = event.sequence;
                }
            }
            return Ok(false);
        }
        let expected = self.watermark + 1;
        if event.sequence != expected {
            return Err(ProjectionError::Gap {
                expected,
                actual: event.sequence,
            });
        }
        for projection in self.projections.values_mut() {
            if event.sequence > projection.sequence {
                let expected = projection.sequence + 1;
                if event.sequence != expected {
                    return Err(ProjectionError::Gap {
                        expected,
                        actual: event.sequence,
                    });
                }
                projection.definition.apply(&mut projection.state, event);
                projection.sequence = event.sequence;
            }
        }
        self.watermark = event.sequence;
        Ok(true)
    }

    pub fn snapshot(&self) -> RuntimeProjectionSnapshot {
        let consistent_sequence = self
            .projections
            .values()
            .map(|projection| projection.sequence)
            .min()
            .unwrap_or(self.watermark);
        RuntimeProjectionSnapshot {
            consistent_sequence,
            values: self
                .projections
                .iter()
                .map(|(key, projection)| {
                    (
                        key.clone(),
                        ProjectionValue {
                            version: projection.definition.version(),
                            sequence: projection.sequence,
                            value: projection.state.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::{RuntimeEventKind, new_runtime_id};

    fn snapshot(sequence: u64) -> RuntimeSnapshotView {
        RuntimeSnapshotView {
            session_id: "s1".into(),
            agents: Vec::new(),
            runs: Vec::new(),
            attempts: Vec::new(),
            mailbox: Vec::new(),
            latest_sequence: sequence,
            closed: false,
        }
    }

    fn event(sequence: u64, kind: RuntimeEventKind) -> RuntimeControlEvent {
        RuntimeControlEvent {
            session_id: "s1".into(),
            sequence,
            event_id: new_runtime_id("event"),
            kind,
            agent_id: Some("a1".into()),
            run_id: Some("run-1".into()),
            attempt: Some(1),
            backend_epoch: Some("epoch-1".into()),
            payload: serde_json::json!({}),
            created_at_ms: 0,
        }
    }

    struct RunProjection;

    impl RuntimeProjectionDefinition for RunProjection {
        fn key(&self) -> &str {
            "test.runs"
        }

        fn version(&self) -> u32 {
            3
        }

        fn initial(&self, snapshot: &RuntimeSnapshotView) -> serde_json::Value {
            serde_json::json!({ "initialized_at": snapshot.latest_sequence, "status": null })
        }

        fn apply(&self, state: &mut serde_json::Value, event: &RuntimeControlEvent) {
            if event.kind == RuntimeEventKind::RunStarted {
                state["status"] = serde_json::json!("running");
            } else if event.kind == RuntimeEventKind::RunSettled {
                state["status"] = serde_json::json!("settled");
            }
        }
    }

    #[test]
    fn projections_fold_ordered_events_and_resync_from_snapshots() {
        let mut registry = RuntimeProjectionRegistry::new(&snapshot(0));
        registry
            .register(Box::new(RunProjection), &snapshot(0))
            .expect("register");
        registry
            .apply(&event(1, RuntimeEventKind::AgentRegistered))
            .expect("first");
        registry
            .apply(&event(2, RuntimeEventKind::RunStarted))
            .expect("second");
        assert!(
            !registry
                .apply(&event(2, RuntimeEventKind::RunStarted))
                .expect("duplicate")
        );
        assert!(matches!(
            registry.apply(&event(4, RuntimeEventKind::RunSettled)),
            Err(ProjectionError::Gap {
                expected: 3,
                actual: 4
            })
        ));
        let current = registry.snapshot();
        assert_eq!(current.consistent_sequence, 2);
        assert_eq!(current.values["test.runs"].version, 3);
        assert_eq!(current.values["test.runs"].value["status"], "running");

        registry.resync_all(&snapshot(4)).expect("resync all");
        registry
            .apply(&event(5, RuntimeEventKind::RunSettled))
            .expect("post-resync");
        let current = registry.snapshot();
        assert_eq!(current.consistent_sequence, 5);
        assert_eq!(current.values["test.runs"].value["initialized_at"], 4);
        assert_eq!(current.values["test.runs"].value["status"], "settled");
        assert!(matches!(
            registry.register(Box::new(RunProjection), &snapshot(5)),
            Err(ProjectionError::DuplicateKey(_))
        ));
    }

    #[test]
    fn single_projection_resync_cannot_skip_other_projections() {
        struct OtherProjection;
        impl RuntimeProjectionDefinition for OtherProjection {
            fn key(&self) -> &str {
                "test.other"
            }

            fn version(&self) -> u32 {
                1
            }

            fn initial(&self, snapshot: &RuntimeSnapshotView) -> serde_json::Value {
                serde_json::json!({ "initialized_at": snapshot.latest_sequence, "events": 0 })
            }

            fn apply(&self, state: &mut serde_json::Value, _event: &RuntimeControlEvent) {
                state["events"] = serde_json::json!(state["events"].as_u64().unwrap_or(0) + 1);
            }
        }

        let mut registry = RuntimeProjectionRegistry::new(&snapshot(0));
        registry
            .register(Box::new(RunProjection), &snapshot(0))
            .expect("runs");
        registry
            .register(Box::new(OtherProjection), &snapshot(0))
            .expect("other");
        registry
            .apply(&event(1, RuntimeEventKind::AgentRegistered))
            .expect("first");
        registry
            .resync("test.runs", &snapshot(2))
            .expect("single resync");
        assert_eq!(registry.snapshot().consistent_sequence, 1);
        assert!(matches!(
            registry.apply(&event(3, RuntimeEventKind::RunStarted)),
            Err(ProjectionError::Gap { .. })
        ));
        registry.resync_all(&snapshot(3)).expect("resync all");
        registry
            .apply(&event(4, RuntimeEventKind::RunSettled))
            .expect("next event");
        assert_eq!(registry.snapshot().consistent_sequence, 4);
        assert_eq!(registry.snapshot().values["test.other"].value["events"], 1);
    }
}
