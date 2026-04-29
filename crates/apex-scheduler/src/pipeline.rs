use std::any::TypeId;
use std::marker::PhantomData;
use crate::{Scheduler, SystemId};

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

/// Запись о системе в конвейере.
struct PipelineEntry {
    system_id: SystemId,
    role:      PipelineRole,
    name:      String,
}

/// Строитель конвейера событий.
///
/// Создаётся через `Scheduler::event_pipeline::<E>()`.
/// Зависимости добавляются в Scheduler при вызове `build()`.
pub struct EventPipelineBuilder<E: Send + Sync + 'static> {
    entries:   Vec<PipelineEntry>,
    _phantom:  PhantomData<E>,
}

// ── Ошибки валидации ──────────────────────────────────────────

#[derive(Debug)]
pub enum PipelineValidationError {
    /// Система объявлена как Producer, но не имеет Emit<E>.
    ProducerMissingEmit { system_name: String, event: &'static str },
    /// Система объявлена как Consumer, но не имеет Listen<E>.
    ConsumerMissingListen { system_name: String, event: &'static str },
    /// Система объявлена как Transformer, но не имеет Listen<E> или Emit<E>.
    TransformerIncomplete { system_name: String, event: &'static str, missing: &'static str },
    /// Система не найдена в планировщике.
    SystemNotFound { system_name: String },
}

impl std::fmt::Display for PipelineValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProducerMissingEmit { system_name, event } =>
                write!(f, "Pipeline: система '{}' объявлена как Producer для '{}', \
                           но не имеет Emit<{}>. Добавь в type Events.", system_name, event, event),
            Self::ConsumerMissingListen { system_name, event } =>
                write!(f, "Pipeline: система '{}' объявлена как Consumer для '{}', \
                           но не имеет Listen<{}>. Добавь в type Events.", system_name, event, event),
            Self::TransformerIncomplete { system_name, event, missing } =>
                write!(f, "Pipeline: система '{}' объявлена как Transformer для '{}', \
                           но не имеет {}. Transformer должен иметь оба: Listen<{}> + Emit<{}>.",
                           system_name, event, missing, event, event),
            Self::SystemNotFound { system_name } =>
                write!(f, "Pipeline: система '{}' не найдена в планировщике. \
                           Убедись, что система зарегистрирована до вызова .build().", system_name),
        }
    }
}

impl std::error::Error for PipelineValidationError {}

impl<E: Send + Sync + 'static> EventPipelineBuilder<E> {
    pub(crate) fn new() -> Self {
        Self { entries: Vec::new(), _phantom: PhantomData }
    }

    /// Добавить систему-производитель (только Emit<E>).
    pub fn produced_by(mut self, id: SystemId, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            system_id: id,
            role: PipelineRole::Producer,
            name: name.into(),
        });
        self
    }

    /// Добавить систему-трансформер (Listen<E> + Emit<E>).
    ///
    /// Получает события от всех предыдущих систем,
    /// обрабатывает и перевыпускает событие.
    pub fn transformed_by(mut self, id: SystemId, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            system_id: id,
            role: PipelineRole::Transformer,
            name: name.into(),
        });
        self
    }

    /// Добавить систему-потребитель (только Listen<E>).
    ///
    /// Несколько `consumed_by` подряд образуют параллельную группу:
    /// они все зависят от предыдущей стадии, но не зависят друг от друга.
    pub fn consumed_by(mut self, id: SystemId, name: impl Into<String>) -> Self {
        self.entries.push(PipelineEntry {
            system_id: id,
            role: PipelineRole::Consumer,
            name: name.into(),
        });
        self
    }

    /// Применить конвейер к планировщику — добавить зависимости.
    ///
    /// Правило: каждая стадия зависит от последней не-Consumer стадии
    /// или от предыдущей Consumer-стадии, если они sequential.
    ///
    /// Параллельные Consumer: две Consumer-системы подряд зависят от
    /// одной и той же предыдущей стадии (не друг от друга).
    pub fn build(self, sched: &mut Scheduler) {
        if self.entries.len() < 2 {
            return;
        }

        let mut last_barrier: Option<SystemId> = None;
        let mut prev_consumer: Option<SystemId> = None;

        for entry in &self.entries {
            match entry.role {
                PipelineRole::Producer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(entry.system_id, barrier);
                    }
                    last_barrier = Some(entry.system_id);
                    prev_consumer = None;
                }
                PipelineRole::Transformer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(entry.system_id, barrier);
                    }
                    if let Some(prev) = prev_consumer {
                        sched.add_dependency(entry.system_id, prev);
                    }
                    last_barrier = Some(entry.system_id);
                    prev_consumer = None;
                }
                PipelineRole::Consumer => {
                    if let Some(barrier) = last_barrier {
                        sched.add_dependency(entry.system_id, barrier);
                    }
                    prev_consumer = Some(entry.system_id);
                }
            }
        }
    }

    /// Применить конвейер с предварительной валидацией ролей.
    ///
    /// Проверяет, что AccessDescriptor каждой системы соответствует
    /// заявленной роли. При ошибках возвращает список проблем.
    pub fn build_validated(self, sched: &mut Scheduler) -> Result<(), Vec<PipelineValidationError>> {
        let event_tid = TypeId::of::<E>();
        let event_name = std::any::type_name::<E>();
        let mut errors = Vec::new();

        for entry in &self.entries {
            let access = match sched.system_access(entry.system_id) {
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
                    let has_read  = access.reads_event.iter().any(|(id, _)| *id == event_tid);
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
