use apex_core::events::Events;

// Events — send 10k событий + чтение всех одним читателем (полный round-trip). Не покрыто другими
// бенчами; базовый путь межсистемной коммуникации. Standalone `Events<T>` — тот же уровень
// абстракции, что и bevy `Events<T>` (без World/регистрации). Возвращает сумму (страж честности:
// обе реализации обязаны прочитать все 10k ⇒ sum == 49995000).
#[derive(Clone, Copy)]
pub struct E(pub u64);

pub struct EventsBench;

impl Default for EventsBench {
    fn default() -> Self {
        Self::new()
    }
}

impl EventsBench {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&mut self) -> u64 {
        let mut events = Events::<E>::new();
        let cursor = events.add_reader();
        for i in 0..10_000u64 {
            events.send(E(i));
        }
        events.update(); // pending → читаемо
        let mut sum = 0u64;
        for e in events.read(&cursor).iter() {
            sum += e.0;
        }
        sum
    }
}
