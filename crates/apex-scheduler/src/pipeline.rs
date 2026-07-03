use crate::{Scheduler, SystemId};
use std::any::TypeId;
use std::marker::PhantomData;

/// Запись о системе в конвейере.
struct PipelineEntry {
    name: String,
    role: PipelineRole,
}

/// Роль системы в конвейере — используется для валидации AccessDescriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineRole {
    /// Только отправляет событие — Emit<E> обязателен.
    Producer,
    /// Читает и перевыпускает — Listen<E> + Emit<E> обязательны.
    Transformer,
    /// Только читает — Listen<E> обязателен.
    Consumer,
}

/// Строитель конвейера событий.
///
/// Создаётся через `Scheduler::event_pipeline::<E>()`.
/// Зависимости добавляются в Scheduler при вызове `build()`.
pub struct EventPipelineBuilder<E: Send + Sync + 'static> {
    entries: Vec<PipelineEntry>,
    _phantom: PhantomData<E>,
}

// ── Ошибки валидации ──────────────────────────────────────────

#[derive(Debug)]
pub enum PipelineValidationError {
    /// Система объявлена как Producer, но не имеет Emit<E>.
    ProducerMissingEmit {
        system_name: String,
        event: &'static str,
    },
    /// Система объявлена как Consumer, но не имеет Listen<E>.
    ConsumerMissingListen {
        system_name: String,
        event: &'static str,
    },
    /// Система объявлена как Transformer, но не имеет Listen<E> или Emit<E>.
    TransformerIncomplete {
        system_name: String,
        event: &'static str,
        missing: &'static str,
    },
    /// Система не найдена в планировщике.
    SystemNotFound { system_name: String },
}

impl std::fmt::Display for PipelineValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProducerMissingEmit { system_name, event } => write!(
                f,
                "Pipeline: система '{}' объявлена как Producer для '{}', \
                           но не имеет Emit<{}>. Добавь в type Events.",
                system_name, event, event
            ),
            Self::ConsumerMissingListen { system_name, event } => write!(
                f,
                "Pipeline: система '{}' объявлена как Consumer для '{}', \
                           но не имеет Listen<{}>. Добавь в type Events.",
                system_name, event, event
            ),
            Self::TransformerIncomplete {
                system_name,
                event,
                missing,
            } => write!(
                f,
                "Pipeline: система '{}' объявлена как Transformer для '{}', \
                           но не имеет {}. Transformer должен иметь оба: Listen<{}> + Emit<{}>.",
                system_name, event, missing, event, event
            ),
            Self::SystemNotFound { system_name } => write!(
                f,
                "Pipeline: система '{}' не найдена в планировщике. \
                           Убедись, что система зарегистрирована до вызова .build().",
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

    /// Добавить систему-производитель (только Emit<E>).
    pub fn produced_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Producer,
            name: name.into(),
        });
        self
    }

    /// Добавить систему-трансформер (Listen<E> + Emit<E>).
    pub fn transformed_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Transformer,
            name: name.into(),
        });
        self
    }

    /// Добавить систему-потребитель (только Listen<E>).
    pub fn consumed_by(mut self, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            role: PipelineRole::Consumer,
            name: name.into(),
        });
        self
    }

    /// Применить конвейер к планировщику — добавить зависимости.
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

    /// Применить конвейер с предварительной валидацией ролей.
    ///
    /// Проверяет, что AccessDescriptor каждой системы соответствует
    /// заявленной роли. При ошибках возвращает список проблем.
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
