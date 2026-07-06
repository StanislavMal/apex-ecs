use crate::{Scheduler, SystemId};
use std::any::TypeId;
use std::marker::PhantomData;

/// A record of a system in the pipeline.
struct PipelineEntry {
    name: String,
    role: PipelineRole,
}

/// Role of a system in the pipeline — used for AccessDescriptor validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineRole {
    /// Only emits the event — Emit<E> is required.
    Producer,
    /// Reads and re-emits — Listen<E> + Emit<E> are required.
    Transformer,
    /// Only reads — Listen<E> is required.
    Consumer,
}

/// Event pipeline builder.
///
/// Created via `Scheduler::event_pipeline::<E>()`.
/// Dependencies are added to the Scheduler when `build()` is called.
pub struct EventPipelineBuilder<E: Send + Sync + 'static> {
    entries: Vec<PipelineEntry>,
    _phantom: PhantomData<E>,
}

// ── Validation errors ──────────────────────────────────────────

#[derive(Debug)]
pub enum PipelineValidationError {
    /// The system is declared as Producer but has no Emit<E>.
    ProducerMissingEmit {
        system_name: String,
        event: &'static str,
    },
    /// The system is declared as Consumer but has no Listen<E>.
    ConsumerMissingListen {
        system_name: String,
        event: &'static str,
    },
    /// The system is declared as Transformer but lacks Listen<E> or Emit<E>.
    TransformerIncomplete {
        system_name: String,
        event: &'static str,
        missing: &'static str,
    },
    /// The system was not found in the scheduler.
    SystemNotFound { system_name: String },
}

impl std::fmt::Display for PipelineValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProducerMissingEmit { system_name, event } => write!(
                f,
                "Pipeline: system '{}' is declared as Producer for '{}', \
                           but has no Emit<{}>. Add it to type Events.",
                system_name, event, event
            ),
            Self::ConsumerMissingListen { system_name, event } => write!(
                f,
                "Pipeline: system '{}' is declared as Consumer for '{}', \
                           but has no Listen<{}>. Add it to type Events.",
                system_name, event, event
            ),
            Self::TransformerIncomplete {
                system_name,
                event,
                missing,
            } => write!(
                f,
                "Pipeline: system '{}' is declared as Transformer for '{}', \
                           but lacks {}. A Transformer must have both: Listen<{}> + Emit<{}>.",
                system_name, event, missing, event, event
            ),
            Self::SystemNotFound { system_name } => write!(
                f,
                "Pipeline: system '{}' was not found in the scheduler. \
                           Make sure the system is registered before calling .build().",
                system_name
            ),
        }
    }
}

impl std::error::Error for PipelineValidationError {}

impl<E: Send + Sync + 'static> EventPipelineBuilder<E> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            _phantom: PhantomData,
        }
    }

    /// Add a producer system (Emit<E> only).
    pub fn produced_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Producer,
            name: name.into(),
        });
        self
    }

    /// Add a transformer system (Listen<E> + Emit<E>).
    pub fn transformed_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Transformer,
            name: name.into(),
        });
        self
    }

    /// Add a consumer system (Listen<E> only).
    pub fn consumed_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Consumer,
            name: name.into(),
        });
        self
    }

    /// Apply the pipeline to the scheduler — add the dependencies.
    pub fn build(self, sched: &mut Scheduler) {
        if self.entries.len() < 2 {
            return;
        }
        // §0.2a: a pipeline naming an unregistered system used to panic with an
        // opaque `unwrap` message. Report which system is missing and skip
        // wiring (the ordering constraints are simply not added) rather than
        // aborting the whole app for a config typo. `build_validated` returns a
        // structured `Result` for callers that want to handle it.
        let mut ids: Vec<SystemId> = Vec::with_capacity(self.entries.len());
        for e in &self.entries {
            match sched.find_id_by_name(&e.name) {
                Ok(id) => ids.push(id),
                Err(_) => {
                    log::error!(
                        "Pipeline::build: system '{}' is not registered — pipeline not wired \
                         (register it before .build(), or use build_validated for a checked Result)",
                        e.name,
                    );
                    return;
                }
            }
        }

        let mut last_barrier: Option<SystemId> = None;
        let mut prev_consumer: Option<SystemId> = None;

        for (i, entry) in self.entries.iter().enumerate() {
            let id = ids[i];
            match entry.role {
                PipelineRole::Producer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(id, barrier);
                    }
                    last_barrier = Some(id);
                    prev_consumer = None;
                }
                PipelineRole::Transformer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(id, barrier);
                    }
                    if let Some(prev) = prev_consumer {
                        sched.add_dependency(id, prev);
                    }
                    last_barrier = Some(id);
                    prev_consumer = None;
                }
                PipelineRole::Consumer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(id, barrier);
                    }
                    prev_consumer = Some(id);
                }
            }
        }
    }

    /// Apply the pipeline with prior role validation.
    ///
    /// Checks that each system's AccessDescriptor matches its declared
    /// role. On errors, returns the list of problems.
    pub fn build_validated(
        self,
        sched: &mut Scheduler,
    ) -> Result<(), Vec<PipelineValidationError>> {
        let event_tid = TypeId::of::<E>();
        let event_name = std::any::type_name::<E>();
        let mut errors = Vec::new();

        for entry in &self.entries {
            let id = match sched.find_id_by_name(&entry.name) {
                Ok(id) => id,
                Err(_) => {
                    errors.push(PipelineValidationError::SystemNotFound {
                        system_name: entry.name.clone(),
                    });
                    continue;
                }
            };
            let access = match sched.system_access(id) {
                Some(a) => a,
                None => {
                    errors.push(PipelineValidationError::SystemNotFound {
                        system_name: entry.name.clone(),
                    });
                    continue;
                }
            };

            match entry.role {
                PipelineRole::Producer => {
                    if !access.writes_event.iter().any(|(id, _)| *id == event_tid) {
                        errors.push(PipelineValidationError::ProducerMissingEmit {
                            system_name: entry.name.clone(),
                            event: event_name,
                        });
                    }
                }
                PipelineRole::Consumer => {
                    if !access.reads_event.iter().any(|(id, _)| *id == event_tid) {
                        errors.push(PipelineValidationError::ConsumerMissingListen {
                            system_name: entry.name.clone(),
                            event: event_name,
                        });
                    }
                }
                PipelineRole::Transformer => {
                    let has_read = access.reads_event.iter().any(|(id, _)| *id == event_tid);
                    let has_write = access.writes_event.iter().any(|(id, _)| *id == event_tid);
                    if !has_read {
                        errors.push(PipelineValidationError::TransformerIncomplete {
                            system_name: entry.name.clone(),
                            event: event_name,
                            missing: "Listen<E>",
                        });
                    }
                    if !has_write {
                        errors.push(PipelineValidationError::TransformerIncomplete {
                            system_name: entry.name.clone(),
                            event: event_name,
                            missing: "Emit<E>",
                        });
                    }
                }
            }
        }

        if errors.is_empty() {
            self.build(sched);
            Ok(())
        } else {
            Err(errors)
        }
    }
}
