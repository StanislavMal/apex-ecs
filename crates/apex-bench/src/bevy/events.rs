// Bevy 0.18 переименовал буферные `Events<T>` → `Messages<T>` (а `Event` стал триггер-типом
// наблюдателей). `Messages<T>` — точный аналог apex `Events<T>`: очередь + курсоры-читатели.
use bevy_ecs::message::{Message, Messages};

#[derive(Message)]
struct E(u64);

pub struct Benchmark;

impl Benchmark {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut messages = Messages::<E>::default();
        for i in 0..10_000u64 {
            messages.write(E(i));
        }
        let mut cursor = messages.get_cursor();
        let mut sum = 0u64;
        for e in cursor.read(&messages) {
            sum += e.0;
        }
        sum
    }
}
